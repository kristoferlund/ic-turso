#![allow(clippy::arc_with_non_send_sync)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::array;
use std::cell::UnsafeCell;
use std::collections::HashMap;
use strum::EnumString;
use tracing::{instrument, Level};

use std::fmt::Formatter;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::{
    cell::{Cell, RefCell},
    fmt,
    rc::Rc,
    sync::Arc,
};

use crate::fast_lock::SpinLock;
use crate::io::{File, IO};
use crate::result::LimboResult;
use crate::storage::sqlite3_ondisk::{
    begin_read_wal_frame, begin_read_wal_frame_raw, finish_read_page, prepare_wal_frame,
    WAL_FRAME_HEADER_SIZE, WAL_HEADER_SIZE,
};
use crate::types::IOResult;
use crate::{turso_assert, Buffer, LimboError, Result};
use crate::{Completion, Page};

use self::sqlite3_ondisk::{checksum_wal, PageContent, WAL_MAGIC_BE, WAL_MAGIC_LE};

use super::buffer_pool::BufferPool;
use super::pager::{PageRef, Pager};
use super::sqlite3_ondisk::{self, begin_write_btree_page, WalHeader};

pub const READMARK_NOT_USED: u32 = 0xffffffff;

pub const NO_LOCK: u32 = 0;
pub const SHARED_LOCK: u32 = 1;
pub const WRITE_LOCK: u32 = 2;

#[derive(Debug, Copy, Clone, Default)]
pub struct CheckpointResult {
    /// number of frames in WAL
    pub num_wal_frames: u64,
    /// number of frames moved successfully from WAL to db file after checkpoint
    pub num_checkpointed_frames: u64,
}

impl CheckpointResult {
    pub fn new(n_frames: u64, n_ckpt: u64) -> Self {
        Self {
            num_wal_frames: n_frames,
            num_checkpointed_frames: n_ckpt,
        }
    }
}

#[derive(Debug, Copy, Clone, EnumString)]
#[strum(ascii_case_insensitive)]
pub enum CheckpointMode {
    /// Checkpoint as many frames as possible without waiting for any database readers or writers to finish, then sync the database file if all frames in the log were checkpointed.
    Passive,
    /// This mode blocks until there is no database writer and all readers are reading from the most recent database snapshot. It then checkpoints all frames in the log file and syncs the database file. This mode blocks new database writers while it is pending, but new database readers are allowed to continue unimpeded.
    Full,
    /// This mode works the same way as `Full` with the addition that after checkpointing the log file it blocks (calls the busy-handler callback) until all readers are reading from the database file only. This ensures that the next writer will restart the log file from the beginning. Like `Full`, this mode blocks new database writer attempts while it is pending, but does not impede readers.
    Restart,
    /// This mode works the same way as `Restart` with the addition that it also truncates the log file to zero bytes just prior to a successful return.
    Truncate,
}

#[derive(Debug, Default)]
pub struct LimboRwLock {
    lock: AtomicU32,
    nreads: AtomicU32,
    value: AtomicU32,
}

impl LimboRwLock {
    pub fn new() -> Self {
        Self {
            lock: AtomicU32::new(NO_LOCK),
            nreads: AtomicU32::new(0),
            value: AtomicU32::new(READMARK_NOT_USED),
        }
    }

    /// Shared lock. Returns true if it was successful, false if it couldn't lock it
    pub fn read(&mut self) -> bool {
        let lock = self.lock.load(Ordering::SeqCst);
        let ok = match lock {
            NO_LOCK => {
                let res = self.lock.compare_exchange(
                    lock,
                    SHARED_LOCK,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                );
                let ok = res.is_ok();
                if ok {
                    self.nreads.fetch_add(1, Ordering::SeqCst);
                }
                ok
            }
            SHARED_LOCK => {
                // There is this race condition where we could've unlocked after loading lock ==
                // SHARED_LOCK.
                self.nreads.fetch_add(1, Ordering::SeqCst);
                let lock_after_load = self.lock.load(Ordering::SeqCst);
                if lock_after_load != lock {
                    // try to lock it again
                    let res = self.lock.compare_exchange(
                        lock_after_load,
                        SHARED_LOCK,
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    );
                    let ok = res.is_ok();
                    if ok {
                        // we were able to acquire it back
                        true
                    } else {
                        // we couldn't acquire it back, reduce number again
                        self.nreads.fetch_sub(1, Ordering::SeqCst);
                        false
                    }
                } else {
                    true
                }
            }
            WRITE_LOCK => false,
            _ => unreachable!(),
        };
        tracing::trace!("read_lock({})", ok);
        ok
    }

    /// Locks exclusively. Returns true if it was successful, false if it couldn't lock it
    pub fn write(&mut self) -> bool {
        let lock = self.lock.load(Ordering::SeqCst);
        let ok = match lock {
            NO_LOCK => {
                let res = self.lock.compare_exchange(
                    lock,
                    WRITE_LOCK,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                );
                res.is_ok()
            }
            SHARED_LOCK => {
                // no op
                false
            }
            WRITE_LOCK => false,
            _ => unreachable!(),
        };
        tracing::trace!("write_lock({})", ok);
        ok
    }

    /// Unlock the current held lock.
    pub fn unlock(&mut self) {
        let lock = self.lock.load(Ordering::SeqCst);
        tracing::trace!("unlock(value={})", lock);
        match lock {
            NO_LOCK => {}
            SHARED_LOCK => {
                let prev = self.nreads.fetch_sub(1, Ordering::SeqCst);
                if prev == 1 {
                    let res = self.lock.compare_exchange(
                        lock,
                        NO_LOCK,
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    );
                    assert!(res.is_ok());
                }
            }
            WRITE_LOCK => {
                let res =
                    self.lock
                        .compare_exchange(lock, NO_LOCK, Ordering::SeqCst, Ordering::SeqCst);
                assert!(res.is_ok());
            }
            _ => unreachable!(),
        }
    }
}

