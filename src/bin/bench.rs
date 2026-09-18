//! The bench that sets ferrite's targets.
//!
//! Phase 0 of `PLAN.md`: before any engine code, measure SQLite doing
//! the work ferrite means to do, so every later phase has a number to
//! beat rather than a hope.
//!
//! Four workloads, the same on every engine: a bulk load, all reads, one
//! update in twenty, and one update in two. Each one reports how many
//! operations a second, how long the middling operation took, how long
//! the slow one in a hundred took, and how much CPU an operation burns.
//!
//! SQLite gets its best settings: WAL and a statement prepared once and
//! run many times. Both durability modes are measured, because that
//! choice is worth more than any amount of tuning.
//!
//! Run it with:
//!
//! ```text
//! cargo run --release --features bench --bin ferrite-bench
//! ```

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// How many rows the table holds before the read workloads start.
const ROWS: u64 = 100_000;
/// Operations in the read-only workload.
const READ_OPS: u64 = 200_000;
/// Operations in each mixed workload. Every update commits on its own,
/// which in FULL mode means an fsync, so this stays small.
const MIXED_OPS: u64 = 20_000;

// ── Measuring ──────────────────────────────────────────────────────────

/// Nanoseconds this process has spent on a CPU. The kernel keeps the
/// count, so it excludes time we spent waiting for the disk, which is
/// the whole point of measuring it next to wall clock time.
fn cpu_ns() -> u64 {
    std::fs::read_to_string("/proc/self/schedstat")
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse().ok())
        .unwrap_or(0)
}

/// What one workload did.
struct Run {
    what: &'static str,
    ops: u64,
    wall_ns: u64,
    cpu_ns: u64,
    /// Nanoseconds each operation took, for the percentiles.
    each: Vec<u64>,
}

impl Run {
    fn per_sec(&self) -> f64 { self.ops as f64 / (self.wall_ns as f64 / 1e9) }
    fn cpu_us_per_op(&self) -> f64 { self.cpu_ns as f64 / 1000.0 / self.ops as f64 }

    /// The operation at a given place in the sorted list, in microseconds.
    fn at(&mut self, per_mille: u64) -> f64 {
        if self.each.is_empty() { return 0.0; }
        self.each.sort_unstable();
        let i = ((self.each.len() as u64 - 1) * per_mille / 1000) as usize;
        self.each[i] as f64 / 1000.0
    }
}

/// A repeatable stream of numbers. A real random source would make two
/// runs incomparable, and that is the one thing a bench must not do.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng { Rng(seed | 1) }
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    /// A number below `n`.
    fn below(&mut self, n: u64) -> u64 { self.next() % n }
}

// ── What every engine has to do ────────────────────────────────────────

/// The bench drives a whole workload through one call, not one operation
/// through one call. That lets an engine prepare its statements once, the
/// way a real caller would, and keeps the harness out of the measurement.
trait Engine {
    fn name(&self) -> String;
    /// Put `rows` rows in, all in one transaction, and give back how long
    /// each row took.
    fn load(&mut self, rows: u64) -> Vec<u64>;
    /// Look up `ops` rows by key.
    fn reads(&mut self, ops: u64, rng: &mut Rng) -> Vec<u64>;
    /// Read, except that `writes` in a thousand are an update that
    /// commits on its own.
    fn mixed(&mut self, ops: u64, writes: u64, rng: &mut Rng) -> Vec<u64>;
}

fn measure(engine: &mut dyn Engine, what: &'static str, f: impl FnOnce(&mut dyn Engine) -> Vec<u64>) -> Run {
    let cpu0 = cpu_ns();
    let t0 = Instant::now();
    let each = f(engine);
    let wall_ns = t0.elapsed().as_nanos() as u64;
    let cpu_ns = cpu_ns().saturating_sub(cpu0);
    Run { what, ops: each.len() as u64, wall_ns, cpu_ns, each }
}

// ── SQLite ─────────────────────────────────────────────────────────────

#[cfg(feature = "bench")]
mod sqlite {
    use super::*;
    use rusqlite::{params, Connection};

    pub struct Sqlite {
        conn: Connection,
        rows: u64,
        mode: &'static str,
    }

