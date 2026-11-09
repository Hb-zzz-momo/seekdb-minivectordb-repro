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
//! separate undo log. Recovery replays valid frames in order and truncates
//! the active segment at the first invalid one; running it again on the
//! truncated file finds the same frames, so an interrupted recovery is safe.
//!
//! Segments are `seg-<id>.log` files; only the highest-numbered one is
//! appended to. Row locations handed out by `append` point straight at the
//! row bytes inside a frame so point reads need one `pread`.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read};
use std::os::unix::fs::FileExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use super::catalog::TableDef;
use super::codec::{self, Reader};
use super::crc::crc32c;
use crate::error::DbResult;
use crate::{bail, db_err};

const FRAME_HEADER: u64 = 8;
const PAGE: u64 = 4096;
const SCAN_BUFFER: usize = 1 << 20;

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
}

impl Default for LogOptions {
    fn default() -> Self {
        LogOptions { segment_bytes: 64 << 20, sync: true }
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
    frame: Vec<u8>,
    next_lsn: u64,
    /// Active-segment offset below which the page cache has been dropped.
    dropped_upto: u64,
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
            let end = scan_segment(&file, id, file_len, &mut payload, |lsn, ops| {
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
            drop_cache(&file, 0, end);
            segs.push(Segment { id, file, len: end });
        }
        let mut store = LogStore {
            dir: dir.to_path_buf(),
            opts,
            segs,
            frame: Vec::new(),
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
        let mut row_offsets = Vec::new();
        encode_frame(&mut self.frame, self.next_lsn, ops, &mut row_offsets)?;
        let sync = self.opts.sync;
        let seg = self.segs.last_mut().expect("log always has an active segment");
        let start = seg.len;
        if let Err(e) = write_frame(&seg.file, &self.frame, start, sync) {
            // Cut off whatever part of the frame reached the file so the next
            // frame does not land behind garbage.
            let _ = seg.file.set_len(start);
            return Err(e.into());
        }
        seg.len = start + self.frame.len() as u64;
        self.next_lsn += 1;
        let seg_id = seg.id;
        let locs = row_offsets.iter().map(|&(off, len)| RowLoc { seg: seg_id, off: start + off, len }).collect();

        // The written pages are durable and will be read back only through
        // our own caches, so drop them from the page cache (it counts toward
        // the container's memory). Keep the partial last page: the next
        // append writes into it and would otherwise have to read it back.
        let keep_from = seg.len & !(PAGE - 1);
        if keep_from > self.dropped_upto {
            drop_cache(&seg.file, self.dropped_upto, keep_from - self.dropped_upto);
            self.dropped_upto = keep_from;
        }
        if seg.len >= self.opts.segment_bytes {
            self.new_segment(seg_id + 1)?;
        }
        Ok(locs)
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

    /// Streams every committed frame again, oldest first, dropping each
    /// segment from the page cache once read. For scans over tables too large
    /// to keep in memory.
    pub fn scan(&self, mut f: impl FnMut(u64, &[OpView<'_>]) -> DbResult<()>) -> DbResult<()> {
        let mut payload = Vec::new();
        for seg in &self.segs {
            let end = scan_segment(&seg.file, seg.id, seg.len, &mut payload, &mut f)?;
            if end != seg.len {
                bail!(Corrupt, "segment {} changed under a scan", seg.id);
            }
            if seg.id != self.active_id() {
                drop_cache(&seg.file, 0, seg.len);
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
        let file = OpenOptions::new().read(true).write(true).create_new(true).open(segment_path(&self.dir, id))?;
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

fn write_frame(file: &File, frame: &[u8], at: u64, sync: bool) -> io::Result<()> {
    file.write_all_at(frame, at)?;
    if sync {
        file.sync_data()?;
    }
    Ok(())
}

/// Encodes `ops` as one frame into `buf`, recording `(offset, len)` of every
/// `Put` row body relative to the frame start.
fn encode_frame(buf: &mut Vec<u8>, lsn: u64, ops: &[Op], row_offsets: &mut Vec<(u64, u32)>) -> DbResult<()> {
    buf.clear();
    buf.extend_from_slice(&[0u8; FRAME_HEADER as usize]);
    codec::put_u64(buf, lsn);
    codec::put_varint(buf, ops.len() as u64);
    for op in ops {
        match op {
            Op::CreateTable(def) => {
                buf.push(OP_CREATE_TABLE);
                def.encode(buf);
            }
            Op::DropTable { table } => {
                buf.push(OP_DROP_TABLE);
                codec::put_u32(buf, *table);
            }
            Op::CreateIndex { table, name } => {
                buf.push(OP_CREATE_INDEX);
                codec::put_u32(buf, *table);
                codec::put_str(buf, name);
            }
            Op::Put { table, pk, row } => {
                let len = u32::try_from(row.len()).map_err(|_| db_err!(Internal, "row of {} bytes is too large", row.len()))?;
                buf.push(OP_PUT);
                codec::put_u32(buf, *table);
                codec::put_i64(buf, *pk);
                codec::put_varint(buf, row.len() as u64);
                row_offsets.push((buf.len() as u64, len));
                buf.extend_from_slice(row);
            }
            Op::Delete { table, pk } => {
                buf.push(OP_DELETE);
                codec::put_u32(buf, *table);
                codec::put_i64(buf, *pk);
            }
        }
    }
    let payload_len = buf.len() - FRAME_HEADER as usize;
    let len = u32::try_from(payload_len).map_err(|_| db_err!(Internal, "transaction of {} bytes is too large", payload_len))?;
    let crc = crc32c(&buf[FRAME_HEADER as usize..]);
    buf[0..4].copy_from_slice(&len.to_le_bytes());
    buf[4..8].copy_from_slice(&crc.to_le_bytes());
    Ok(())
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
/// decode is real corruption and is an error.
fn scan_segment(
    file: &File,
    seg: u32,
    limit: u64,
    payload: &mut Vec<u8>,
    mut f: impl FnMut(u64, &[OpView<'_>]) -> DbResult<()>,
) -> DbResult<u64> {
    let mut reader = BufReader::with_capacity(SCAN_BUFFER, PosReader { file, pos: 0 });
    let mut off = 0u64;
    while off + FRAME_HEADER <= limit {
        let mut header = [0u8; FRAME_HEADER as usize];
        reader.read_exact(&mut header)?;
        let len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as u64;
        let crc = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
        if len == 0 || off + FRAME_HEADER + len > limit {
            break;
        }
        payload.resize(len as usize, 0);
        reader.read_exact(payload)?;
        if crc32c(payload) != crc {
            break;
        }
        let (lsn, ops) = decode_frame(payload, seg, off + FRAME_HEADER)?;
        f(lsn, &ops)?;
        off += FRAME_HEADER + len;
    }
    Ok(off)
}

/// `Read` over a shared `File` using positional reads, so scans never move
/// the file cursor.
struct PosReader<'a> {
    file: &'a File,
    pos: u64,
}

impl Read for PosReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
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
        LogOptions { segment_bytes: 64 << 20, sync: false }
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
        let o = LogOptions { segment_bytes: 200, sync: false };
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
        let o = LogOptions { segment_bytes: 100, sync: false };
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
}