/// Write-ahead log (WAL).
pub trait Wal {
    /// Begin a read transaction.
    fn begin_read_tx(&mut self) -> Result<(LimboResult, bool)>;

    /// Begin a write transaction.
    fn begin_write_tx(&mut self) -> Result<LimboResult>;

    /// End a read transaction.
    fn end_read_tx(&self);

    /// End a write transaction.
    fn end_write_tx(&self);

    /// Find the latest frame containing a page.
    fn find_frame(&self, page_id: u64) -> Result<Option<u64>>;

    /// Read a frame from the WAL.
    fn read_frame(&self, frame_id: u64, page: PageRef, buffer_pool: Arc<BufferPool>) -> Result<()>;

    /// Read a raw frame (header included) from the WAL.
    fn read_frame_raw(&self, frame_id: u64, frame: &mut [u8]) -> Result<Arc<Completion>>;

    /// Write a raw frame (header included) from the WAL.
    /// Note, that turso-db will use page_no and size_after fields from the header, but will overwrite checksum with proper value
    fn write_frame_raw(
        &mut self,
        buffer_pool: Arc<BufferPool>,
        frame_id: u64,
        page_id: u64,
        db_size: u64,
        page: &[u8],
    ) -> Result<()>;

    /// Write a frame to the WAL.
    /// db_size is the database size in pages after the transaction finishes.
    /// db_size > 0    -> last frame written in transaction
    /// db_size == 0   -> non-last frame written in transaction
    /// write_counter is the counter we use to track when the I/O operation starts and completes
    fn append_frame(
        &mut self,
        page: PageRef,
        db_size: u32,
        write_counter: Rc<RefCell<usize>>,
    ) -> Result<()>;

    /// Complete append of frames by updating shared wal state. Before this
    /// all changes were stored locally.
    fn finish_append_frames_commit(&mut self) -> Result<()>;

    fn should_checkpoint(&self) -> bool;
    fn checkpoint(
        &mut self,
        pager: &Pager,
        write_counter: Rc<RefCell<usize>>,
        mode: CheckpointMode,
    ) -> Result<IOResult<CheckpointResult>>;
    fn sync(&mut self) -> Result<IOResult<()>>;
    fn get_max_frame_in_wal(&self) -> u64;
    fn get_max_frame(&self) -> u64;
    fn get_min_frame(&self) -> u64;
    fn rollback(&mut self) -> Result<()>;
}

/// A dummy WAL implementation that does nothing.
/// This is used for ephemeral indexes where a WAL is not really
/// needed, and is preferable to passing an Option<dyn Wal> around
/// everywhere.
pub struct DummyWAL;

impl Wal for DummyWAL {
    fn begin_read_tx(&mut self) -> Result<(LimboResult, bool)> {
        Ok((LimboResult::Ok, false))
    }

    fn end_read_tx(&self) {}

    fn begin_write_tx(&mut self) -> Result<LimboResult> {
        Ok(LimboResult::Ok)
    }

    fn end_write_tx(&self) {}

    fn find_frame(&self, _page_id: u64) -> Result<Option<u64>> {
        Ok(None)
    }

    fn read_frame(
        &self,
        _frame_id: u64,
        _page: crate::PageRef,
        _buffer_pool: Arc<BufferPool>,
    ) -> Result<()> {
        Ok(())
    }

    fn read_frame_raw(&self, _frame_id: u64, _frame: &mut [u8]) -> Result<Arc<Completion>> {
        todo!();
    }

    fn write_frame_raw(
        &mut self,
        _buffer_pool: Arc<BufferPool>,
        _frame_id: u64,
        _page_id: u64,
        _db_size: u64,
        _page: &[u8],
    ) -> Result<()> {
        todo!();
    }

    fn append_frame(
        &mut self,
        _page: crate::PageRef,
        _db_size: u32,
        _write_counter: Rc<RefCell<usize>>,
    ) -> Result<()> {
        Ok(())
    }

    fn should_checkpoint(&self) -> bool {
        false
    }

    fn checkpoint(
        &mut self,
        _pager: &Pager,
        _write_counter: Rc<RefCell<usize>>,
        _mode: crate::CheckpointMode,
    ) -> Result<IOResult<CheckpointResult>> {
        Ok(IOResult::Done(CheckpointResult::default()))
    }

    fn sync(&mut self) -> Result<IOResult<()>> {
        Ok(IOResult::Done(()))
    }

    fn get_max_frame_in_wal(&self) -> u64 {
        0
    }

    fn get_max_frame(&self) -> u64 {
        0
    }

    fn get_min_frame(&self) -> u64 {
        0
    }

    fn finish_append_frames_commit(&mut self) -> Result<()> {
        tracing::trace!("finish_append_frames_commit_dumb");
        Ok(())
    }

    fn rollback(&mut self) -> Result<()> {
        Ok(())
    }
}

// Syncing requires a state machine because we need to schedule a sync and then wait until it is
// finished. If we don't wait there will be undefined behaviour that no one wants to debug.
#[derive(Copy, Clone, Debug)]
enum SyncState {
    NotSyncing,
    Syncing,
}

#[derive(Debug, Copy, Clone)]
pub enum CheckpointState {
    Start,
    ReadFrame,
    WaitReadFrame,
    WritePage,
    WaitWritePage,
    Done,
}