    impl Sqlite {
        pub fn open(path: &Path, mode: &'static str) -> Sqlite {
            let _ = std::fs::remove_file(path);
            let _ = std::fs::remove_file(path.with_extension("db-wal"));
            let _ = std::fs::remove_file(path.with_extension("db-shm"));
            let conn = Connection::open(path).expect("open");
            let journal: String = conn
                .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
                .expect("wal");
            assert_eq!(journal, "wal", "SQLite refused WAL mode");
            conn.execute_batch(&format!("PRAGMA synchronous={mode};"))
                .expect("synchronous");
            conn.execute_batch(
                "CREATE TABLE kv (
                     id INTEGER PRIMARY KEY,
                     a  INTEGER NOT NULL,
                     b  REAL    NOT NULL,
                     c  TEXT    NOT NULL
                 );",
            )
            .expect("create");
            Sqlite { conn, rows: 0, mode }
        }
    }

    impl Engine for Sqlite {
        fn name(&self) -> String {
            let v: String = self
                .conn
                .query_row("SELECT sqlite_version()", [], |r| r.get(0))
                .unwrap_or_else(|_| "?".into());
            format!("SQLite {v}, WAL, synchronous={}", self.mode)
        }

        fn load(&mut self, rows: u64) -> Vec<u64> {
            let mut each = Vec::with_capacity(rows as usize);
            self.conn.execute_batch("BEGIN").expect("begin");
            {
                let mut put = self
                    .conn
                    .prepare("INSERT INTO kv (id, a, b, c) VALUES (?1, ?2, ?3, ?4)")
                    .expect("prepare insert");
                for i in 0..rows {
                    let text = format!("row {i}");
                    let t0 = Instant::now();
                    put.execute(params![i as i64, i as i64, i as f64 * 1.5, text])
                        .expect("insert");
                    each.push(t0.elapsed().as_nanos() as u64);
                }
            }
            let t0 = Instant::now();
            self.conn.execute_batch("COMMIT").expect("commit");
            // The commit is one operation's worth of work shared by every
            // row, so it belongs to the last row rather than to none.
            if let Some(last) = each.last_mut() {
                *last += t0.elapsed().as_nanos() as u64;
            }
            self.rows = rows;
            each
        }

        fn reads(&mut self, ops: u64, rng: &mut Rng) -> Vec<u64> {
            let mut each = Vec::with_capacity(ops as usize);
            let mut get = self
                .conn
                .prepare("SELECT a FROM kv WHERE id = ?1")
                .expect("prepare select");
            let rows = self.rows;
            for _ in 0..ops {
                let key = rng.below(rows) as i64;
                let t0 = Instant::now();
                let got: i64 = get.query_row(params![key], |r| r.get(0)).expect("select");
                each.push(t0.elapsed().as_nanos() as u64);
                debug_assert_eq!(got, key);
            }
            each
        }

        fn mixed(&mut self, ops: u64, writes: u64, rng: &mut Rng) -> Vec<u64> {
            let mut each = Vec::with_capacity(ops as usize);
            let mut get = self
                .conn
                .prepare("SELECT a FROM kv WHERE id = ?1")
                .expect("prepare select");
            let mut set = self
                .conn
                .prepare("UPDATE kv SET a = ?2 WHERE id = ?1")
                .expect("prepare update");
            let rows = self.rows;
            for _ in 0..ops {
                let key = rng.below(rows) as i64;
                let write = rng.below(1000) < writes;
                let t0 = Instant::now();
                if write {
                    set.execute(params![key, key + 1]).expect("update");
                } else {
                    let _: i64 = get.query_row(params![key], |r| r.get(0)).expect("select");
                }
                each.push(t0.elapsed().as_nanos() as u64);
            }
            each
        }
    }
}

// ── ferrite ────────────────────────────────────────────────────────────

mod engine {
    use super::*;
    use ferrite::{Db, Durability, Kind, Statement, TableId, Value};

    pub struct Ferrite {
        db: Db,
        kv: TableId,
        rows: u64,
    }

    impl Ferrite {
        pub fn open() -> Ferrite {
            let mut db = Db::new();
            let kv = db
                .create_table("kv", &[("a", Kind::Int), ("b", Kind::Real), ("c", Kind::Text)])
                .expect("create");
            Ferrite { db, kv, rows: 0 }
        }
    }

