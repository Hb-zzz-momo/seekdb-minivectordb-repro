//! Append-only commit log, which is also the table store.
//!
//! Every committed transaction becomes exactly one frame:
//!
//! ```text
//! frame   = [payload_len u32][crc32c(payload) u32][payload]
//! payload = [lsn u64][op_count varint][op...]
//! ```
//!
//! A frame is either completely valid after a crash or detected as torn by
//! its length or checksum, so transactions are all-or-nothing without a
//! separate undo log. A frame too large to buffer is written in pieces
//! behind a header that is still zero and so reads as a torn tail; the
//! header goes in last (`FrameWriter`). Recovery replays valid frames in order and truncates
//! the active segment at the first invalid one; running it again on the
//! truncated file finds the same frames, so an interrupted recovery is safe.
//!
//! Segments are `seg-<id>.log` files; only the highest-numbered one is
//! appended to. Row locations handed out by `append` point straight at the
//! row bytes inside a frame so point reads need one `pread`.

use std::cell::Cell;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read};
use std::rc::Rc;
use std::os::unix::fs::FileExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use super::catalog::TableDef;
use super::codec::{self, Reader};
use super::crc::{crc32c, Crc32c};
use crate::error::DbResult;
use crate::{bail, db_err};

const FRAME_HEADER: u64 = 8;
const PAGE: u64 = 4096;
const SCAN_BUFFER: usize = 256 << 10;
/// A frame under construction is made durable and dropped from the page
/// cache each time this much of it has been written, so that a very large
/// frame builds up neither dirty pages nor cache.
const SYNC_EVERY: u64 = 16 << 20;
/// Largest buffer kept from one frame for the next.
const SPARE_BYTES: usize = 4 << 20;

const OP_CREATE_TABLE: u8 = 1;
const OP_DROP_TABLE: u8 = 2;
const OP_CREATE_INDEX: u8 = 3;
const OP_PUT: u8 = 4;
const OP_DELETE: u8 = 5;

/// Where a row body lives on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowLoc {
    pub seg: u32,
    pub off: u64,
    pub len: u32,
}

/// A logged change, as written by `append`.
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    CreateTable(TableDef),
    DropTable { table: u32 },
    CreateIndex { table: u32, name: String },
    Put { table: u32, pk: i64, row: Vec<u8> },
    Delete { table: u32, pk: i64 },
}

/// A logged change as read back; row bodies borrow the read buffer.
#[derive(Debug, PartialEq)]
pub enum OpView<'a> {
    CreateTable(TableDef),
    DropTable { table: u32 },
    CreateIndex { table: u32, name: String },
    Put { table: u32, pk: i64, row: &'a [u8], loc: RowLoc },
    Delete { table: u32, pk: i64 },
}

#[derive(Debug, Clone)]
pub struct LogOptions {
    /// Start a new segment once the active one reaches this size.
    pub segment_bytes: u64,
    /// `fdatasync` every commit. Only tests turn this off.
    pub sync: bool,
    /// Drop log pages from the page cache once they are durable or replayed.
    /// Off only to measure what dropping saves.
    pub drop_cache: bool,
    /// Bytes of a frame under construction that are buffered before they
    /// are written out. Only tests make it small.
    pub chunk_bytes: usize,
    /// Frames with a larger payload are replayed without being held in
    /// memory whole. Only tests make it small.
    pub big_frame_bytes: u64,
}

impl Default for LogOptions {
    fn default() -> Self {
        LogOptions { segment_bytes: 64 << 20, sync: true, drop_cache: true, chunk_bytes: 1 << 20, big_frame_bytes: 4 << 20 }
    }
}

struct Segment {
    id: u32,
    file: File,
    len: u64,
}

pub struct LogStore {
    dir: PathBuf,
    opts: LogOptions,
    /// Ordered by id; the last one is the active segment.
    segs: Vec<Segment>,
    /// The last frame's buffer, kept for the next one.
    spare: Cell<Vec<u8>>,
    /// Set while a `FrameWriter` exists: there is one frame at a time.
    writing: Rc<Cell<bool>>,
    next_lsn: u64,
    /// Active-segment offset below which the page cache has been dropped.
    dropped_upto: u64,
}

/// A transaction's frame while it is being written. Ops are added one at a
/// time; once `chunk_bytes` of them are buffered they go to the file, behind
/// a header that is still zero, so until `LogStore::end_frame` writes the
/// header recovery takes whatever is there for a torn tail. A frame that
/// never fills the buffer is written with a single `pwrite`, header
/// included. A writer dropped without `end_frame` cuts off what it wrote.
pub struct FrameWriter {
    /// The active segment, through its own descriptor, so the log can still
    /// be read while the frame is written.
    file: File,
    seg: u32,
    /// Offset of the frame's header in the segment.
    start: u64,
    /// Bytes not written yet; while `headed`, after 8 bytes kept free for
    /// the header.
    buf: Vec<u8>,
    headed: bool,
    /// Payload bytes already in the file, and how many of them are synced.
    flushed: u64,
    synced: u64,
    /// Checksum of the flushed payload.
    crc: Crc32c,
    ops_left: u64,
    /// One encoded row.
    row: Vec<u8>,
    chunk_bytes: usize,
    sync: bool,
    drop_cache: bool,
    finished: bool,
    writing: Rc<Cell<bool>>,
}

impl FrameWriter {
    fn payload_len(&self) -> u64 {
        self.flushed + self.buf.len() as u64 - if self.headed { FRAME_HEADER } else { 0 }
    }