// Checkpointing is a state machine that has multiple steps. Since there are multiple steps we save
// in flight information of the checkpoint in OngoingCheckpoint. page is just a helper Page to do
// page operations like reading a frame to a page, and writing a page to disk. This page should not
// be placed back in pager page cache or anything, it's just a helper.
// min_frame and max_frame is the range of frames that can be safely transferred from WAL to db
// file.
// current_page is a helper to iterate through all the pages that might have a frame in the safe
// range. This is inefficient for now.
struct OngoingCheckpoint {
    page: PageRef,
    state: CheckpointState,
    min_frame: u64,
    max_frame: u64,
    current_page: u64,
}

impl fmt::Debug for OngoingCheckpoint {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("OngoingCheckpoint")
            .field("state", &self.state)
            .field("min_frame", &self.min_frame)
            .field("max_frame", &self.max_frame)
            .field("current_page", &self.current_page)
            .finish()
    }
}

#[allow(dead_code)]
pub struct WalFile {
    io: Arc<dyn IO>,
    buffer_pool: Arc<BufferPool>,

    syncing: Rc<Cell<bool>>,
    sync_state: Cell<SyncState>,

    shared: Arc<UnsafeCell<WalFileShared>>,
    ongoing_checkpoint: OngoingCheckpoint,
    checkpoint_threshold: usize,
    // min and max frames for this connection
    /// This is the index to the read_lock in WalFileShared that we are holding. This lock contains
    /// the max frame for this connection.
    max_frame_read_lock_index: usize,
    /// Max frame allowed to lookup range=(minframe..max_frame)
    max_frame: u64,
    /// Start of range to look for frames range=(minframe..max_frame)
    min_frame: u64,
    /// Check of last frame in WAL, this is a cumulative checksum over all frames in the WAL
    last_checksum: (u32, u32),

    /// Hack for now in case of rollback, will not be needed once we remove this bullshit frame cache.
    start_pages_in_frames: usize,

    /// Count of possible pages to checkpoint, and number of backfilled
    prev_checkpoint: CheckpointResult,

    /// Private copy of WalHeader
    pub header: WalHeader,
}

impl fmt::Debug for WalFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WalFile")
            .field("syncing", &self.syncing.get())
            .field("sync_state", &self.sync_state)
            .field("page_size", &self.page_size())
            .field("shared", &self.shared)
            .field("ongoing_checkpoint", &self.ongoing_checkpoint)
            .field("checkpoint_threshold", &self.checkpoint_threshold)
            .field("max_frame_read_lock_index", &self.max_frame_read_lock_index)
            .field("max_frame", &self.max_frame)
            .field("min_frame", &self.min_frame)
            // Excluding other fields
            .finish()
    }
}

// TODO(pere): lock only important parts + pin WalFileShared
/// WalFileShared is the part of a WAL that will be shared between threads. A wal has information
/// that needs to be communicated between threads so this struct does the job.
#[allow(dead_code)]
pub struct WalFileShared {
    pub wal_header: Arc<SpinLock<WalHeader>>,
    pub min_frame: AtomicU64,
    pub max_frame: AtomicU64,
    pub nbackfills: AtomicU64,
    // Frame cache maps a Page to all the frames it has stored in WAL in ascending order.
    // This is to easily find the frame it must checkpoint each connection if a checkpoint is
    // necessary.
    // One difference between SQLite and limbo is that we will never support multi process, meaning
    // we don't need WAL's index file. So we can do stuff like this without shared memory.
    // TODO: this will need refactoring because this is incredible memory inefficient.
    pub frame_cache: Arc<SpinLock<HashMap<u64, Vec<u64>>>>,
    // Another memory inefficient array made to just keep track of pages that are in frame_cache.
    pub pages_in_frames: Arc<SpinLock<Vec<u64>>>,
    pub last_checksum: (u32, u32), // Check of last frame in WAL, this is a cumulative checksum over all frames in the WAL
    pub file: Arc<dyn File>,
    /// read_locks is a list of read locks that can coexist with the max_frame number stored in
    /// value. There is a limited amount because and unbounded amount of connections could be
    /// fatal. Therefore, for now we copy how SQLite behaves with limited amounts of read max
    /// frames that is equal to 5
    pub read_locks: [LimboRwLock; 5],
    /// There is only one write allowed in WAL mode. This lock takes care of ensuring there is only
    /// one used.
    pub write_lock: LimboRwLock,
    pub checkpoint_lock: LimboRwLock,
    pub loaded: AtomicBool,
}

impl fmt::Debug for WalFileShared {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WalFileShared")
            .field("wal_header", &self.wal_header)
            .field("min_frame", &self.min_frame)
            .field("max_frame", &self.max_frame)
            .field("nbackfills", &self.nbackfills)
            .field("frame_cache", &self.frame_cache)
            .field("pages_in_frames", &self.pages_in_frames)
            .field("last_checksum", &self.last_checksum)
            // Excluding `file`, `read_locks`, and `write_lock`
            .finish()
    }
}