    /// The same engine reached through SQL, with every statement
    /// prepared once. The phase 2 gate is that this row costs within 10%
    /// of the plain Rust one.
    pub struct FerriteSql {
        db: Db,
        rows: u64,
        put: Statement,
        get: Statement,
        set: Statement,
        on_disk: bool,
    }

    impl FerriteSql {
        pub fn open() -> FerriteSql { Self::make(None) }

        /// The same thing with its files on disk, so that it can be put
        /// beside SQLite in the same durability mode.
        pub fn on_disk(dir: &std::path::Path, mode: Durability) -> FerriteSql {
            let _ = std::fs::remove_dir_all(dir);
            Self::make(Some((dir.to_path_buf(), mode)))
        }

        fn make(files: Option<(std::path::PathBuf, Durability)>) -> FerriteSql {
            let mut db = match &files {
                Some((dir, mode)) => Db::open_with(dir, *mode).expect("open"),
                None => Db::new(),
            };
            db.execute(
                "CREATE TABLE kv (id INTEGER PRIMARY KEY, a INTEGER, b REAL, c TEXT)",
                &[],
            )
            .expect("create");
            let put = db.prepare("INSERT INTO kv (id, a, b, c) VALUES (?1, ?2, ?3, ?4)").unwrap();
            let get = db.prepare("SELECT a FROM kv WHERE id = ?1").unwrap();
            let set = db.prepare("UPDATE kv SET a = ?2 WHERE id = ?1").unwrap();
            assert!(get.is_point_lookup(), "the lookup did not plan as a point lookup");
            let on_disk = files.is_some();
            FerriteSql { db, rows: 0, put, get, set, on_disk }
        }
    }

    impl Engine for FerriteSql {
        fn name(&self) -> String {
            match self.db.durability() {
                Some(d) => format!(
                    "ferrite {}, on disk, {}",
                    env!("CARGO_PKG_VERSION"),
                    if d == Durability::Full { "synchronous=FULL" } else { "synchronous=NORMAL" }
                ),
                None => format!(
                    "ferrite {}, through SQL, in memory, statements prepared once",
                    env!("CARGO_PKG_VERSION")
                ),
            }
        }

        fn load(&mut self, rows: u64) -> Vec<u64> {
            let mut each = Vec::with_capacity(rows as usize);
            // A bulk load is one transaction, so the whole batch shares
            // one commit and one fsync. That is what makes it bulk.
            if self.on_disk { self.db.execute("BEGIN", &[]).expect("begin"); }
            for i in 0..rows {
                let t0 = Instant::now();
                self.put
                    .run(
                        &mut self.db,
                        &[
                            Value::Int(i as i64),
                            Value::Int(i as i64),
                            Value::Real(i as f64 * 1.5),
                            Value::Text(format!("row {i}")),
                        ],
                    )
                    .expect("insert");
                each.push(t0.elapsed().as_nanos() as u64);
            }
            if self.on_disk {
                let t0 = Instant::now();
                self.db.execute("COMMIT", &[]).expect("commit");
                if let Some(last) = each.last_mut() {
                    *last += t0.elapsed().as_nanos() as u64;
                }
            }
            self.rows = rows;
            each
        }

        fn reads(&mut self, ops: u64, rng: &mut Rng) -> Vec<u64> {
            let mut each = Vec::with_capacity(ops as usize);
            let rows = self.rows;
            for _ in 0..ops {
                let key = rng.below(rows) as i64;
                let t0 = Instant::now();
                let got = self
                    .get
                    .value(&self.db, &[Value::Int(key)])
                    .expect("select")
                    .and_then(|v| v.as_int())
                    .expect("a row");
                each.push(t0.elapsed().as_nanos() as u64);
                debug_assert_eq!(got, key);
            }
            each
        }