    /// Adds a row; `encode` appends its body to the buffer it is given.
    /// Returns where the body will be once the frame is committed.
    pub fn put(&mut self, table: u32, pk: i64, encode: impl FnOnce(&mut Vec<u8>) -> DbResult<()>) -> DbResult<RowLoc> {
        self.begin_op()?;
        self.row.clear();
        encode(&mut self.row)?;
        let len = u32::try_from(self.row.len()).map_err(|_| db_err!(Internal, "row of {} bytes is too large", self.row.len()))?;
        self.buf.push(OP_PUT);
        codec::put_u32(&mut self.buf, table);
        codec::put_i64(&mut self.buf, pk);
        codec::put_varint(&mut self.buf, len as u64);
        let loc = RowLoc { seg: self.seg, off: self.start + FRAME_HEADER + self.payload_len(), len };
        self.buf.extend_from_slice(&self.row);
        self.end_op()?;
        Ok(loc)
    }

    pub fn delete(&mut self, table: u32, pk: i64) -> DbResult<()> {
        self.begin_op()?;
        self.buf.push(OP_DELETE);
        codec::put_u32(&mut self.buf, table);
        codec::put_i64(&mut self.buf, pk);
        self.end_op()
    }

    /// Adds a catalog change.
    fn ddl(&mut self, op: &Op) -> DbResult<()> {
        self.begin_op()?;
        match op {
            Op::CreateTable(def) => {
                self.buf.push(OP_CREATE_TABLE);
                def.encode(&mut self.buf);
            }
            Op::DropTable { table } => {
                self.buf.push(OP_DROP_TABLE);
                codec::put_u32(&mut self.buf, *table);
            }
            Op::CreateIndex { table, name } => {
                self.buf.push(OP_CREATE_INDEX);
                codec::put_u32(&mut self.buf, *table);
                codec::put_str(&mut self.buf, name);
            }
            Op::Put { .. } | Op::Delete { .. } => bail!(Internal, "not a catalog change"),
        }
        self.end_op()
    }

    fn begin_op(&mut self) -> DbResult<()> {
        if self.ops_left == 0 {
            bail!(Internal, "more operations than the frame was opened for");
        }
        self.ops_left -= 1;
        Ok(())
    }

    fn end_op(&mut self) -> DbResult<()> {
        if self.payload_len() > u32::MAX as u64 {
            bail!(Internal, "transaction of {} bytes is too large", self.payload_len());
        }
        if self.buf.len() >= self.chunk_bytes {
            self.flush()?;
        }
        Ok(())
    }

    /// Writes the buffered payload to its place behind the header.
    fn flush(&mut self) -> io::Result<()> {
        let body = &self.buf[if self.headed { FRAME_HEADER as usize } else { 0 }..];
        self.crc.update(body);
        self.file.write_all_at(body, self.start + FRAME_HEADER + self.flushed)?;
        self.flushed += body.len() as u64;
        self.buf.clear();
        self.headed = false;
        if self.flushed - self.synced >= SYNC_EVERY {
            if self.sync {
                self.file.sync_data()?;
            }
            // Whole pages of what is now durable; never the page that holds
            // the header, which is still to be written.
            let from = (self.start + FRAME_HEADER + self.synced).next_multiple_of(PAGE);
            let upto = (self.start + FRAME_HEADER + self.flushed) & !(PAGE - 1);
            if self.drop_cache && upto > from {
                drop_cache(&self.file, from, upto - from);
            }
            self.synced = self.flushed;
        }
        Ok(())
    }

    /// Writes what is left and then the header, and makes the frame durable.
    fn finish(&mut self) -> io::Result<()> {
        let len = (self.payload_len() as u32).to_le_bytes();
        if self.headed {
            let crc = crc32c(&self.buf[FRAME_HEADER as usize..]);
            self.buf[0..4].copy_from_slice(&len);
            self.buf[4..8].copy_from_slice(&crc.to_le_bytes());
            self.file.write_all_at(&self.buf, self.start)?;
        } else {
            self.flush()?;
            let mut header = [0u8; FRAME_HEADER as usize];
            header[0..4].copy_from_slice(&len);
            header[4..8].copy_from_slice(&self.crc.finish().to_le_bytes());
            self.file.write_all_at(&header, self.start)?;
        }
        if self.sync {
            self.file.sync_data()?;
        }
        Ok(())
    }
}

impl Drop for FrameWriter {
    fn drop(&mut self) {
        // Given up after part of it reached the file: cut that part off, so
        // the next frame does not land behind garbage. Nothing can have been
        // appended after it, since there is one writer at a time.
        if !self.finished && !self.headed {
            let _ = self.file.set_len(self.start);
        }
        self.writing.set(false);
    }
}