impl Wal for WalFile {
    /// Begin a read transaction.
    #[instrument(skip_all, level = Level::DEBUG)]
    fn begin_read_tx(&mut self) -> Result<(LimboResult, bool)> {
        let max_frame_in_wal = self.get_shared().max_frame.load(Ordering::SeqCst);

        let db_has_changed = max_frame_in_wal > self.max_frame;

        let mut max_read_mark = 0;
        let mut max_read_mark_index = -1;
        // Find the largest mark we can find, ignore frames that are impossible to be in range and
        // that are not set
        for (index, lock) in self.get_shared().read_locks.iter().enumerate() {
            let this_mark = lock.value.load(Ordering::SeqCst);
            if this_mark > max_read_mark && this_mark <= max_frame_in_wal as u32 {
                max_read_mark = this_mark;
                max_read_mark_index = index as i64;
            }
        }

        // If we didn't find any mark or we can update, let's update them
        if (max_read_mark as u64) < max_frame_in_wal || max_read_mark_index == -1 {
            for (index, lock) in self.get_shared().read_locks.iter_mut().enumerate() {
                let busy = !lock.write();
                if !busy {
                    // If this was busy then it must mean >1 threads tried to set this read lock
                    lock.value.store(max_frame_in_wal as u32, Ordering::SeqCst);
                    max_read_mark = max_frame_in_wal as u32;
                    max_read_mark_index = index as i64;
                    lock.unlock();
                    break;
                }
            }
        }

        if max_read_mark_index == -1 {
            return Ok((LimboResult::Busy, db_has_changed));
        }

        let (min_frame, last_checksum, start_pages_in_frames) = {
            let shared = self.get_shared();
            let lock = &mut shared.read_locks[max_read_mark_index as usize];
            tracing::trace!("begin_read_tx_read_lock(lock={})", max_read_mark_index);
            let busy = !lock.read();
            if busy {
                return Ok((LimboResult::Busy, db_has_changed));
            }
            (
                shared.nbackfills.load(Ordering::SeqCst) + 1,
                shared.last_checksum,
                shared.pages_in_frames.lock().len(),
            )
        };
        self.min_frame = min_frame;
        self.max_frame_read_lock_index = max_read_mark_index as usize;
        self.max_frame = max_read_mark as u64;
        self.last_checksum = last_checksum;
        self.start_pages_in_frames = start_pages_in_frames;
        tracing::debug!(
            "begin_read_tx(min_frame={}, max_frame={}, lock={}, max_frame_in_wal={})",
            self.min_frame,
            self.max_frame,
            self.max_frame_read_lock_index,
            max_frame_in_wal
        );
        Ok((LimboResult::Ok, db_has_changed))
    }

    /// End a read transaction.
    #[inline(always)]
    #[instrument(skip_all, level = Level::DEBUG)]
    fn end_read_tx(&self) {
        tracing::debug!("end_read_tx(lock={})", self.max_frame_read_lock_index);
        let read_lock = &mut self.get_shared().read_locks[self.max_frame_read_lock_index];
        read_lock.unlock();
    }

    /// Begin a write transaction
    #[instrument(skip_all, level = Level::DEBUG)]
    fn begin_write_tx(&mut self) -> Result<LimboResult> {
        let busy = !self.get_shared().write_lock.write();
        tracing::debug!("begin_write_transaction(busy={})", busy);
        if busy {
            return Ok(LimboResult::Busy);
        }
        // If the max frame is not the same as the one in the shared state, it means another
        // transaction wrote to the WAL after we started our read transaction. This means our
        // snapshot is not consistent with the one in the shared state and we need to start another
        // one.
        let shared = self.get_shared();
        if self.max_frame != shared.max_frame.load(Ordering::SeqCst) {
            shared.write_lock.unlock();
            return Ok(LimboResult::Busy);
        }
        Ok(LimboResult::Ok)
    }

    /// End a write transaction
    #[instrument(skip_all, level = Level::DEBUG)]
    fn end_write_tx(&self) {
        tracing::debug!("end_write_txn");
        self.get_shared().write_lock.unlock();
    }

    /// Find the latest frame containing a page.
    #[instrument(skip_all, level = Level::DEBUG)]
    fn find_frame(&self, page_id: u64) -> Result<Option<u64>> {
        let shared = self.get_shared();
        let frames = shared.frame_cache.lock();
        let frames = frames.get(&page_id);
        if frames.is_none() {
            return Ok(None);
        }
        let frames = frames.unwrap();
        for frame in frames.iter().rev() {
            if *frame <= self.max_frame {
                return Ok(Some(*frame));
            }
        }
        Ok(None)
    }

    /// Read a frame from the WAL.
    #[instrument(skip_all, level = Level::DEBUG)]
    fn read_frame(&self, frame_id: u64, page: PageRef, buffer_pool: Arc<BufferPool>) -> Result<()> {
        tracing::debug!("read_frame({})", frame_id);
        let offset = self.frame_offset(frame_id);
        page.set_locked();
        let frame = page.clone();
        let complete = Box::new(move |buf: Arc<RefCell<Buffer>>, bytes_read: i32| {
            let buf_len = buf.borrow().len();
            turso_assert!(
                bytes_read == buf_len as i32,
                "read({bytes_read}) less than expected({buf_len})"
            );
            let frame = frame.clone();
            finish_read_page(page.get().id, buf, frame).unwrap();
        });
        begin_read_wal_frame(
            &self.get_shared().file,
            offset + WAL_FRAME_HEADER_SIZE,
            buffer_pool,
            complete,
        )?;
        Ok(())
    }

    #[instrument(skip_all, level = Level::DEBUG)]
    fn read_frame_raw(&self, frame_id: u64, frame: &mut [u8]) -> Result<Arc<Completion>> {
        tracing::debug!("read_frame({})", frame_id);
        let offset = self.frame_offset(frame_id);
        let (frame_ptr, frame_len) = (frame.as_mut_ptr(), frame.len());
        let complete = Box::new(move |buf: Arc<RefCell<Buffer>>, bytes_read: i32| {
            let buf = buf.borrow();
            let buf_len = buf.len();
            turso_assert!(
                bytes_read == buf_len as i32,
                "read({bytes_read}) != expected({buf_len})"
            );
            let buf_ptr = buf.as_ptr();
            unsafe {
                std::ptr::copy_nonoverlapping(buf_ptr, frame_ptr, frame_len);
            }
        });
        let c =
            begin_read_wal_frame_raw(&self.get_shared().file, offset, self.page_size(), complete)?;
        Ok(c)
    }

