//! Making it survive: an append-only log and a snapshot.
//!
//! Phase 3 of `PLAN.md`. A database on disk is two files in a directory.
//! `snapshot` holds everything as of some moment, and `log` holds every
//! commit since. Opening reads the snapshot and then replays the log.
//!
//! Every record carries its length and a checksum, so a half-written
//! record at the end of the log, which is exactly what a crash leaves
//! behind, is recognised and cut off rather than believed.
//!
//! Nothing here runs on a timer. A snapshot happens when the log has
//! grown past a size, checked after a commit, so an open database that
//! nobody is using makes no syscalls at all.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::{Column, Error, Kind, Result, Row, Value};

/// How hard a commit tries to survive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Durability {
    /// Every commit is forced to the disk before it returns. A power cut
    /// loses nothing. It costs one fsync a commit, which on a laptop is
    /// around 400 microseconds.
    Full,
    /// Every commit is written, and the operating system flushes it when
    /// it sees fit. A crashed program loses nothing; a power cut can
    /// lose the last few commits. This is the default, and it is about
    /// ten times faster.
    Normal,
}

impl Default for Durability {
    fn default() -> Self { Durability::Normal }
}

/// How big the log gets before the whole database is written out fresh.
pub const SNAPSHOT_AT: u64 = 4 << 20;

/// The first claim, and the biggest one.
///
/// Claiming a megabyte before the first commit makes opening a small
/// database cost a megabyte of writing, which is silly for a database
/// that holds ten rows. So the claim starts small and doubles, and a
/// database that keeps going settles at the large size.
const FIRST_CLAIM: u64 = 64 << 10;

/// How much log file is claimed at a time.
///
/// An append that makes a file longer changes the file's size, and on
/// ext4 that means the filesystem's own journal has to be written too,
/// which is a second trip to the disk on every commit. Claiming space
/// ahead of time and writing inside it keeps the size unchanged, so an
/// fsync only has the data to write.
const CLAIM: u64 = 1 << 20;

/// A record this big or bigger is written straight out.
///
/// Claiming space costs a write of zeros the same size as the space, and
/// that only pays when many small commits would each have lengthened the
/// file. One big record lengthens it once, so the bookkeeping is spread
/// over megabytes and claiming ahead would only write everything twice.
const BIG: u64 = 1 << 18;

// ── Checksum ───────────────────────────────────────────────────────────

fn table() -> &'static [u32; 256] {
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, slot) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *slot = c;
        }
        t
    })
}

