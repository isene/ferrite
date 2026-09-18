//! The phase 3 gate: kill the process mid-write, over and over, and
//! check that nothing committed was lost and nothing on disk was broken.
//!
//! A child process opens a database and inserts keys 0, 1, 2, and so on.
//! After each insert comes back it prints the key and flushes, so the
//! parent's copy of that line is proof the commit had returned. The
//! parent waits a random moment, sends SIGKILL, reads what arrived,
//! reopens the database and checks it.
//!
//! What each mode promises:
//!
//! - FULL: every key the parent saw is in the database. The commit
//!   reached the disk before it returned.
//! - NORMAL: the database opens, the keys form a run from zero with no
//!   holes, and the last few may be missing. SIGKILL does not lose them,
//!   because each commit is a write the kernel already holds; a power
//!   cut would.
//!
//! Run it with:
//!
//! ```text
//! cargo run --release --features bench --bin ferrite-crash -- 10000
//! ```

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use ferrite::{Db, Durability, Value};

const SCHEMA: &str = "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, c TEXT)";

fn scratch() -> PathBuf {
    let base = std::env::var("FERRITE_BENCH_DIR").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        format!("{home}/.cache")
    });
    let dir = PathBuf::from(base).join("ferrite-crash");
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// The child: write until something kills it.
fn child(dir: &Path, mode: Durability) -> ! {
    let mut db = Db::open_with(dir, mode).expect("open");
    db.execute(SCHEMA, &[]).expect("schema");
    let put = db
        .prepare("INSERT INTO t (id, a, c) VALUES (?1, ?2, ?3)")
        .expect("prepare");
    let out = std::io::stdout();
    let mut out = out.lock();
    let mut key = 0i64;
    loop {
        put.run(
            &mut db,
            &[Value::Int(key), Value::Int(key * 7), Value::Text(format!("row {key}"))],
        )
        .expect("insert");
        // The commit has returned. Saying so is what the parent will
        // hold us to.
        writeln!(out, "{key}").expect("write");
        out.flush().expect("flush");
        key += 1;
    }
}

/// What one run found.
struct Run {
    /// The highest key the child said it had committed.
    claimed: Option<i64>,
    /// How many rows the database had when it was opened again.
    found: usize,
}

fn one_run(dir: &Path, mode: Durability, wait: Duration, exe: &Path) -> Result<Run, String> {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;

    let mut child = Command::new(exe)
        .arg("child")
        .arg(dir)
        .arg(match mode { Durability::Full => "full", Durability::Normal => "normal" })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("could not start the child: {e}"))?;

    std::thread::sleep(wait);
    // SIGKILL: no chance to tidy up, close a file or flush anything.
    child.kill().map_err(|e| e.to_string())?;

    let stdout = child.stdout.take().expect("stdout");
    let mut claimed = None;
    for line in BufReader::new(stdout).lines() {
        let Ok(line) = line else { break };
        if let Ok(n) = line.trim().parse::<i64>() { claimed = Some(n); }
    }
    let _ = child.wait();

    // Open it again and see what is really there.
    let db = Db::open_with(dir, mode).map_err(|e| format!("it would not open: {e}"))?;
    let t = match db.table_id("t") {
        Ok(t) => t,
        // The child may have died before it made the table. Then it can
        // have claimed nothing, and an empty database is right.
        Err(_) => {
            return match claimed {
                None => Ok(Run { claimed: None, found: 0 }),
                Some(k) => Err(format!("it claimed key {k} and there is no table")),
            }
        }
    };
    let table = db.table(t);

    // The keys have to be a run from zero with no holes, whatever the
    // mode. A hole would mean a commit in the middle went missing.
    let mut want = 0i64;
    for (key, row) in table.iter() {
        if key != want {
            return Err(format!("a hole: key {want} is missing but {key} is there"));
        }
        if row[0] != Value::Int(key * 7) || row[1] != Value::Text(format!("row {key}")) {
            return Err(format!("key {key} came back with the wrong row: {row:?}"));
        }
        want += 1;
    }

    let found = table.len();
    if mode == Durability::Full {
        if let Some(k) = claimed {
            if (found as i64) < k + 1 {
                return Err(format!(
                    "it claimed key {k} and only {found} rows came back"
                ));
            }
        }
    }
    Ok(Run { claimed, found })
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let exe = PathBuf::from(&args[0]);

    if args.get(1).map(String::as_str) == Some("child") {
        let dir = PathBuf::from(&args[2]);
        let mode = match args.get(3).map(String::as_str) {
            Some("normal") => Durability::Normal,
            _ => Durability::Full,
        };
        child(&dir, mode);
    }

    let runs: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(10_000);
    let dir = scratch();
    println!("phase 3 gate: {runs} runs a mode, each killed with SIGKILL mid-write");
    println!("  working in {}", dir.display());

    for mode in [Durability::Full, Durability::Normal] {
        let name = match mode { Durability::Full => "FULL", Durability::Normal => "NORMAL" };
        let started = Instant::now();
        let mut rng = 0x1234_5678_9ABC_DEF0u64;
        let mut claimed_total = 0i64;
        let mut missing = 0i64;
        let mut extra = 0i64;
        let mut with_rows = 0usize;

        for run in 0..runs {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            // Long enough that the child has usually got going, short
            // enough that the run is over quickly.
            let wait = Duration::from_micros(700 + rng % 9_000);
            match one_run(&dir, mode, wait, &exe) {
                Ok(r) => {
                    if r.found > 0 { with_rows += 1; }
                    if let Some(k) = r.claimed {
                        claimed_total += 1;
                        // A kill often lands between the commit coming
                        // back and the line reaching the parent, so the
                        // database usually holds more than was claimed.
                        let gap = (k + 1) - r.found as i64;
                        if gap > 0 { missing += gap; } else { extra -= gap; }
                    }
                }
                Err(why) => {
                    eprintln!("\n{name} run {run} failed: {why}");
                    eprintln!("the files are in {} for a look", dir.display());
                    std::process::exit(1);
                }
            }
            if run % 200 == 199 {
                print!("\r  {name}: {} of {runs}", run + 1);
                let _ = std::io::stdout().flush();
            }
        }
        println!(
            "\r  {name}: {runs} runs, none lost a committed row, none broke the files.\n    \
             {with_rows} runs wrote something, {claimed_total} claimed a key, \
             {missing} claimed rows missing, {extra} rows there that were never claimed. \
             {:.0} s.",
            started.elapsed().as_secs_f64()
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