    #[instrument(skip_all, level = Level::DEBUG)]
    fn write_frame_raw(
        &mut self,
        buffer_pool: Arc<BufferPool>,
        frame_id: u64,
        page_id: u64,
        db_size: u64,
        page: &[u8],
    ) -> Result<()> {
        tracing::debug!("write_raw_frame({})", frame_id);
        if page.len() != self.page_size() as usize {
            return Err(LimboError::InvalidArgument(format!(
                "unexpected page size in frame: got={}, expected={}",
                page.len(),
                self.page_size(),
            )));
        }
        if frame_id > self.max_frame + 1 {
            // attempt to write frame out of sequential order - error out
            return Err(LimboError::InvalidArgument(format!(
                "frame_id is beyond next frame in the WAL: frame_id={}, max_frame={}",
                frame_id, self.max_frame
            )));
        }
        if frame_id <= self.max_frame {
            // just validate if page content from the frame matches frame in the WAL
            let offset = self.frame_offset(frame_id);
            let conflict = Arc::new(Cell::new(false));
            let (page_ptr, page_len) = (page.as_ptr(), page.len());
            let complete = Box::new({
                let conflict = conflict.clone();
                move |buf: Arc<RefCell<Buffer>>, bytes_read: i32| {
                    let buf = buf.borrow();
                    let buf_len = buf.len();
                    turso_assert!(
                        bytes_read == buf_len as i32,
                        "read({bytes_read}) != expected({buf_len})"
                    );
                    let page = unsafe { std::slice::from_raw_parts(page_ptr, page_len) };
                    if buf.as_slice() != page {
                        conflict.set(true);
                    }
                }
            });
            let c = begin_read_wal_frame(
                &self.get_shared().file,
                offset + WAL_FRAME_HEADER_SIZE,
                buffer_pool,
                complete,
            )?;
            self.io.wait_for_completion(c)?;
            return if conflict.get() {
                Err(LimboError::Conflict(format!(
                    "frame content differs from the WAL: frame_id={frame_id}"
                )))
            } else {
                Ok(())
            };
        }

        // perform actual write
        let offset = self.frame_offset(frame_id);
        let shared = self.get_shared();
        let header = shared.wal_header.clone();
        let header = header.lock();
        let checksums = self.last_checksum;
        let (checksums, frame_bytes) = prepare_wal_frame(
            &header,
            checksums,
            header.page_size,
            page_id as u32,
            db_size as u32,
            page,
        );
        let c = Arc::new(Completion::new_write(|_| {}));
        let c = shared.file.pwrite(offset, frame_bytes, c)?;
        self.io.wait_for_completion(c)?;
        self.complete_append_frame(page_id, frame_id, checksums);
        if db_size > 0 {
            self.finish_append_frames_commit()?;
        }
        Ok(())
    }

    /// Write a frame to the WAL.
    #[instrument(skip_all, level = Level::DEBUG)]
    fn append_frame(
        &mut self,
        page: PageRef,
        db_size: u32,
        write_counter: Rc<RefCell<usize>>,
    ) -> Result<()> {
        let page_id = page.get().id;
        let frame_id = self.max_frame + 1;
        let offset = self.frame_offset(frame_id);
        tracing::debug!(frame_id, offset, page_id);
        let checksums = {
            let shared = self.get_shared();
            let header = shared.wal_header.clone();
            let header = header.lock();
            let checksums = self.last_checksum;
            let page_content = page.get_contents();
            let page_buf = page_content.as_ptr();
            let (frame_checksums, frame_bytes) = prepare_wal_frame(
                &header,
                checksums,
                header.page_size,
                page_id as u32,
                db_size,
                page_buf,
            );

            *write_counter.borrow_mut() += 1;
            let c = Completion::new_write({
                let frame_bytes = frame_bytes.clone();
                let write_counter = write_counter.clone();
                move |bytes_written| {
                    let frame_len = frame_bytes.borrow().len();
                    turso_assert!(
                        bytes_written == frame_len as i32,
                        "wrote({bytes_written}) != expected({frame_len})"
                    );

                    page.clear_dirty();
                    *write_counter.borrow_mut() -= 1;
                }
            });
            let result = shared.file.pwrite(offset, frame_bytes.clone(), c.into());
            if let Err(err) = result {
                *write_counter.borrow_mut() -= 1;
                return Err(err);
            }
            frame_checksums
        };
        self.complete_append_frame(page_id as u64, frame_id, checksums);
        Ok(())
    }

    #[instrument(skip_all, level = Level::DEBUG)]
    fn should_checkpoint(&self) -> bool {
        let shared = self.get_shared();
        let frame_id = shared.max_frame.load(Ordering::SeqCst) as usize;
        let nbackfills = shared.nbackfills.load(Ordering::SeqCst) as usize;
        frame_id > self.checkpoint_threshold + nbackfills
    }