        fn mixed(&mut self, ops: u64, writes: u64, rng: &mut Rng) -> Vec<u64> {
            let mut each = Vec::with_capacity(ops as usize);
            let rows = self.rows;
            for _ in 0..ops {
                let key = rng.below(rows) as i64;
                let write = rng.below(1000) < writes;
                let t0 = Instant::now();
                if write {
                    self.set
                        .run(&mut self.db, &[Value::Int(key), Value::Int(key + 1)])
                        .expect("update");
                } else {
                    let _ = self.get.value(&self.db, &[Value::Int(key)]).expect("select");
                }
                each.push(t0.elapsed().as_nanos() as u64);
            }
            each
        }
    }

    impl Engine for Ferrite {
        fn name(&self) -> String {
            format!("ferrite {}, in memory, no durability yet", env!("CARGO_PKG_VERSION"))
        }

        fn load(&mut self, rows: u64) -> Vec<u64> {
            let mut each = Vec::with_capacity(rows as usize);
            for i in 0..rows {
                // The text is built inside the timed part, the way
                // SQLite's side builds it, so neither engine is handed a
                // string the other had to make.
                let t0 = Instant::now();
                let row = vec![
                    Value::Int(i as i64),
                    Value::Real(i as f64 * 1.5),
                    Value::Text(format!("row {i}")),
                ];
                self.db.insert(self.kv, i as i64, row).expect("insert");
                each.push(t0.elapsed().as_nanos() as u64);
            }
            self.rows = rows;
            each
        }

        fn reads(&mut self, ops: u64, rng: &mut Rng) -> Vec<u64> {
            let mut each = Vec::with_capacity(ops as usize);
            let t = self.db.table(self.kv);
            let rows = self.rows;
            for _ in 0..ops {
                let key = rng.below(rows) as i64;
                let t0 = Instant::now();
                let got = t.get_at(key, 0).and_then(|v| v.as_int()).expect("select");
                each.push(t0.elapsed().as_nanos() as u64);
                debug_assert_eq!(got, key);
            }
            each
        }

        fn mixed(&mut self, ops: u64, writes: u64, rng: &mut Rng) -> Vec<u64> {
            let mut each = Vec::with_capacity(ops as usize);
            let rows = self.rows;
            for _ in 0..ops {
                let key = rng.below(rows) as i64;
                let write = rng.below(1000) < writes;
                let t0 = Instant::now();
                if write {
                    self.db
                        .update(self.kv, key, 0, Value::Int(key + 1))
                        .expect("update");
                } else {
                    let _ = self.db.table(self.kv).get_at(key, 0).expect("select");
                }
                each.push(t0.elapsed().as_nanos() as u64);
            }
            each
        }
    }
}

// ── The phase 2 gate ───────────────────────────────────────────────────

/// What SQL costs over calling the engine directly.
///
/// Measuring the two in separate tables lets the machine drift between
/// them, and the answer wandered from 3% to 17% run to run. So both run
/// here in one loop, in alternating batches over the same rows and the
/// same keys, and each batch is timed whole to keep the clock out of the
/// per-lookup cost.
fn phase2_gate(rows: u64, batches: u64, per_batch: u64) -> (f64, f64) {
    use ferrite::{Db, Kind, Value};
    let mut db = Db::new();
    let kv = db
        .create_table("kv", &[("a", Kind::Int), ("b", Kind::Real), ("c", Kind::Text)])
        .unwrap();
    for i in 0..rows {
        db.insert(
            kv,
            i as i64,
            vec![Value::Int(i as i64), Value::Real(i as f64 * 1.5), Value::Text(format!("row {i}"))],
        )
        .unwrap();
    }
    let get = db.prepare("SELECT a FROM kv WHERE id = ?1").unwrap();
    assert!(get.is_point_lookup());

    let mut plain = Vec::with_capacity(batches as usize);
    let mut sql = Vec::with_capacity(batches as usize);
    for b in 0..batches {
        // Whichever batch runs second finds the B-tree already in cache,
        // and that was worth 10% on its own. So the two take turns going
        // first, and the advantage cancels out over the run.
        let plain_first = b % 2 == 0;
        let mut run_plain = |plain: &mut Vec<u64>| {
            let mut keys = Rng::new(100 + b);
            let t0 = Instant::now();
            let mut sum = 0i64;
            for _ in 0..per_batch {
                let key = keys.below(rows) as i64;
                sum += db.table(kv).get_at(key, 0).unwrap().as_int().unwrap();
            }
            plain.push(t0.elapsed().as_nanos() as u64 / per_batch);
            sum
        };
        let mut run_sql = |sql: &mut Vec<u64>| {
            let mut keys = Rng::new(100 + b);
            let t0 = Instant::now();
            let mut sum = 0i64;
            for _ in 0..per_batch {
                let key = keys.below(rows) as i64;
                sum += get.value(&db, &[Value::Int(key)]).unwrap().unwrap().as_int().unwrap();
            }
            sql.push(t0.elapsed().as_nanos() as u64 / per_batch);
            sum
        };
        let (a, b2) = if plain_first {
            let a = run_plain(&mut plain);
            (a, run_sql(&mut sql))
        } else {
            let b2 = run_sql(&mut sql);
            (run_plain(&mut plain), b2)
        };
        assert_eq!(a, b2, "the two paths gave different answers");
    }
    plain.sort_unstable();
    sql.sort_unstable();
    (plain[plain.len() / 2] as f64, sql[sql.len() / 2] as f64)
}