/// The usual CRC32, so a torn or rotted record is spotted rather than
/// read as data.
pub fn crc32(bytes: &[u8]) -> u32 {
    let t = table();
    let mut c = 0xFFFF_FFFFu32;
    for b in bytes {
        c = t[((c ^ *b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

// ── What a commit records ──────────────────────────────────────────────

/// One thing that happened. An update is written as the whole row, so
/// replaying is the same work whatever made the change.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    NewTable { name: String, key: String, columns: Vec<Column> },
    Put { table: u32, key: i64, row: Row },
    /// One column of one row. Cheaper to write than the whole row, which
    /// matters because an update is the commonest change there is.
    Set { table: u32, key: i64, col: u32, value: Value },
    Delete { table: u32, key: i64 },
    DropTable { name: String },
}

impl PartialEq for Column {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.kind == other.kind && self.null_ok == other.null_ok
    }
}

// ── Writing bytes ──────────────────────────────────────────────────────

fn put_u32(out: &mut Vec<u8>, n: u32) { out.extend_from_slice(&n.to_le_bytes()); }
fn put_i64(out: &mut Vec<u8>, n: i64) { out.extend_from_slice(&n.to_le_bytes()); }

fn put_str(out: &mut Vec<u8>, s: &str) {
    put_u32(out, s.len() as u32);
    out.extend_from_slice(s.as_bytes());
}

fn put_value(out: &mut Vec<u8>, v: &Value) {
    match v {
        Value::Null => out.push(0),
        Value::Int(i) => { out.push(1); put_i64(out, *i); }
        Value::Real(r) => { out.push(2); out.extend_from_slice(&r.to_le_bytes()); }
        Value::Text(t) => { out.push(3); put_str(out, t); }
        Value::Blob(b) => {
            out.push(4);
            put_u32(out, b.len() as u32);
            out.extend_from_slice(b);
        }
    }
}

fn put_change(out: &mut Vec<u8>, c: &Change) {
    match c {
        Change::NewTable { name, key, columns } => {
            out.push(1);
            put_str(out, name);
            put_str(out, key);
            put_u32(out, columns.len() as u32);
            for col in columns {
                put_str(out, &col.name);
                out.push(match col.kind {
                    Kind::Int => 1,
                    Kind::Real => 2,
                    Kind::Text => 3,
                    Kind::Blob => 4,
                });
                out.push(col.null_ok as u8);
            }
        }
        Change::Put { table, key, row } => {
            out.push(2);
            put_u32(out, *table);
            put_i64(out, *key);
            put_u32(out, row.len() as u32);
            for v in row { put_value(out, v); }
        }
        Change::Set { table, key, col, value } => {
            out.push(5);
            put_u32(out, *table);
            put_i64(out, *key);
            put_u32(out, *col);
            put_value(out, value);
        }
        Change::Delete { table, key } => {
            out.push(3);
            put_u32(out, *table);
            put_i64(out, *key);
        }
        Change::DropTable { name } => {
            out.push(4);
            put_str(out, name);
        }
    }
}

/// Room at the front of a commit buffer for its length, its checksum
/// and how many changes it holds. All three are only known at the end,
/// so the space is left and filled in then.
pub const HEADER: usize = 12;

/// Start building a commit in `buf`.
pub fn begin(buf: &mut Vec<u8>) {
    buf.clear();
    buf.extend_from_slice(&[0u8; HEADER]);
}

/// Close a commit off, so that `buf` is a record ready to append.
pub fn finish(buf: &mut Vec<u8>, count: u32) {
    buf[8..12].copy_from_slice(&count.to_le_bytes());
    let body_len = (buf.len() - 8) as u32;
    let crc = crc32(&buf[8..]);
    buf[0..4].copy_from_slice(&body_len.to_le_bytes());
    buf[4..8].copy_from_slice(&crc.to_le_bytes());
}

/// A whole row, written straight from the row rather than from a copy
/// of it. Making a copy of every row on the way to the log was worth
/// more than a microsecond an insert.
pub fn put_row(buf: &mut Vec<u8>, table: u32, key: i64, row: &[Value]) {
    buf.push(2);
    put_u32(buf, table);
    put_i64(buf, key);
    put_u32(buf, row.len() as u32);
    for v in row { put_value(buf, v); }
}

/// One column of one row.
pub fn put_set(buf: &mut Vec<u8>, table: u32, key: i64, col: u32, value: &Value) {
    buf.push(5);
    put_u32(buf, table);
    put_i64(buf, key);
    put_u32(buf, col);
    put_value(buf, value);
}

pub fn put_delete(buf: &mut Vec<u8>, table: u32, key: i64) {
    buf.push(3);
    put_u32(buf, table);
    put_i64(buf, key);
}

pub fn put_one(buf: &mut Vec<u8>, change: &Change) { put_change(buf, change); }

/// One commit, ready to append: its length, its checksum, then the
/// changes.
pub fn encode(changes: &[Change]) -> Vec<u8> {
    let mut body = Vec::with_capacity(64 * changes.len());
    put_u32(&mut body, changes.len() as u32);
    for c in changes { put_change(&mut body, c); }
    let mut out = Vec::with_capacity(body.len() + 8);
    put_u32(&mut out, body.len() as u32);
    put_u32(&mut out, crc32(&body));
    out.extend_from_slice(&body);
    out
}

// ── Reading bytes ──────────────────────────────────────────────────────

struct Reader<'a> { b: &'a [u8], at: usize }

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(n)?;
        let out = self.b.get(self.at..end)?;
        self.at = end;
        Some(out)
    }
    fn u8(&mut self) -> Option<u8> { Some(self.take(1)?[0]) }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn i64(&mut self) -> Option<i64> {
        Some(i64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn f64(&mut self) -> Option<f64> {
        Some(f64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn string(&mut self) -> Option<String> {
        let n = self.u32()? as usize;
        String::from_utf8(self.take(n)?.to_vec()).ok()
    }
    fn value(&mut self) -> Option<Value> {
        Some(match self.u8()? {
            0 => Value::Null,
            1 => Value::Int(self.i64()?),
            2 => Value::Real(self.f64()?),
            3 => Value::Text(self.string()?),
            4 => {
                let n = self.u32()? as usize;
                Value::Blob(self.take(n)?.to_vec())
            }
            _ => return None,
        })
    }
    fn change(&mut self) -> Option<Change> {
        Some(match self.u8()? {
            1 => {
                let name = self.string()?;
                let key = self.string()?;
                let n = self.u32()? as usize;
                let mut columns = Vec::with_capacity(n);
                for _ in 0..n {
                    let cname = self.string()?;
                    let kind = match self.u8()? {
                        1 => Kind::Int,
                        2 => Kind::Real,
                        3 => Kind::Text,
                        4 => Kind::Blob,
                        _ => return None,
                    };
                    let null_ok = self.u8()? != 0;
                    columns.push(Column { name: cname, kind, null_ok });
                }
                Change::NewTable { name, key, columns }
            }
            2 => {
                let table = self.u32()?;
                let key = self.i64()?;
                let n = self.u32()? as usize;
                let mut row = Vec::with_capacity(n);
                for _ in 0..n { row.push(self.value()?); }
                Change::Put { table, key, row }
            }
            3 => Change::Delete { table: self.u32()?, key: self.i64()? },
            5 => Change::Set {
                table: self.u32()?,
                key: self.i64()?,
                col: self.u32()?,
                value: self.value()?,
            },
            4 => Change::DropTable { name: self.string()? },
            _ => return None,
        })
    }
}

/// Read one commit from the front of `bytes`. Comes back with the
/// changes and how many bytes they took, or None when what is there is
/// short or damaged, which is what the end of a crashed log looks like.
pub fn decode(bytes: &[u8]) -> Option<(Vec<Change>, usize)> {
    if bytes.len() < 8 { return None; }
    let len = u32::from_le_bytes(bytes[0..4].try_into().ok()?) as usize;
    let want = u32::from_le_bytes(bytes[4..8].try_into().ok()?);
    let body = bytes.get(8..8 + len)?;
    if crc32(body) != want { return None; }
    let mut r = Reader { b: body, at: 0 };
    let n = r.u32()? as usize;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n { out.push(r.change()?); }
    if r.at != body.len() { return None; }
    Some((out, 8 + len))
}

/// Every commit in a stretch of bytes, and how far the good part went.
/// A crash leaves a part-written record at the end, and that is where
/// reading stops.
pub fn decode_all(bytes: &[u8]) -> (Vec<Vec<Change>>, usize) {
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        match decode(&bytes[at..]) {
            Some((changes, used)) => { out.push(changes); at += used; }
            None => break,
        }
    }
    (out, at)
}

// ── The files ──────────────────────────────────────────────────────────

fn io(e: std::io::Error) -> Error { Error::Disk(e.to_string()) }

/// The log and the snapshot that live in one directory.
#[derive(Debug)]
pub struct Store {
    dir: PathBuf,
    log: File,
    /// Bytes in the log, kept as we go so nothing has to ask the
    /// filesystem after every commit.
    log_len: u64,
    /// How long the file is, which is usually more than the log, because
    /// space is claimed ahead of the writing.
    claimed: u64,
    /// How much to claim next time, doubling towards `CLAIM`.
    claim_size: u64,
    /// Changes written since the last snapshot. A snapshot is only worth
    /// taking when the log holds a good deal more than the database
    /// does, which is what this counts towards.
    records: u64,
    pub durability: Durability,
    /// True when something has been written but not forced to the disk.
    unsynced: bool,
}

impl Store {
    pub fn log_path(dir: &Path) -> PathBuf { dir.join("log") }
    pub fn snapshot_path(dir: &Path) -> PathBuf { dir.join("snapshot") }

    /// Open a database directory, making it if it is not there.
    /// Everything that was committed comes back as a list of commits to
    /// replay, oldest first.
    pub fn open(dir: &Path, durability: Durability) -> Result<(Store, Vec<Vec<Change>>)> {
        std::fs::create_dir_all(dir).map_err(io)?;
        let mut commits = Vec::new();

        // The snapshot first: everything as of the moment it was taken.
        let snap = Self::snapshot_path(dir);
        if snap.exists() {
            let bytes = std::fs::read(&snap).map_err(io)?;
            let (mut found, used) = decode_all(&bytes);
            if used != bytes.len() {
                // A snapshot is written whole and renamed into place, so
                // a short one means the file was damaged after the fact.
                return Err(Error::Disk("the snapshot is damaged".into()));
            }
            commits.append(&mut found);
        }

        // Then the log, up to the last record that is whole.
        let path = Self::log_path(dir);
        let mut log = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(io)?;
        let mut bytes = Vec::new();
        log.read_to_end(&mut bytes).map_err(io)?;
        let (mut found, good) = decode_all(&bytes);
        // Only the log counts towards the next snapshot. The records in
        // the snapshot are already compacted, so counting them would
        // make the log look fuller than it is.
        let counted = found.iter().map(|c| c.len() as u64).sum();
        commits.append(&mut found);
        if good as u64 != bytes.len() as u64 {
            // Cut the half-written tail a crash left behind.
            log.set_len(good as u64).map_err(io)?;
            log.sync_data().map_err(io)?;
        }
        log.seek(SeekFrom::Start(good as u64)).map_err(io)?;

        Ok((
            Store {
                dir: dir.to_path_buf(),
                log,
                log_len: good as u64,
                claimed: good as u64,
                claim_size: FIRST_CLAIM,
                records: counted,
                durability,
                unsynced: false,
            },
            commits,
        ))
    }

    /// Write one commit. In FULL it is on the disk when this returns; in
    /// NORMAL it has reached the operating system, which is enough to
    /// survive the program dying but not the power going.
    pub fn commit(&mut self, changes: &[Change]) -> Result<()> {
        if changes.is_empty() { return Ok(()); }
        let bytes = encode(changes);
        self.records += changes.len() as u64;
        self.write(&bytes)
    }

    /// Write a commit that has already been built up byte by byte.
    pub fn commit_bytes(&mut self, bytes: &[u8], count: u32) -> Result<()> {
        if bytes.is_empty() { return Ok(()); }
        self.records += count as u64;
        self.write(bytes)
    }

    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        let len = bytes.len() as u64;
        if len < BIG { self.claim(len)?; }
        self.log.write_all(bytes).map_err(io)?;
        self.log_len += len;
        // A big record put its own blocks down as it was written.
        self.claimed = self.claimed.max(self.log_len);
        match self.durability {
            Durability::Full => { self.log.sync_data().map_err(io)?; self.unsynced = false; }
            Durability::Normal => self.unsynced = true,
        }
        Ok(())
    }

    /// Make sure there is room for `more` bytes without the file having
    /// to grow while a commit is being written.
    fn claim(&mut self, more: u64) -> Result<()> {
        if self.log_len + more <= self.claimed { return Ok(()); }
        let want = (self.log_len + more).max(self.claimed + self.claim_size);
        self.claim_size = (self.claim_size * 2).min(CLAIM);
        let here = self.log.stream_position().map_err(io)?;
        // Real zeros, not a shorter file made longer. Making a file
        // longer leaves the new part unwritten, and the first write into
        // it costs the same filesystem bookkeeping this is trying to
        // avoid. Writing zeros puts the blocks there for good.
        let zeros = vec![0u8; (want - self.claimed) as usize];
        self.log.seek(SeekFrom::Start(self.claimed)).map_err(io)?;
        self.log.write_all(&zeros).map_err(io)?;
        self.log.sync_all().map_err(io)?;
        self.log.seek(SeekFrom::Start(here)).map_err(io)?;
        self.claimed = want;
        Ok(())
    }

    /// True when writing the database out fresh would make the log
    /// shorter. Asked after a commit, never on a clock.
    ///
    /// Size alone is the wrong question. A hundred thousand rows loaded
    /// once fill the log with one record each, and a snapshot of them
    /// would be the same size, so it would write everything twice for
    /// nothing. What makes a snapshot pay is records the database no
    /// longer needs: rows written over, rows deleted.
    pub fn wants_snapshot(&self, live_rows: u64) -> bool {
        self.log_len >= SNAPSHOT_AT && self.records > 2 * live_rows.max(1)
    }

    pub fn log_len(&self) -> u64 { self.log_len }

    /// Force everything written so far onto the disk.
    pub fn flush(&mut self) -> Result<()> {
        if self.unsynced {
            self.log.sync_data().map_err(io)?;
            self.unsynced = false;
        }
        Ok(())
    }

    /// Write the whole database out fresh and start the log again.
    ///
    /// The new snapshot goes to a temporary name, is forced to the disk,
    /// and only then takes the place of the old one. Until the rename
    /// there are two good files; after it there is one; at no point is
    /// there none.
    pub fn snapshot(&mut self, changes: &[Change]) -> Result<()> {
        let tmp = self.dir.join("snapshot.new");
        {
            let mut f = File::create(&tmp).map_err(io)?;
            f.write_all(&encode(changes)).map_err(io)?;
            f.sync_all().map_err(io)?;
        }
        std::fs::rename(&tmp, Self::snapshot_path(&self.dir)).map_err(io)?;
        // The rename itself has to reach the disk, or a power cut could
        // leave the old snapshot beside an emptied log.
        if let Ok(d) = File::open(&self.dir) { let _ = d.sync_all(); }
        self.log.set_len(0).map_err(io)?;
        self.log.seek(SeekFrom::Start(0)).map_err(io)?;
        self.log.sync_data().map_err(io)?;
        self.log_len = 0;
        self.claimed = 0;
        self.claim_size = FIRST_CLAIM;
        self.records = 0;
        self.unsynced = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn some_changes() -> Vec<Change> {
        vec![
            Change::NewTable {
                name: "kv".into(),
                key: "id".into(),
                columns: vec![
                    Column { name: "a".into(), kind: Kind::Int, null_ok: true },
                    Column { name: "c".into(), kind: Kind::Text, null_ok: false },
                ],
            },
            Change::Put {
                table: 0,
                key: 7,
                row: vec![Value::Int(-3), Value::Text("it's".into())],
            },
            Change::Put {
                table: 0,
                key: 8,
                row: vec![Value::Null, Value::Blob(vec![0, 1, 255])],
            },
            Change::Put { table: 0, key: 9, row: vec![Value::Real(-1.5), Value::Text(String::new())] },
            Change::Set { table: 0, key: 8, col: 0, value: Value::Int(4) },
            Change::Delete { table: 0, key: 7 },
            Change::DropTable { name: "old".into() },
        ]
    }

    #[test]
    fn a_commit_survives_the_round_trip() {
        let want = some_changes();
        let bytes = encode(&want);
        let (got, used) = decode(&bytes).expect("it should read back");
        assert_eq!(got, want);
        assert_eq!(used, bytes.len());
    }

    #[test]
    fn a_commit_built_byte_by_byte_reads_the_same() {
        let mut buf = Vec::new();
        begin(&mut buf);
        put_row(&mut buf, 3, 11, &[Value::Int(1), Value::Text("x".into())]);
        put_set(&mut buf, 3, 11, 0, &Value::Real(2.5));
        put_delete(&mut buf, 3, 12);
        finish(&mut buf, 3);
        let (got, used) = decode(&buf).expect("it should read back");
        assert_eq!(used, buf.len());
        assert_eq!(
            got,
            vec![
                Change::Put { table: 3, key: 11, row: vec![Value::Int(1), Value::Text("x".into())] },
                Change::Set { table: 3, key: 11, col: 0, value: Value::Real(2.5) },
                Change::Delete { table: 3, key: 12 },
            ]
        );
    }

    #[test]
    fn several_commits_read_back_in_order() {
        let mut bytes = Vec::new();
        for i in 0..5i64 {
            bytes.extend(encode(&[Change::Delete { table: 0, key: i }]));
        }
        let (commits, used) = decode_all(&bytes);
        assert_eq!(used, bytes.len());
        assert_eq!(commits.len(), 5);
        assert_eq!(commits[3], vec![Change::Delete { table: 0, key: 3 }]);
    }

    #[test]
    fn a_half_written_record_is_left_out() {
        let good = encode(&[Change::Delete { table: 0, key: 1 }]);
        let torn = encode(&[Change::Delete { table: 0, key: 2 }]);
        for cut in 1..torn.len() {
            let mut bytes = good.clone();
            bytes.extend_from_slice(&torn[..cut]);
            let (commits, used) = decode_all(&bytes);
            assert_eq!(commits.len(), 1, "a record cut at {cut} was believed");
            assert_eq!(used, good.len());
        }
    }

    #[test]
    fn one_wrong_bit_is_caught() {
        let good = encode(&some_changes());
        for bit in 0..64 {
            let mut bytes = good.clone();
            let byte = 8 + bit / 8;
            bytes[byte] ^= 1 << (bit % 8);
            assert!(decode(&bytes).is_none(), "a flipped bit at {bit} got through");
        }
    }

    #[test]
    fn rubbish_never_panics() {
        let mut rng: u64 = 12345;
        for _ in 0..2000 {
            let mut bytes = Vec::new();
            for _ in 0..(rng % 64) {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                bytes.push(rng as u8);
            }
            let _ = decode_all(&bytes);
        }
    }

    #[test]
    fn the_checksum_is_the_usual_one() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn a_store_gives_back_what_was_committed() {
        let dir = std::env::temp_dir().join(format!("ferrite-log-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        {
            let (mut store, commits) = Store::open(&dir, Durability::Full).unwrap();
            assert!(commits.is_empty());
            store.commit(&some_changes()).unwrap();
            store.commit(&[Change::Delete { table: 0, key: 9 }]).unwrap();
        }
        let (_store, commits) = Store::open(&dir, Durability::Full).unwrap();
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0], some_changes());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_torn_log_is_cut_back_on_open() {
        let dir = std::env::temp_dir().join(format!("ferrite-torn-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        {
            let (mut store, _) = Store::open(&dir, Durability::Full).unwrap();
            store.commit(&[Change::Delete { table: 0, key: 1 }]).unwrap();
        }
        // A crash in the middle of the next record.
        {
            let mut f = OpenOptions::new().append(true).open(Store::log_path(&dir)).unwrap();
            let half = encode(&[Change::Delete { table: 0, key: 2 }]);
            f.write_all(&half[..half.len() - 3]).unwrap();
        }
        let (store, commits) = Store::open(&dir, Durability::Full).unwrap();
        assert_eq!(commits.len(), 1);
        // And the file is now the length of the good part, so the next
        // commit lands on clean ground.
        assert_eq!(store.log_len(), std::fs::metadata(Store::log_path(&dir)).unwrap().len());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_snapshot_waits_for_something_to_compact() {
        let dir = std::env::temp_dir().join(format!("ferrite-wants-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (mut store, _) = Store::open(&dir, Durability::Normal).unwrap();
        // One record a row is nothing to compact, however big it gets.
        store.records = 100_000;
        store.log_len = SNAPSHOT_AT + 1;
        assert!(!store.wants_snapshot(100_000));
        // Three records a row is mostly dead weight.
        store.records = 300_000;
        assert!(store.wants_snapshot(100_000));
        // And a short log is left alone whatever is in it.
        store.log_len = 10;
        assert!(!store.wants_snapshot(1));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn claimed_space_reads_back_as_the_end_of_the_log() {
        let dir = std::env::temp_dir().join(format!("ferrite-claim-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        {
            let (mut store, _) = Store::open(&dir, Durability::Full).unwrap();
            store.commit(&some_changes()).unwrap();
            // The file is far longer than what has been written.
            let on_disk = std::fs::metadata(Store::log_path(&dir)).unwrap().len();
            assert!(on_disk > store.log_len(), "space should have been claimed");
        }
        // The zeros past the end stop the reading, rather than being
        // read as a record.
        let (store, commits) = Store::open(&dir, Durability::Full).unwrap();
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0], some_changes());
        assert_eq!(store.log_len(), std::fs::metadata(Store::log_path(&dir)).unwrap().len());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_snapshot_replaces_the_log() {
        let dir = std::env::temp_dir().join(format!("ferrite-snap-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        {
            let (mut store, _) = Store::open(&dir, Durability::Normal).unwrap();
            for i in 0..50i64 {
                store.commit(&[Change::Delete { table: 0, key: i }]).unwrap();
            }
            store.snapshot(&some_changes()).unwrap();
            assert_eq!(store.log_len(), 0);
            store.commit(&[Change::Delete { table: 0, key: 99 }]).unwrap();
        }
        let (_store, commits) = Store::open(&dir, Durability::Normal).unwrap();
        assert_eq!(commits.len(), 2, "the snapshot and the one commit after it");
        assert_eq!(commits[0], some_changes());
        assert_eq!(commits[1], vec![Change::Delete { table: 0, key: 99 }]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