    #[instrument(skip_all, level = Level::DEBUG)]
    fn checkpoint(
        &mut self,
        pager: &Pager,
        write_counter: Rc<RefCell<usize>>,
        mode: CheckpointMode,
    ) -> Result<IOResult<CheckpointResult>> {
        assert!(
            matches!(mode, CheckpointMode::Passive),
            "only passive mode supported for now"
        );
        'checkpoint_loop: loop {
            let state = self.ongoing_checkpoint.state;
            tracing::debug!(?state);
            match state {
                CheckpointState::Start => {
                    // TODO(pere): check what frames are safe to checkpoint between many readers!
                    let (shared_max, nbackfills) = {
                        let shared = self.get_shared();
                        (
                            shared.max_frame.load(Ordering::SeqCst),
                            shared.nbackfills.load(Ordering::SeqCst),
                        )
                    };
                    if shared_max <= nbackfills {
                        // if there's nothing to do and we are fully back-filled, to match sqlite
                        // we return the previous number of backfilled pages from last checkpoint.
                        return Ok(IOResult::Done(self.prev_checkpoint));
                    }
                    self.ongoing_checkpoint.min_frame = nbackfills + 1;
                    let shared = self.get_shared();
                    let busy = !shared.checkpoint_lock.write();
                    if busy {
                        return Err(LimboError::Busy);
                    }
                    let mut max_safe_frame = shared.max_frame.load(Ordering::SeqCst);
                    for (read_lock_idx, read_lock) in shared.read_locks.iter_mut().enumerate() {
                        let this_mark = read_lock.value.load(Ordering::SeqCst);
                        if this_mark < max_safe_frame as u32 {
                            let busy = !read_lock.write();
                            if !busy {
                                let new_mark = if read_lock_idx == 0 {
                                    max_safe_frame as u32
                                } else {
                                    READMARK_NOT_USED
                                };
                                read_lock.value.store(new_mark, Ordering::SeqCst);
                                read_lock.unlock();
                            } else {
                                max_safe_frame = this_mark as u64;
                            }
                        }
                    }
                    self.ongoing_checkpoint.max_frame = max_safe_frame;
                    self.ongoing_checkpoint.current_page = 0;
                    self.ongoing_checkpoint.state = CheckpointState::ReadFrame;
                    tracing::trace!(
                        "checkpoint_start(min_frame={}, max_frame={})",
                        self.ongoing_checkpoint.min_frame,
                        self.ongoing_checkpoint.max_frame,
                    );
                }
                CheckpointState::ReadFrame => {
                    let shared = self.get_shared();
                    let min_frame = self.ongoing_checkpoint.min_frame;
                    let max_frame = self.ongoing_checkpoint.max_frame;
                    let pages_in_frames = shared.pages_in_frames.clone();
                    let pages_in_frames = pages_in_frames.lock();

                    let frame_cache = shared.frame_cache.clone();
                    let frame_cache = frame_cache.lock();
                    assert!(self.ongoing_checkpoint.current_page as usize <= pages_in_frames.len());
                    if self.ongoing_checkpoint.current_page as usize == pages_in_frames.len() {
                        self.ongoing_checkpoint.state = CheckpointState::Done;
                        continue 'checkpoint_loop;
                    }
                    let page = pages_in_frames[self.ongoing_checkpoint.current_page as usize];
                    let frames = frame_cache
                        .get(&page)
                        .expect("page must be in frame cache if it's in list");

                    for frame in frames.iter().rev() {
                        if *frame >= min_frame && *frame <= max_frame {
                            tracing::debug!(
                                "checkpoint page(state={:?}, page={}, frame={})",
                                state,
                                page,
                                *frame
                            );
                            self.ongoing_checkpoint.page.get().id = page as usize;

                            self.read_frame(
                                *frame,
                                self.ongoing_checkpoint.page.clone(),
                                self.buffer_pool.clone(),
                            )?;
                            self.ongoing_checkpoint.state = CheckpointState::WaitReadFrame;
                            continue 'checkpoint_loop;
                        }
                    }
                    self.ongoing_checkpoint.current_page += 1;
                }
                CheckpointState::WaitReadFrame => {
                    if self.ongoing_checkpoint.page.is_locked() {
                        return Ok(IOResult::IO);
                    } else {
                        self.ongoing_checkpoint.state = CheckpointState::WritePage;
                    }
                }
                CheckpointState::WritePage => {
                    self.ongoing_checkpoint.page.set_dirty();
                    begin_write_btree_page(
                        pager,
                        &self.ongoing_checkpoint.page,
                        write_counter.clone(),
                    )?;
                    self.ongoing_checkpoint.state = CheckpointState::WaitWritePage;
                }
                CheckpointState::WaitWritePage => {
                    if *write_counter.borrow() > 0 {
                        return Ok(IOResult::IO);
                    }
                    // If page was in cache clear it.
                    if let Some(page) = pager.cache_get(self.ongoing_checkpoint.page.get().id) {
                        page.clear_dirty();
                    }
                    self.ongoing_checkpoint.page.clear_dirty();
                    let shared = self.get_shared();
                    if (self.ongoing_checkpoint.current_page as usize)
                        < shared.pages_in_frames.lock().len()
                    {
                        self.ongoing_checkpoint.current_page += 1;
                        self.ongoing_checkpoint.state = CheckpointState::ReadFrame;
                    } else {
                        self.ongoing_checkpoint.state = CheckpointState::Done;
                    }
                }
                CheckpointState::Done => {
                    if *write_counter.borrow() > 0 {
                        return Ok(IOResult::IO);
                    }
                    let shared = self.get_shared();
                    shared.checkpoint_lock.unlock();
                    let max_frame = shared.max_frame.load(Ordering::SeqCst);
                    let nbackfills = shared.nbackfills.load(Ordering::SeqCst);

                    // Record two num pages fields to return as checkpoint result to caller.
                    // Ref: pnLog, pnCkpt on https://www.sqlite.org/c3ref/wal_checkpoint_v2.html
                    let frames_in_wal = max_frame.saturating_sub(nbackfills);
                    let frames_checkpointed = self
                        .ongoing_checkpoint
                        .max_frame
                        .saturating_sub(self.ongoing_checkpoint.min_frame - 1);
                    let checkpoint_result =
                        CheckpointResult::new(frames_in_wal, frames_checkpointed);
                    let everything_backfilled = shared.max_frame.load(Ordering::SeqCst)
                        == self.ongoing_checkpoint.max_frame;
                    shared
                        .nbackfills
                        .store(self.ongoing_checkpoint.max_frame, Ordering::SeqCst);
                    if everything_backfilled {
                        // TODO: Even in Passive mode, if everything was backfilled we should
                        // truncate and fsync the *db file*

                        // To properly reset the *wal file* we will need restart and/or truncate mode.
                        // Currently, it will grow the WAL file indefinetly, but don't resetting is better than breaking.
                        // Check: https://github.com/sqlite/sqlite/blob/2bd9f69d40dd240c4122c6d02f1ff447e7b5c098/src/wal.c#L2193
                        if !matches!(mode, CheckpointMode::Passive) {
                            // Here we know that we backfilled everything, therefore we can safely
                            // reset the wal.
                            shared.frame_cache.lock().clear();
                            shared.pages_in_frames.lock().clear();
                            shared.max_frame.store(0, Ordering::SeqCst);
                            shared.nbackfills.store(0, Ordering::SeqCst);
                            // TODO: if all frames were backfilled into the db file, calls fsync
                            // TODO(pere): truncate wal file here.
                        }
                    }
                    self.prev_checkpoint = checkpoint_result;
                    self.ongoing_checkpoint.state = CheckpointState::Start;
                    return Ok(IOResult::Done(checkpoint_result));
                }
            }
        }
    }

    #[instrument(err, skip_all, level = Level::DEBUG)]
    fn sync(&mut self) -> Result<IOResult<()>> {
        match self.sync_state.get() {
            SyncState::NotSyncing => {
                tracing::debug!("wal_sync");
                let syncing = self.syncing.clone();
                self.syncing.set(true);
                let completion = Completion::new_sync(move |_| {
                    tracing::debug!("wal_sync finish");
                    syncing.set(false);
                });
                let shared = self.get_shared();
                shared.file.sync(completion.into())?;
                self.sync_state.set(SyncState::Syncing);
                Ok(IOResult::IO)
            }
            SyncState::Syncing => {
                if self.syncing.get() {
                    tracing::debug!("wal_sync is already syncing");
                    Ok(IOResult::IO)
                } else {
                    self.sync_state.set(SyncState::NotSyncing);
                    Ok(IOResult::Done(()))
                }
            }
        }
    }

    fn get_max_frame_in_wal(&self) -> u64 {
        self.get_shared().max_frame.load(Ordering::SeqCst)
    }

    fn get_max_frame(&self) -> u64 {
        self.max_frame
    }

    fn get_min_frame(&self) -> u64 {
        self.min_frame
    }

    #[instrument(err, skip_all, level = Level::DEBUG)]
    fn rollback(&mut self) -> Result<()> {
        // TODO(pere): have to remove things from frame_cache because they are no longer valid.
        // TODO(pere): clear page cache in pager.
        {
            // TODO(pere): implement proper hashmap, this sucks :).
            let shared = self.get_shared();
            let max_frame = shared.max_frame.load(Ordering::SeqCst);
            tracing::debug!(to_max_frame = max_frame);
            {
                let mut frame_cache = shared.frame_cache.lock();
                for (_, frames) in frame_cache.iter_mut() {
                    let mut last_valid_frame = frames.len();
                    for frame in frames.iter().rev() {
                        if *frame <= max_frame {
                            break;
                        }
                        last_valid_frame -= 1;
                    }
                    frames.truncate(last_valid_frame);
                }
                let mut pages_in_frames = shared.pages_in_frames.lock();
                pages_in_frames.truncate(self.start_pages_in_frames);
            }
            self.last_checksum = shared.last_checksum;
        }
        self.reset_internal_states();
        Ok(())
    }

    #[instrument(skip_all, level = Level::DEBUG)]
    fn finish_append_frames_commit(&mut self) -> Result<()> {
        let shared = self.get_shared();
        shared.max_frame.store(self.max_frame, Ordering::SeqCst);
        tracing::trace!(self.max_frame, ?self.last_checksum);
        shared.last_checksum = self.last_checksum;
        Ok(())
    }
}