// ── The fsync floor ────────────────────────────────────────────────────

/// How long one small append takes to reach the disk for certain. Every
/// promise that a committed row survives a power cut costs this much, in
/// any engine, so it is the floor under the write numbers.
fn fsync_floor(dir: &Path, tries: u32) -> Vec<u64> {
    let path = dir.join("fsync-probe");
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)
        .expect("probe file");
    let block = vec![0u8; 4096];
    let mut each = Vec::with_capacity(tries as usize);
    for _ in 0..tries {
        let t0 = Instant::now();
        f.write_all(&block).expect("write");
        f.sync_data().expect("fsync");
        each.push(t0.elapsed().as_nanos() as u64);
    }
    drop(f);
    let _ = std::fs::remove_file(&path);
    each
}

// ── Reporting ──────────────────────────────────────────────────────────

fn thousands(n: f64) -> String {
    let s = format!("{:.0}", n);
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 { out.push(' '); }
        out.push(c);
    }
    out
}

/// Out of several goes at the same workload, the middling one. A single
/// disk stall can cost a run five seconds, so one go proves nothing; the
/// median is what the engine does when the disk is behaving.
fn median_run(mut goes: Vec<Run>) -> (Run, f64, f64) {
    goes.sort_by(|a, b| a.per_sec().partial_cmp(&b.per_sec()).unwrap());
    let (low, high) = (goes[0].per_sec(), goes[goes.len() - 1].per_sec());
    (goes.swap_remove(goes.len() / 2), low, high)
}

fn table(title: &str, runs: &mut [Run]) {
    println!("\n{title}");
    println!(
        "  {:<18}{:>12}{:>9}{:>9}{:>10}{:>11}{:>11}",
        "workload", "ops/s", "p50 \u{b5}s", "p99 \u{b5}s", "p99.9 \u{b5}s", "worst \u{b5}s", "CPU \u{b5}s/op"
    );
    for r in runs.iter_mut() {
        // The tail is the story on a write workload: a single stall can
        // hold more of the wall clock than every other operation together.
        let (p50, p99, p999, worst) = (r.at(500), r.at(990), r.at(999), r.at(1000));
        println!(
            "  {:<18}{:>12}{:>9.2}{:>9.2}{:>10.1}{:>11.1}{:>11.2}",
            r.what,
            thousands(r.per_sec()),
            p50,
            p99,
            p999,
            worst,
            r.cpu_us_per_op()
        );
    }
}

/// Which filesystem a path sits on, read out of the kernel's mount
/// table. The longest mount point that the path starts with wins.
fn fs_type(path: &Path) -> String {
    let table = match std::fs::read_to_string("/proc/self/mountinfo") {
        Ok(t) => t,
        Err(_) => return "unknown".into(),
    };
    let want = path.to_string_lossy();
    let mut best = ("".to_string(), "unknown".to_string());
    for line in table.lines() {
        // mountinfo: ... <mount point> ... - <fs type> <source> <options>
        let Some((left, right)) = line.split_once(" - ") else { continue };
        let Some(point) = left.split_whitespace().nth(4) else { continue };
        let Some(kind) = right.split_whitespace().next() else { continue };
        if want.starts_with(point) && point.len() >= best.0.len() {
            best = (point.to_string(), kind.to_string());
        }
    }
    best.1
}