impl LogStore {
    /// Opens the log in `dir`, creating it if needed, and replays every valid
    /// frame through `apply` in commit order.
    pub fn open(
        dir: &Path,
        opts: LogOptions,
        mut apply: impl FnMut(u64, &[OpView<'_>]) -> DbResult<()>,
    ) -> DbResult<LogStore> {
        fs::create_dir_all(dir)?;
        let ids = list_segments(dir)?;
        let mut segs = Vec::with_capacity(ids.len());
        let mut last_lsn = 0;
        let mut payload = Vec::new();
        for (i, &id) in ids.iter().enumerate() {
            let file = OpenOptions::new().read(true).write(true).open(segment_path(dir, id))?;
            let file_len = file.metadata()?.len();
            let end = scan_segment(&file, id, file_len, &opts, &mut payload, |lsn, ops| {
                last_lsn = lsn;
                apply(lsn, ops)
            })?;
            if end < file_len {
                if i + 1 != ids.len() {
                    bail!(Corrupt, "segment {} is damaged at offset {} but is not the newest segment", id, end);
                }
                eprintln!("log: discarding torn tail of segment {id}: {} bytes at offset {end}", file_len - end);
                file.set_len(end)?;
                file.sync_all()?;
            }
            if opts.drop_cache {
                drop_cache(&file, 0, end);
            }
            segs.push(Segment { id, file, len: end });
        }
        let mut store = LogStore {
            dir: dir.to_path_buf(),
            opts,
            segs,
            spare: Cell::new(Vec::new()),
            writing: Rc::new(Cell::new(false)),
            next_lsn: last_lsn + 1,
            dropped_upto: 0,
        };
        match store.segs.last() {
            Some(s) => store.dropped_upto = s.len & !(PAGE - 1),
            None => store.new_segment(1)?,
        }
        Ok(store)
    }

    /// Durably appends one transaction. Returns the location of each `Put`
    /// row body, in op order. On error nothing is appended.
    pub fn append(&mut self, ops: &[Op]) -> DbResult<Vec<RowLoc>> {
        let mut frame = self.begin_frame(ops.len() as u64)?;
        let mut locs = Vec::new();
        for op in ops {
            match op {
                Op::Put { table, pk, row } => locs.push(frame.put(*table, *pk, |buf| {
                    buf.extend_from_slice(row);
                    Ok(())
                })?),
                Op::Delete { table, pk } => frame.delete(*table, *pk)?,
                catalog => frame.ddl(catalog)?,
            }
        }
        self.end_frame(frame)?;
        Ok(locs)
    }

    /// Starts the frame of one transaction with `ops` operations. They are
    /// added to the writer, which `end_frame` then commits; until then, and
    /// if the writer is dropped instead, the log is unchanged.
    pub fn begin_frame(&self, ops: u64) -> DbResult<FrameWriter> {
        if self.writing.replace(true) {
            bail!(Internal, "a log frame is already being written");
        }
        let seg = self.segs.last().expect("log always has an active segment");
        let mut buf = self.spare.take();
        buf.clear();
        buf.extend_from_slice(&[0u8; FRAME_HEADER as usize]);
        codec::put_u64(&mut buf, self.next_lsn);
        codec::put_varint(&mut buf, ops);
        Ok(FrameWriter {
            file: seg.file.try_clone().inspect_err(|_| self.writing.set(false))?,
            seg: seg.id,
            start: seg.len,
            buf,
            headed: true,
            flushed: 0,
            synced: 0,
            crc: Crc32c::new(),
            ops_left: ops,
            row: Vec::new(),
            chunk_bytes: self.opts.chunk_bytes,
            sync: self.opts.sync,
            drop_cache: self.opts.drop_cache,
            finished: false,
            writing: Rc::clone(&self.writing),
        })
    }

    /// Commits a frame: once this returns the transaction is durable. On
    /// error nothing of it is left in the log.
    pub fn end_frame(&mut self, mut frame: FrameWriter) -> DbResult<()> {
        let seg = self.segs.last_mut().expect("log always has an active segment");
        if frame.ops_left != 0 || frame.seg != seg.id || frame.start != seg.len {
            bail!(Internal, "log frame does not match the log: {} operations missing", frame.ops_left);
        }
        if let Err(e) = frame.finish() {
            // Cut off whatever part of the frame reached the file so the next
            // frame does not land behind garbage.
            let _ = seg.file.set_len(frame.start);
            frame.finished = true;
            return Err(e.into());
        }
        frame.finished = true;
        seg.len = frame.start + FRAME_HEADER + frame.payload_len();
        self.next_lsn += 1;
        let seg_id = seg.id;

        // The written pages are durable and will be read back only through
        // our own caches, so drop them from the page cache (it counts toward
        // the container's memory). Keep the partial last page: the next
        // append writes into it and would otherwise have to read it back.
        let keep_from = seg.len & !(PAGE - 1);
        if keep_from > self.dropped_upto && self.opts.drop_cache {
            drop_cache(&seg.file, self.dropped_upto, keep_from - self.dropped_upto);
            self.dropped_upto = keep_from;
        }
        if seg.len >= self.opts.segment_bytes {
            // The frame is durable, so the commit has happened even if the
            // next segment cannot be started; the next append tries again.
            if let Err(e) = self.new_segment(seg_id + 1) {
                eprintln!("log: cannot start segment {}: {e}; segment {seg_id} stays active", seg_id + 1);
            }
        }
        let buf = std::mem::take(&mut frame.buf);
        if buf.capacity() <= SPARE_BYTES {
            self.spare.set(buf);
        }
        Ok(())
    }

    /// Reads a row body previously returned by `append` or replay.
    pub fn read(&self, loc: RowLoc, out: &mut Vec<u8>) -> DbResult<()> {
        let idx = self
            .segs
            .binary_search_by_key(&loc.seg, |s| s.id)
            .map_err(|_| db_err!(Corrupt, "row points at missing segment {}", loc.seg))?;
        out.resize(loc.len as usize, 0);
        self.segs[idx].file.read_exact_at(out, loc.off)?;
        Ok(())
    }

    /// Streams every committed frame again, oldest first, dropping the pages
    /// it has read from the page cache as it goes. Used to rebuild a table's
    /// row cache.
    pub fn scan(&self, mut f: impl FnMut(u64, &[OpView<'_>]) -> DbResult<()>) -> DbResult<()> {
        let mut payload = Vec::new();
        for seg in &self.segs {
            let end = scan_segment(&seg.file, seg.id, seg.len, &self.opts, &mut payload, &mut f)?;
            if end != seg.len {
                bail!(Corrupt, "segment {} changed under a scan", seg.id);
            }
            if self.opts.drop_cache {
                // Not the partial last page of the active segment: the next
                // append writes into it and would have to read it back.
                let upto = if seg.id == self.active_id() { seg.len & !(PAGE - 1) } else { seg.len };
                drop_cache(&seg.file, 0, upto);
            }
        }
        Ok(())
    }

    pub fn active_id(&self) -> u32 {
        self.segs.last().map_or(0, |s| s.id)
    }

    /// Total bytes across all segments.
    pub fn size(&self) -> u64 {
        self.segs.iter().map(|s| s.len).sum()
    }

    fn new_segment(&mut self, id: u32) -> DbResult<()> {
        // An attempt that failed after creating the file leaves it empty.
        let path = segment_path(&self.dir, id);
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
        if file.metadata()?.len() != 0 {
            bail!(Corrupt, "new segment {} already holds data", id);
        }
        if self.opts.sync {
            File::open(&self.dir)?.sync_all()?;
        }
        self.segs.push(Segment { id, file, len: 0 });
        self.dropped_upto = 0;
        Ok(())
    }
}

fn segment_path(dir: &Path, id: u32) -> PathBuf {
    dir.join(format!("seg-{id:08}.log"))
}

fn list_segments(dir: &Path) -> DbResult<Vec<u32>> {
    let mut ids = Vec::new();
    for entry in fs::read_dir(dir)? {
        let name = entry?.file_name();
        let Some(name) = name.to_str() else { continue };
        if let Some(id) = name.strip_prefix("seg-").and_then(|s| s.strip_suffix(".log")) {
            if let Ok(id) = id.parse() {
                ids.push(id);
            }
        }
    }
    ids.sort_unstable();
    Ok(ids)
}

/// `payload` holds one frame's payload starting at file offset `base`.
fn decode_frame(payload: &[u8], seg: u32, base: u64) -> DbResult<(u64, Vec<OpView<'_>>)> {
    let mut r = Reader::new(payload);
    let lsn = r.u64()?;
    let n = r.varint()? as usize;
    let mut ops = Vec::with_capacity(n.min(4096));
    for _ in 0..n {
        let op = match r.u8()? {
            OP_CREATE_TABLE => OpView::CreateTable(TableDef::decode(&mut r)?),
            OP_DROP_TABLE => OpView::DropTable { table: r.u32()? },
            OP_CREATE_INDEX => OpView::CreateIndex { table: r.u32()?, name: r.str()?.to_string() },
            OP_PUT => {
                let table = r.u32()?;
                let pk = r.i64()?;
                let len = r.varint()?;
                let at = r.pos() as u64;
                let row = r.take(len as usize)?;
                OpView::Put { table, pk, row, loc: RowLoc { seg, off: base + at, len: len as u32 } }
            }
            OP_DELETE => OpView::Delete { table: r.u32()?, pk: r.i64()? },
            t => bail!(Corrupt, "unknown log op {}", t),
        };
        ops.push(op);
    }
    if r.remaining() != 0 {
        bail!(Corrupt, "{} trailing bytes in log frame {}", r.remaining(), lsn);
    }
    Ok((lsn, ops))
}

/// Replays frames from the start of `file` up to `limit` bytes and returns
/// the offset just past the last valid frame. A frame that is cut short or
/// fails its checksum ends the scan; a frame that checks out but does not
/// decode is real corruption and is an error. `payload` is scratch space.
fn scan_segment(
    file: &File,
    seg: u32,
    limit: u64,
    opts: &LogOptions,
    payload: &mut Vec<u8>,
    mut f: impl FnMut(u64, &[OpView<'_>]) -> DbResult<()>,
) -> DbResult<u64> {
    let drop_behind = opts.drop_cache;
    let mut reader = BufReader::with_capacity(SCAN_BUFFER, PosReader { file, pos: 0, drop_behind, dropped: 0 });
    let mut off = 0u64;
    while off + FRAME_HEADER <= limit {
        let mut header = [0u8; FRAME_HEADER as usize];
        reader.read_exact(&mut header)?;
        let len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as u64;
        let crc = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
        if len == 0 || off + FRAME_HEADER + len > limit {
            break;
        }
        let base = off + FRAME_HEADER;
        if len > opts.big_frame_bytes && holds_rows(file, base) {
            // Too large to hold: one pass for the checksum, and only if it
            // is right a second one that hands the ops on as they are read.
            let mut sum = Crc32c::new();
            payload.resize(64 << 10, 0);
            let mut left = len;
            while left > 0 {
                let n = left.min(payload.len() as u64) as usize;
                reader.read_exact(&mut payload[..n])?;
                sum.update(&payload[..n]);
                left -= n as u64;
            }
            if sum.finish() != crc {
                break;
            }
            replay_large_frame(file, seg, base, len, drop_behind, payload, &mut f)?;
        } else {
            payload.resize(len as usize, 0);
            reader.read_exact(payload)?;
            if crc32c(payload) != crc {
                break;
            }
            let (lsn, ops) = decode_frame(payload, seg, base)?;
            f(lsn, &ops)?;
        }
        off += FRAME_HEADER + len;
    }
    Ok(off)
}

/// Whether the frame whose payload starts at `base` begins with a row
/// operation. Only such frames are ever large; a catalog change is a small
/// frame of its own. Anything unreadable is left to the ordinary path.
fn holds_rows(file: &File, base: u64) -> bool {
    let mut head = [0u8; 32];
    let n = file.read_at(&mut head, base).unwrap_or(0);
    let mut r = Reader::new(&head[..n]);
    r.u64().and_then(|_| r.varint()).and_then(|_| r.u8()).is_ok_and(|tag| tag == OP_PUT || tag == OP_DELETE)
}

/// Replays a frame of row operations whose checksum has been verified,
/// reading it again from `base` and handing each op on as soon as it is
/// read, so memory does not grow with the frame. `row` is scratch space.
fn replay_large_frame(
    file: &File,
    seg: u32,
    base: u64,
    len: u64,
    drop_behind: bool,
    row: &mut Vec<u8>,
    f: &mut impl FnMut(u64, &[OpView<'_>]) -> DbResult<()>,
) -> DbResult<()> {
    let source = PosReader { file, pos: base, drop_behind, dropped: base & !(PAGE - 1) };
    let mut r = FrameReader { src: BufReader::with_capacity(SCAN_BUFFER, source).take(len), pos: 0 };
    let lsn = u64::from_le_bytes(r.array()?);
    let ops = r.varint()?;
    for _ in 0..ops {
        match r.array::<1>()?[0] {
            OP_PUT => {
                let table = u32::from_le_bytes(r.array()?);
                let pk = i64::from_le_bytes(r.array()?);
                let row_len = r.varint()?;
                let at = r.pos;
                if row_len > len - at {
                    bail!(Corrupt, "row of {} bytes runs past the end of log frame {}", row_len, lsn);
                }
                row.resize(row_len as usize, 0);
                r.fill(row)?;
                let loc = RowLoc { seg, off: base + at, len: row_len as u32 };
                f(lsn, &[OpView::Put { table, pk, row, loc }])?;
            }
            OP_DELETE => {
                let table = u32::from_le_bytes(r.array()?);
                let pk = i64::from_le_bytes(r.array()?);
                f(lsn, &[OpView::Delete { table, pk }])?;
            }
            t => bail!(Corrupt, "unexpected log op {} in large frame {}", t, lsn),
        }
    }
    if r.pos != len {
        bail!(Corrupt, "{} trailing bytes in log frame {}", len - r.pos, lsn);
    }
    Ok(())
}

/// Reads a frame's payload piece by piece and counts the bytes read. A
/// payload that ends early is corruption: its checksum was right.
struct FrameReader<R: Read> {
    src: R,
    pos: u64,
}

impl<R: Read> FrameReader<R> {
    fn fill(&mut self, out: &mut [u8]) -> DbResult<()> {
        self.src.read_exact(out).map_err(|e| match e.kind() {
            io::ErrorKind::UnexpectedEof => db_err!(Corrupt, "log frame ends inside an operation"),
            _ => e.into(),
        })?;
        self.pos += out.len() as u64;
        Ok(())
    }

    fn array<const N: usize>(&mut self) -> DbResult<[u8; N]> {
        let mut a = [0u8; N];
        self.fill(&mut a)?;
        Ok(a)
    }

    fn varint(&mut self) -> DbResult<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = self.array::<1>()?[0];
            v |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        bail!(Corrupt, "varint too long")
    }
}

/// `Read` over a shared `File` using positional reads, so scans never move
/// the file cursor. With `drop_behind`, pages before the read position are
/// dropped from the page cache as the scan goes, so a replay holds at most
/// about one buffer of the log in cache instead of a whole segment.
struct PosReader<'a> {
    file: &'a File,
    pos: u64,
    drop_behind: bool,
    dropped: u64,
}

impl Read for PosReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.drop_behind {
            let upto = self.pos & !(PAGE - 1);
            if upto > self.dropped {
                drop_cache(self.file, self.dropped, upto - self.dropped);
                self.dropped = upto;
            }
        }
        let n = self.file.read_at(buf, self.pos)?;
        self.pos += n as u64;
        Ok(n)
    }
}

/// Asks the kernel to drop clean cached pages of `[off, off + len)`.
fn drop_cache(file: &File, off: u64, len: u64) {
    extern "C" {
        fn posix_fadvise(fd: i32, offset: i64, len: i64, advice: i32) -> i32;
    }
    const POSIX_FADV_DONTNEED: i32 = 4;
    // len == 0 would mean "to the end of the file".
    if len == 0 {
        return;
    }
    // SAFETY: plain syscall wrapper on a valid open descriptor; advisory only.
    unsafe {
        posix_fadvise(file.as_raw_fd(), off as i64, len as i64, POSIX_FADV_DONTNEED);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::catalog::Column;
    use crate::types::DataType;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> TempDir {
            static N: AtomicU32 = AtomicU32::new(0);
            let p = std::env::temp_dir().join(format!("mvdb-log-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
            let _ = fs::remove_dir_all(&p);
            TempDir(p)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn opts() -> LogOptions {
        LogOptions { sync: false, ..LogOptions::default() }
    }

    fn table() -> TableDef {
        TableDef {
            id: 7,
            name: "t".into(),
            columns: vec![Column { name: "id".into(), ty: DataType::BigInt }, Column { name: "v".into(), ty: DataType::Vector(2) }],
            pk: 0,
            vector_index: None,
        }
    }

    /// Owned copy of what replay saw: (lsn, ops) with row bytes and locations.
    #[derive(Debug, PartialEq)]
    enum Seen {
        Create(u32),
        Drop(u32),
        Index(u32, String),
        Put(u32, i64, Vec<u8>, RowLoc),
        Del(u32, i64),
    }

    fn replay(dir: &Path, o: LogOptions) -> (LogStore, Vec<(u64, Vec<Seen>)>) {
        let mut seen = Vec::new();
        let store = LogStore::open(dir, o, |lsn, ops| {
            seen.push((lsn, ops.iter().map(own).collect()));
            Ok(())
        })
        .unwrap();
        (store, seen)
    }

    fn own(op: &OpView<'_>) -> Seen {
        match op {
            OpView::CreateTable(d) => Seen::Create(d.id),
            OpView::DropTable { table } => Seen::Drop(*table),
            OpView::CreateIndex { table, name } => Seen::Index(*table, name.clone()),
            OpView::Put { table, pk, row, loc } => Seen::Put(*table, *pk, row.to_vec(), *loc),
            OpView::Delete { table, pk } => Seen::Del(*table, *pk),
        }
    }

    fn put(pk: i64, fill: u8, len: usize) -> Op {
        Op::Put { table: 7, pk, row: vec![fill; len] }
    }

    #[test]
    fn append_replay_and_read() {
        let dir = TempDir::new();
        let (mut log, seen) = replay(&dir.0, opts());
        assert!(seen.is_empty());
        log.append(&[Op::CreateTable(table())]).unwrap();
        let locs = log.append(&[put(1, 0xaa, 8), Op::Delete { table: 7, pk: 9 }, put(2, 0xbb, 3)]).unwrap();
        log.append(&[Op::CreateIndex { table: 7, name: "ix".into() }, Op::DropTable { table: 7 }]).unwrap();
        assert_eq!(locs.len(), 2);
        let mut buf = Vec::new();
        log.read(locs[1], &mut buf).unwrap();
        assert_eq!(buf, vec![0xbb; 3]);
        drop(log);

        let (log, seen) = replay(&dir.0, opts());
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[0], (1, vec![Seen::Create(7)]));
        assert_eq!(
            seen[1],
            (2, vec![Seen::Put(7, 1, vec![0xaa; 8], locs[0]), Seen::Del(7, 9), Seen::Put(7, 2, vec![0xbb; 3], locs[1])])
        );
        assert_eq!(seen[2], (3, vec![Seen::Index(7, "ix".into()), Seen::Drop(7)]));
        log.read(locs[0], &mut buf).unwrap();
        assert_eq!(buf, vec![0xaa; 8]);
    }

    /// Cut the log at every byte offset inside the last frame: recovery must
    /// keep exactly the earlier frames, and recovering again changes nothing.
    #[test]
    fn torn_tail_at_every_offset() {
        let dir = TempDir::new();
        let (mut log, _) = replay(&dir.0, opts());
        log.append(&[put(1, 1, 10)]).unwrap();
        log.append(&[put(2, 2, 10)]).unwrap();
        let good = log.size();
        log.append(&[put(3, 3, 40), put(4, 4, 40)]).unwrap();
        let full = log.size();
        drop(log);
        let path = segment_path(&dir.0, 1);
        let bytes = fs::read(&path).unwrap();
        for cut in good..full {
            fs::write(&path, &bytes[..cut as usize]).unwrap();
            let (log, seen) = replay(&dir.0, opts());
            assert_eq!(seen.len(), 2, "cut at {cut}");
            assert_eq!(log.size(), good);
            drop(log);
            assert_eq!(fs::metadata(&path).unwrap().len(), good);
            let (_, again) = replay(&dir.0, opts());
            assert_eq!(again.len(), 2);
        }
    }

    #[test]
    fn corrupt_tail_is_discarded_and_appends_continue() {
        let dir = TempDir::new();
        let (mut log, _) = replay(&dir.0, opts());
        log.append(&[put(1, 1, 16)]).unwrap();
        log.append(&[put(2, 2, 16)]).unwrap();
        let size = log.size();
        drop(log);
        let path = segment_path(&dir.0, 1);
        let mut bytes = fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff; // flip a payload byte of frame 2
        bytes.extend_from_slice(&[0x55; 100]); // and leave garbage after it
        fs::write(&path, &bytes).unwrap();

        let (mut log, seen) = replay(&dir.0, opts());
        assert_eq!(seen.len(), 1);
        assert!(log.size() < size);
        log.append(&[put(5, 5, 16)]).unwrap();
        drop(log);
        let (_, seen) = replay(&dir.0, opts());
        let pks: Vec<i64> = seen.iter().map(|(_, ops)| match &ops[0] { Seen::Put(_, pk, ..) => *pk, _ => -1 }).collect();
        assert_eq!(pks, vec![1, 5]);
        assert_eq!(seen[1].0, 2, "lsn continues after the last valid frame");
    }

    #[test]
    fn segments_roll_over() {
        let dir = TempDir::new();
        let o = LogOptions { segment_bytes: 200, ..opts() };
        let (mut log, _) = replay(&dir.0, o.clone());
        let mut locs = Vec::new();
        for pk in 0..20 {
            locs.extend(log.append(&[put(pk, pk as u8, 50)]).unwrap());
        }
        assert!(log.active_id() > 3);
        drop(log);
        let (log, seen) = replay(&dir.0, o);
        assert_eq!(seen.len(), 20);
        let mut buf = Vec::new();
        for (pk, loc) in locs.iter().enumerate() {
            log.read(*loc, &mut buf).unwrap();
            assert_eq!(buf, vec![pk as u8; 50]);
        }
        let mut scanned = 0;
        log.scan(|_, ops| {
            scanned += ops.len();
            Ok(())
        })
        .unwrap();
        assert_eq!(scanned, 20);
    }

    /// A commit whose frame is written stands even when the rollover after
    /// it fails (here: the directory is read-only); the rollover happens on
    /// a later append.
    #[test]
    fn failed_rollover_keeps_the_commit() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new();
        let o = LogOptions { segment_bytes: 100, sync: true, ..opts() };
        let (mut log, _) = replay(&dir.0, o.clone());
        let mode = |m: u32| fs::set_permissions(&dir.0, fs::Permissions::from_mode(m)).unwrap();
        mode(0o555);
        if File::create(dir.0.join("probe")).is_ok() {
            mode(0o755);
            eprintln!("skipped: running with permission to write a read-only directory");
            return;
        }
        let a = log.append(&[put(1, 1, 150)]).unwrap();
        let b = log.append(&[put(2, 2, 150)]).unwrap();
        assert_eq!(log.active_id(), 1);
        mode(0o755);
        let c = log.append(&[put(3, 3, 150)]).unwrap();
        assert_eq!(log.active_id(), 2);
        drop(log);
        let (log, seen) = replay(&dir.0, o);
        assert_eq!(seen.len(), 3);
        let mut buf = Vec::new();
        for (loc, fill) in [(a[0], 1), (b[0], 2), (c[0], 3)] {
            log.read(loc, &mut buf).unwrap();
            assert_eq!(buf, vec![fill; 150]);
        }
    }

    #[test]
    fn empty_newest_segment_is_fine() {
        let dir = TempDir::new();
        let (mut log, _) = replay(&dir.0, opts());
        log.append(&[put(1, 1, 4)]).unwrap();
        drop(log);
        // Simulates a crash right after a rollover created the next segment.
        File::create(segment_path(&dir.0, 2)).unwrap();
        let (mut log, seen) = replay(&dir.0, opts());
        assert_eq!(seen.len(), 1);
        assert_eq!(log.active_id(), 2);
        log.append(&[put(2, 2, 4)]).unwrap();
        drop(log);
        assert_eq!(replay(&dir.0, opts()).1.len(), 2);
    }

    #[test]
    fn damaged_older_segment_is_an_error() {
        let dir = TempDir::new();
        let o = LogOptions { segment_bytes: 100, ..opts() };
        let (mut log, _) = replay(&dir.0, o.clone());
        for pk in 0..6 {
            log.append(&[put(pk, 1, 60)]).unwrap();
        }
        drop(log);
        let path = segment_path(&dir.0, 1);
        let mut bytes = fs::read(&path).unwrap();
        let n = bytes.len();
        bytes[n - 1] ^= 1;
        fs::write(&path, &bytes).unwrap();
        let err = LogStore::open(&dir.0, o, |_, _| Ok(())).err().unwrap();
        assert_eq!(err.kind, crate::error::ErrorKind::Corrupt);
    }

    /// A mixed log: catalog changes in frames of their own, then
    /// transactions of rows and deletes.
    fn mixed(log: &mut LogStore) {
        log.append(&[Op::CreateTable(table())]).unwrap();
        log.append(&[put(1, 1, 30), put(2, 2, 300), Op::Delete { table: 7, pk: 9 }, put(3, 3, 1)]).unwrap();
        log.append(&[Op::CreateIndex { table: 7, name: "ix".into() }]).unwrap();
        log.append(&[Op::Delete { table: 7, pk: 1 }]).unwrap();
        log.append(&(10..40).map(|pk| put(pk, pk as u8, 20 + pk as usize)).collect::<Vec<_>>()).unwrap();
        log.append(&[put(2, 9, 0)]).unwrap();
    }

    /// A frame written in pieces is byte for byte the frame written at once.
    #[test]
    fn streamed_frames_are_byte_identical() {
        let (whole, pieces) = (TempDir::new(), TempDir::new());
        mixed(&mut replay(&whole.0, opts()).0);
        for chunk_bytes in [1, 64, 1000] {
            let _ = fs::remove_dir_all(&pieces.0);
            mixed(&mut replay(&pieces.0, LogOptions { chunk_bytes, ..opts() }).0);
            assert_eq!(fs::read(segment_path(&pieces.0, 1)).unwrap(), fs::read(segment_path(&whole.0, 1)).unwrap(), "chunks of {chunk_bytes}");
        }
    }

    /// Row locations handed out while a frame is written in pieces point at
    /// the rows once it is committed.
    #[test]
    fn streamed_frame_locations() {
        let dir = TempDir::new();
        let (mut log, _) = replay(&dir.0, LogOptions { chunk_bytes: 50, ..opts() });
        log.append(&[put(0, 7, 5)]).unwrap();
        let mut frame = log.begin_frame(41).unwrap();
        let mut locs = Vec::new();
        for pk in 0..40i64 {
            locs.push(frame.put(7, pk, |buf| {
                buf.extend(std::iter::repeat_n(pk as u8, 10 + pk as usize));
                Ok(())
            })
            .unwrap());
        }
        frame.delete(7, 99).unwrap();
        log.end_frame(frame).unwrap();
        let mut buf = Vec::new();
        for (pk, loc) in locs.iter().enumerate() {
            log.read(*loc, &mut buf).unwrap();
            assert_eq!(buf, vec![pk as u8; 10 + pk], "row {pk}");
        }
        drop(log);
        let (_, seen) = replay(&dir.0, opts());
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[1].1.len(), 41);
        assert_eq!(seen[1].1[3], Seen::Put(7, 3, vec![3; 13], locs[3]));
    }

    /// Cut the log at every byte offset inside a frame that was written in
    /// pieces, header last.
    #[test]
    fn torn_streamed_frame_at_every_offset() {
        let dir = TempDir::new();
        let o = LogOptions { chunk_bytes: 64, ..opts() };
        let (mut log, _) = replay(&dir.0, o.clone());
        log.append(&[put(1, 1, 10)]).unwrap();
        let good = log.size();
        log.append(&(2..12).map(|pk| put(pk, pk as u8, 40)).collect::<Vec<_>>()).unwrap();
        let full = log.size();
        drop(log);
        let path = segment_path(&dir.0, 1);
        let bytes = fs::read(&path).unwrap();
        for cut in good..full {
            fs::write(&path, &bytes[..cut as usize]).unwrap();
            let (log, seen) = replay(&dir.0, o.clone());
            assert_eq!(seen.len(), 1, "cut at {cut}");
            assert_eq!(log.size(), good);
        }
    }

    /// The state a crash leaves between the last piece and the header: the
    /// payload is all there, the header is still zero.
    #[test]
    fn frame_without_its_header_is_discarded() {
        let dir = TempDir::new();
        let o = LogOptions { chunk_bytes: 64, ..opts() };
        let (mut log, _) = replay(&dir.0, o.clone());
        log.append(&[put(1, 1, 10)]).unwrap();
        let good = log.size();
        log.append(&(2..12).map(|pk| put(pk, pk as u8, 40)).collect::<Vec<_>>()).unwrap();
        drop(log);
        let path = segment_path(&dir.0, 1);
        let mut bytes = fs::read(&path).unwrap();
        bytes[good as usize..good as usize + FRAME_HEADER as usize].fill(0);
        fs::write(&path, &bytes).unwrap();
        let (mut log, seen) = replay(&dir.0, o.clone());
        assert_eq!(seen.len(), 1);
        assert_eq!(fs::metadata(&path).unwrap().len(), good);
        log.append(&[put(50, 5, 16)]).unwrap();
        drop(log);
        let (_, seen) = replay(&dir.0, o);
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[1].0, 2, "lsn continues after the last valid frame");
    }

    /// A writer that is dropped leaves the log as it was, also after pieces
    /// of its frame reached the file, and the next frame starts cleanly.
    #[test]
    fn abandoned_frame_leaves_no_trace() {
        let dir = TempDir::new();
        let o = LogOptions { chunk_bytes: 64, ..opts() };
        let (mut log, _) = replay(&dir.0, o.clone());
        log.append(&[put(1, 1, 10)]).unwrap();
        let good = log.size();
        let path = segment_path(&dir.0, 1);
        let mut frame = log.begin_frame(100).unwrap();
        assert!(log.begin_frame(1).is_err(), "one frame at a time");
        for pk in 0..20 {
            frame.put(7, pk, |buf| {
                buf.extend_from_slice(&[9; 50]);
                Ok(())
            })
            .unwrap();
        }
        assert!(fs::metadata(&path).unwrap().len() > good, "pieces were written");
        // Ending it with operations missing fails and cuts it off as well.
        assert!(log.end_frame(frame).is_err());
        assert_eq!(fs::metadata(&path).unwrap().len(), good);
        let frame = log.begin_frame(1).unwrap();
        drop(frame);
        assert_eq!(log.size(), good);
        log.append(&[put(2, 2, 10)]).unwrap();
        drop(log);
        let (_, seen) = replay(&dir.0, o);
        assert_eq!(seen.iter().map(|(lsn, ops)| (*lsn, ops.len())).collect::<Vec<_>>(), [(1, 1), (2, 1)]);
    }

    /// Replaying row frames op by op gives what replaying them whole gives,
    /// through torn and corrupt tails too.
    #[test]
    fn large_frames_replay_like_small_ones() {
        let dir = TempDir::new();
        mixed(&mut replay(&dir.0, opts()).0);
        let (_, whole) = replay(&dir.0, opts());
        let streamed = LogOptions { big_frame_bytes: 0, ..opts() };
        let (_, pieces) = replay(&dir.0, streamed.clone());
        // Op by op, a frame arrives as one call per op under the same lsn.
        let flat = |seen: &[(u64, Vec<Seen>)]| -> Vec<(u64, String)> {
            seen.iter().flat_map(|(lsn, ops)| ops.iter().map(|op| (*lsn, format!("{op:?}")))).collect()
        };
        assert_eq!(flat(&pieces), flat(&whole));
        assert!(pieces.len() > whole.len(), "row frames were replayed op by op");

        let path = segment_path(&dir.0, 1);
        let bytes = fs::read(&path).unwrap();
        let ops_whole = flat(&whole).len();
        // Corrupt the last frame: both ways stop before it.
        let mut bad = bytes.clone();
        let last = bad.len() - 1;
        bad[last] ^= 0xff;
        fs::write(&path, &bad).unwrap();
        let (_, a) = replay(&dir.0, streamed.clone());
        assert_eq!(flat(&a).len(), ops_whole - 1);
        // Cut inside the large frame of thirty rows: it is dropped whole.
        fs::write(&path, &bytes[..bytes.len() - 100]).unwrap();
        let (_, b) = replay(&dir.0, streamed);
        assert_eq!(flat(&b).len(), ops_whole - 31);
    }
}