impl WalFile {
    pub fn new(
        io: Arc<dyn IO>,
        shared: Arc<UnsafeCell<WalFileShared>>,
        buffer_pool: Arc<BufferPool>,
    ) -> Self {
        let checkpoint_page = Arc::new(Page::new(0));
        let buffer = buffer_pool.get();
        {
            let buffer_pool = buffer_pool.clone();
            let drop_fn = Rc::new(move |buf| {
                buffer_pool.put(buf);
            });
            checkpoint_page.get().contents = Some(PageContent::new(
                0,
                Arc::new(RefCell::new(Buffer::new(buffer, drop_fn))),
            ));
        }

        let header = unsafe { shared.get().as_mut().unwrap().wal_header.lock() };
        let last_checksum = unsafe { (*shared.get()).last_checksum };
        Self {
            io,
            // default to max frame in WAL, so that when we read schema we can read from WAL too if it's there.
            max_frame: unsafe { (*shared.get()).max_frame.load(Ordering::SeqCst) },
            shared,
            ongoing_checkpoint: OngoingCheckpoint {
                page: checkpoint_page,
                state: CheckpointState::Start,
                min_frame: 0,
                max_frame: 0,
                current_page: 0,
            },
            checkpoint_threshold: 1000,
            buffer_pool,
            syncing: Rc::new(Cell::new(false)),
            sync_state: Cell::new(SyncState::NotSyncing),
            min_frame: 0,
            max_frame_read_lock_index: 0,
            last_checksum,
            prev_checkpoint: CheckpointResult::default(),
            start_pages_in_frames: 0,
            header: *header,
        }
    }