/// Where the bench puts its files. It must be a real disk: on a RAM disk
/// fsync costs nothing, every write number comes out perfect, and the
/// whole exercise measures memory. That is how the first run of this
/// bench fooled itself, so it now refuses.
fn scratch() -> PathBuf {
    let base = std::env::var("FERRITE_BENCH_DIR").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        format!("{home}/.cache")
    });
    let dir = PathBuf::from(base).join("ferrite-bench");
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let kind = fs_type(&dir);
    if kind == "tmpfs" || kind == "ramfs" {
        eprintln!(
            "{} is a {kind}, which is memory, not a disk.\n\
             fsync there costs nothing and the write numbers would be a lie.\n\
             Point FERRITE_BENCH_DIR at a real disk.",
            dir.display()
        );
        std::process::exit(1);
    }
    println!("  working in {} (on {kind})", dir.display());
    dir
}

#[cfg(feature = "bench")]
fn main() {
    println!("ferrite bench, phase 0: the baseline to beat");
    println!("  {ROWS} rows, {READ_OPS} reads, {MIXED_OPS} mixed operations");
    let dir = scratch();

    let mut floor = Run {
        what: "one 4 KB fsync",
        ops: 0,
        wall_ns: 0,
        cpu_ns: 0,
        each: fsync_floor(&dir, 200),
    };
    floor.ops = floor.each.len() as u64;
    floor.wall_ns = floor.each.iter().sum();
    println!(
        "\nThe floor under every durable commit: one 4 KB append plus fsync\n  \
         p50 {:.0} µs, p99 {:.0} µs, over {} tries",
        floor.at(500),
        floor.at(990),
        floor.ops
    );

    let goes: usize = std::env::var("FERRITE_BENCH_GOES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3);
    println!("  {goes} goes at each workload, reporting the middling one");

    for mode in ["FULL", "NORMAL"] {
        let path = dir.join("bench.db");
        let mut title = String::new();
        let mut collected: Vec<Vec<Run>> = Vec::new();
        for go in 0..goes {
            let mut db = sqlite::Sqlite::open(&path, mode);
            if go == 0 { title = db.name(); }
            let mut rng = Rng::new(0x5EED_1234 + go as u64);
            collected.push(vec![
                measure(&mut db, "bulk insert", |e| e.load(ROWS)),
                measure(&mut db, "all reads", |e| e.reads(READ_OPS, &mut Rng::new(1 + go as u64))),
                measure(&mut db, "95/5 read-update", |e| e.mixed(MIXED_OPS, 50, &mut rng)),
                measure(&mut db, "50/50 read-update", |e| e.mixed(MIXED_OPS, 500, &mut rng)),
            ]);
            drop(db);
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(path.with_extension("db-wal"));
            let _ = std::fs::remove_file(path.with_extension("db-shm"));
        }

        let mut middling = Vec::new();
        let mut spread = Vec::new();
        for i in 0..4 {
            let per_workload: Vec<Run> = collected.iter_mut().map(|g| std::mem::replace(
                &mut g[i],
                Run { what: "", ops: 0, wall_ns: 1, cpu_ns: 0, each: Vec::new() },
            )).collect();
            let (run, low, high) = median_run(per_workload);
            spread.push((run.what, low, high));
            middling.push(run);
        }
        table(&title, &mut middling);
        println!("  slowest and fastest go at each, operations a second:");
        for (what, low, high) in spread {
            println!("    {:<18}{:>12} .. {}", what, thousands(low), thousands(high));
        }
    }

    // ferrite, phase 1: no log and no fsync yet, so there is one set of
    // numbers rather than one per durability mode.
    let mut collected: Vec<Vec<Run>> = Vec::new();
    let mut title = String::new();
    for go in 0..goes {
        let mut db = engine::Ferrite::open();
        if go == 0 { title = db.name(); }
        let mut rng = Rng::new(0x5EED_1234 + go as u64);
        collected.push(vec![
            measure(&mut db, "bulk insert", |e| e.load(ROWS)),
            measure(&mut db, "all reads", |e| e.reads(READ_OPS, &mut Rng::new(1 + go as u64))),
            measure(&mut db, "95/5 read-update", |e| e.mixed(MIXED_OPS, 50, &mut rng)),
            measure(&mut db, "50/50 read-update", |e| e.mixed(MIXED_OPS, 500, &mut rng)),
        ]);
    }
    let mut middling = Vec::new();
    for i in 0..4 {
        let per_workload: Vec<Run> = collected.iter_mut().map(|g| std::mem::replace(
            &mut g[i],
            Run { what: "", ops: 0, wall_ns: 1, cpu_ns: 0, each: Vec::new() },
        )).collect();
        middling.push(median_run(per_workload).0);
    }
    let plain_reads = middling[1].per_sec();
    table(&title, &mut middling);
    // The same engine through SQL. The phase 2 gate lives in the gap
    // between this table and the one above.
    let mut collected: Vec<Vec<Run>> = Vec::new();
    let mut title = String::new();
    for go in 0..goes {
        let mut db = engine::FerriteSql::open();
        if go == 0 { title = db.name(); }
        let mut rng = Rng::new(0x5EED_1234 + go as u64);
        collected.push(vec![
            measure(&mut db, "bulk insert", |e| e.load(ROWS)),
            measure(&mut db, "all reads", |e| e.reads(READ_OPS, &mut Rng::new(1 + go as u64))),
            measure(&mut db, "95/5 read-update", |e| e.mixed(MIXED_OPS, 50, &mut rng)),
            measure(&mut db, "50/50 read-update", |e| e.mixed(MIXED_OPS, 500, &mut rng)),
        ]);
    }
    let mut sql_runs = Vec::new();
    for i in 0..4 {
        let per_workload: Vec<Run> = collected.iter_mut().map(|g| std::mem::replace(
            &mut g[i],
            Run { what: "", ops: 0, wall_ns: 1, cpu_ns: 0, each: Vec::new() },
        )).collect();
        sql_runs.push(median_run(per_workload).0);
    }
    let sql_reads = sql_runs[1].per_sec();
    table(&title, &mut sql_runs);

    let _ = (plain_reads, sql_reads);
    let (plain_ns, sql_ns) = phase2_gate(ROWS, 200, 2000);
    let gap = (sql_ns - plain_ns) / plain_ns * 100.0;
    println!(
        "\nPhase 2 gate: a prepared lookup costs {gap:.1}% more than the plain Rust call.\n  {sql_ns:.0} ns against {plain_ns:.0} ns a lookup, side by side. The gate is 10%."
    );

    // ferrite with its files on disk, in both modes, beside SQLite's
    // two tables above.
    for mode in [ferrite::Durability::Full, ferrite::Durability::Normal] {
        let path = dir.join("ferrite.db");
        let mut collected: Vec<Vec<Run>> = Vec::new();
        let mut title = String::new();
        for go in 0..goes {
            let mut db = engine::FerriteSql::on_disk(&path, mode);
            if go == 0 { title = db.name(); }
            let mut rng = Rng::new(0x5EED_1234 + go as u64);
            collected.push(vec![
                measure(&mut db, "bulk insert", |e| e.load(ROWS)),
                measure(&mut db, "all reads", |e| e.reads(READ_OPS, &mut Rng::new(1 + go as u64))),
                measure(&mut db, "95/5 read-update", |e| e.mixed(MIXED_OPS, 50, &mut rng)),
                measure(&mut db, "50/50 read-update", |e| e.mixed(MIXED_OPS, 500, &mut rng)),
            ]);
        }
        let mut runs = Vec::new();
        for i in 0..4 {
            let per_workload: Vec<Run> = collected.iter_mut().map(|g| std::mem::replace(
                &mut g[i],
                Run { what: "", ops: 0, wall_ns: 1, cpu_ns: 0, each: Vec::new() },
            )).collect();
            runs.push(median_run(per_workload).0);
        }
        table(&title, &mut runs);
        let _ = std::fs::remove_dir_all(&path);
    }
}

#[cfg(not(feature = "bench"))]
fn main() {
    eprintln!("build this with --features bench");
}