    fn page_size(&self) -> u32 {
        self.get_shared().wal_header.lock().page_size
    }

    fn frame_offset(&self, frame_id: u64) -> usize {
        assert!(frame_id > 0, "Frame ID must be 1-based");
        let page_offset = (frame_id - 1) * (self.page_size() + WAL_FRAME_HEADER_SIZE as u32) as u64;
        let offset = WAL_HEADER_SIZE as u64 + page_offset;
        offset as usize
    }

    #[allow(clippy::mut_from_ref)]
    fn get_shared(&self) -> &mut WalFileShared {
        unsafe { self.shared.get().as_mut().unwrap() }
    }

    fn complete_append_frame(&mut self, page_id: u64, frame_id: u64, checksums: (u32, u32)) {
        self.last_checksum = checksums;
        self.max_frame = frame_id;
        let shared = self.get_shared();
        {
            let mut frame_cache = shared.frame_cache.lock();
            let frames = frame_cache.get_mut(&page_id);
            match frames {
                Some(frames) => frames.push(frame_id),
                None => {
                    frame_cache.insert(page_id, vec![frame_id]);
                    shared.pages_in_frames.lock().push(page_id);
                }
            }
        }
    }

    fn reset_internal_states(&mut self) {
        self.ongoing_checkpoint.state = CheckpointState::Start;
        self.ongoing_checkpoint.min_frame = 0;
        self.ongoing_checkpoint.max_frame = 0;
        self.ongoing_checkpoint.current_page = 0;
        self.sync_state.set(SyncState::NotSyncing);
        self.syncing.set(false);
    }
}

impl WalFileShared {
    pub fn open_shared_if_exists(
        io: &Arc<dyn IO>,
        path: &str,
    ) -> Result<Option<Arc<UnsafeCell<WalFileShared>>>> {
        let file = io.open_file(path, crate::io::OpenFlags::Create, false)?;
        if file.size()? > 0 {
            let wal_file_shared = sqlite3_ondisk::read_entire_wal_dumb(&file)?;
            // TODO: Return a completion instead.
            let mut max_loops = 100_000;
            while !unsafe { &*wal_file_shared.get() }
                .loaded
                .load(Ordering::SeqCst)
            {
                io.run_once()?;
                max_loops -= 1;
                if max_loops == 0 {
                    panic!("WAL file not loaded");
                }
            }
            Ok(Some(wal_file_shared))
        } else {
            Ok(None)
        }
    }

    pub fn new_shared(
        page_size: u32,
        io: &Arc<dyn IO>,
        file: Arc<dyn File>,
    ) -> Result<Arc<UnsafeCell<WalFileShared>>> {
        let magic = if cfg!(target_endian = "big") {
            WAL_MAGIC_BE
        } else {
            WAL_MAGIC_LE
        };
        let mut wal_header = WalHeader {
            magic,
            file_format: 3007000,
            page_size,
            checkpoint_seq: 0, // TODO implement sequence number
            salt_1: io.generate_random_number() as u32,
            salt_2: io.generate_random_number() as u32,
            checksum_1: 0,
            checksum_2: 0,
        };
        let native = cfg!(target_endian = "big"); // if target_endian is
                                                  // already big then we don't care but if isn't, header hasn't yet been
                                                  // encoded to big endian, therefore we want to swap bytes to compute this
                                                  // checksum.
        let checksums = (0, 0);
        let checksums = checksum_wal(
            &wal_header.as_bytes()[..WAL_HEADER_SIZE - 2 * 4], // first 24 bytes
            &wal_header,
            checksums,
            native, // this is false because we haven't encoded the wal header yet
        );
        wal_header.checksum_1 = checksums.0;
        wal_header.checksum_2 = checksums.1;
        sqlite3_ondisk::begin_write_wal_header(&file, &wal_header)?;
        let header = Arc::new(SpinLock::new(wal_header));
        let checksum = {
            let checksum = header.lock();
            (checksum.checksum_1, checksum.checksum_2)
        };
        tracing::debug!("new_shared(header={:?})", header);
        let shared = WalFileShared {
            wal_header: header,
            min_frame: AtomicU64::new(0),
            max_frame: AtomicU64::new(0),
            nbackfills: AtomicU64::new(0),
            frame_cache: Arc::new(SpinLock::new(HashMap::new())),
            last_checksum: checksum,
            file,
            pages_in_frames: Arc::new(SpinLock::new(Vec::new())),
            read_locks: array::from_fn(|_| LimboRwLock {
                lock: AtomicU32::new(NO_LOCK),
                nreads: AtomicU32::new(0),
                value: AtomicU32::new(READMARK_NOT_USED),
            }),
            write_lock: LimboRwLock {
                lock: AtomicU32::new(NO_LOCK),
                nreads: AtomicU32::new(0),
                value: AtomicU32::new(READMARK_NOT_USED),
            },
            checkpoint_lock: LimboRwLock::new(),
            loaded: AtomicBool::new(true),
        };
        Ok(Arc::new(UnsafeCell::new(shared)))
    }

    pub fn page_size(&self) -> u32 {
        self.wal_header.lock().page_size
    }
}
