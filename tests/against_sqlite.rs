#![cfg(feature = "bench")]
//! Random SQL, run on ferrite and on SQLite, with every answer compared.
//!
//! This is the phase 2 correctness gate. A parser and a planner can pass
//! every test someone thought to write and still be wrong about the
//! query nobody imagined, so the queries here are made up by a machine.
//! SQLite is the authority: where the two disagree, ferrite is wrong
//! until shown otherwise.
//!
//! One difference is on purpose. SQLite lets you put a string in an
//! INTEGER column and keeps it; ferrite refuses. So the generator only
//! makes values that match the column they go in, and the test is about
//! what queries mean rather than what types bend into.

use ferrite::{Db, Value};
use rusqlite::{types::ValueRef, Connection};

const SCHEMA: &str = "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b REAL, c TEXT)";

/// The same repeatable stream the bench uses.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng { Rng(seed | 1) }
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 { self.next() % n }
    fn one_in(&mut self, n: u64) -> bool { self.below(n) == 0 }
}

/// A value for each column, sometimes null.
fn a_value(rng: &mut Rng) -> String {
    if rng.one_in(11) { return "NULL".into(); }
    format!("{}", rng.below(40) as i64 - 20)
}

fn b_value(rng: &mut Rng) -> String {
    if rng.one_in(11) { return "NULL".into(); }
    format!("{}.{}", rng.below(20) as i64 - 10, rng.below(10))
}

fn c_value(rng: &mut Rng) -> String {
    if rng.one_in(11) { return "NULL".into(); }
    let words = ["alpha", "beta", "gamma", "delta", "it''s", "", "zeta"];
    format!("'{}'", words[rng.below(words.len() as u64) as usize])
}

/// A condition on one of the columns.
fn a_condition(rng: &mut Rng) -> String {
    let cmp = ["=", "<>", "<", "<=", ">", ">="][rng.below(6) as usize];
    match rng.below(4) {
        0 => format!("id {cmp} {}", rng.below(30)),
        1 => format!("a {cmp} {}", rng.below(40) as i64 - 20),
        2 => format!("b {cmp} {}.{}", rng.below(20) as i64 - 10, rng.below(10)),
        _ => format!("c {cmp} {}", c_value(rng)),
    }
}

/// A WHERE, sometimes absent, sometimes two conditions.
fn a_where(rng: &mut Rng) -> String {
    match rng.below(6) {
        0 => String::new(),
        1 => format!(" WHERE {} AND {}", a_condition(rng), a_condition(rng)),
        _ => format!(" WHERE {}", a_condition(rng)),
    }
}

/// A statement that changes something.
fn a_change(rng: &mut Rng) -> String {
    match rng.below(10) {
        0..=5 => format!(
            "INSERT INTO t (id, a, b, c) VALUES ({}, {}, {}, {})",
            rng.below(30),
            a_value(rng),
            b_value(rng),
            c_value(rng)
        ),
        6..=7 => format!(
            "UPDATE t SET a = {}, c = {}{}",
            a_value(rng),
            c_value(rng),
            a_where(rng)
        ),
        _ => format!("DELETE FROM t{}", a_where(rng)),
    }
}

/// A statement that reads.
fn a_query(rng: &mut Rng) -> String {
    let what = ["*", "id", "a", "c", "id, a", "a, b, c", "COUNT(*)"][rng.below(7) as usize];
    let limit = if rng.one_in(5) { format!(" LIMIT {}", rng.below(5)) } else { String::new() };
    format!("SELECT {what} FROM t{}{limit}", a_where(rng))
}

// ── Reading the answers ────────────────────────────────────────────────

fn from_sqlite(v: ValueRef<'_>) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::Int(i),
        ValueRef::Real(r) => Value::Real(r),
        ValueRef::Text(t) => Value::Text(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Value::Blob(b.to_vec()),
    }
}

fn sqlite_rows(conn: &Connection, sql: &str) -> Result<Vec<Vec<Value>>, String> {
    let mut stmt = conn.prepare(sql).map_err(|e| e.to_string())?;
    let width = stmt.column_count();
    let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        let mut one = Vec::with_capacity(width);
        for i in 0..width {
            one.push(from_sqlite(row.get_ref(i).map_err(|e| e.to_string())?));
        }
        out.push(one);
    }
    Ok(out)
}

/// Two values are the same answer. Reals get a hair of slack, because
/// both sides parsed the same text and either may round it.
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Real(x), Value::Real(y)) => (x - y).abs() < 1e-9,
        (Value::Int(x), Value::Real(y)) | (Value::Real(y), Value::Int(x)) => {
            (*x as f64 - y).abs() < 1e-9
        }
        _ => a == b,
    }
}

/// The rows, in an order neither engine chose, so that a difference in
/// order is not read as a difference in answer.
fn sorted(mut rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    rows.sort_by(|x, y| format!("{x:?}").cmp(&format!("{y:?}")));
    rows
}

fn agree(a: &[Vec<Value>], b: &[Vec<Value>]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| same(p, q))
        })
}

// ── The test ───────────────────────────────────────────────────────────

/// Where ferrite keeps its rows for a run.
enum Where {
    /// In memory, forgetting everything at the end.
    Memory,
    /// In a directory, closed and opened again every so often, so that
    /// the log has to give back exactly what was put in.
    OnDisk { dir: std::path::PathBuf, reopen_every: usize },
}

fn run_one_seed(seed: u64, rounds: usize) {
    run_it(seed, rounds, Where::Memory)
}

fn run_it(seed: u64, rounds: usize, place: Where) {
    let conn = Connection::open_in_memory().expect("sqlite");
    conn.execute_batch(SCHEMA).expect("sqlite schema");
    let mut db = match &place {
        Where::Memory => Db::new(),
        Where::OnDisk { dir, .. } => {
            let _ = std::fs::remove_dir_all(dir);
            Db::open(dir).expect("open")
        }
    };
    db.execute(SCHEMA, &[]).expect("ferrite schema");

    let mut rng = Rng::new(seed);
    let mut queries = 0usize;
    let mut reopens = 0usize;

    for round in 0..rounds {
        // Close it and open it again. Everything committed has to come
        // back, or the log has lost something.
        if let Where::OnDisk { dir, reopen_every } = &place {
            if round > 0 && round % reopen_every == 0 {
                db.flush().expect("flush");
                drop(db);
                db = Db::open(dir).expect("reopen");
                reopens += 1;
                let mine = sorted(db.query("SELECT * FROM t", &[]).unwrap().rows().to_vec());
                let theirs = sorted(sqlite_rows(&conn, "SELECT * FROM t").unwrap());
                assert!(
                    agree(&theirs, &mine),
                    "seed {seed}, round {round}: reopening gave a different database\n  \
                     sqlite:  {theirs:?}\n  ferrite: {mine:?}"
                );
            }
        }
        let sql = a_change(&mut rng);
        let theirs = conn.execute(&sql, []);
        let mine = db.execute(&sql, &[]);
        assert_eq!(
            theirs.is_ok(),
            mine.is_ok(),
            "seed {seed}, round {round}: the two disagree about whether this works\n  \
             {sql}\n  sqlite: {theirs:?}\n  ferrite: {:?}",
            mine.as_ref().err()
        );
        if let (Ok(n), Ok(out)) = (&theirs, &mine) {
            assert_eq!(
                *n,
                out.changed(),
                "seed {seed}, round {round}: different number of rows changed\n  {sql}"
            );
        }

        // Every few changes, ask both the same questions.
        if round % 3 == 0 {
            for _ in 0..3 {
                let q = a_query(&mut rng);
                let theirs = sqlite_rows(&conn, &q);
                let mine = db.query(&q, &[]);
                match (theirs, mine) {
                    (Ok(theirs), Ok(mine)) => {
                        let mine = sorted(mine.rows().to_vec());
                        let theirs = sorted(theirs);
                        assert!(
                            agree(&theirs, &mine),
                            "seed {seed}, round {round}: different answers\n  {q}\n  \
                             sqlite:  {theirs:?}\n  ferrite: {mine:?}"
                        );
                        queries += 1;
                    }
                    (Err(t), Ok(_)) => panic!("seed {seed}: sqlite refused {q}: {t}"),
                    (Ok(_), Err(m)) => panic!("seed {seed}: ferrite refused {q}: {m}"),
                    (Err(_), Err(_)) => {}
                }
            }
        }
    }
    assert!(queries > rounds / 2, "hardly any queries were compared");
    if let Where::OnDisk { dir, .. } = &place {
        assert!(reopens > 0, "it never reopened");
        drop(db);
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn random_sql_gives_the_same_answers_as_sqlite() {
    for seed in [1, 2, 3, 7, 11, 12345, 99991, 0xBEEF, 0xD00D, 0xFEED] {
        run_one_seed(seed, 2000);
    }
}

/// The same random SQL, but with the rows on a disk and the database
/// closed and opened again as it goes. In memory a bug in the log can
/// never show; here it has to.
#[test]
fn the_same_holds_with_the_rows_on_a_disk() {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    for (n, seed) in [3u64, 19, 0xBEEF, 0xC0FFEE].into_iter().enumerate() {
        let dir = std::path::PathBuf::from(&home)
            .join(".cache")
            .join(format!("ferrite-diff-{}-{n}", std::process::id()));
        run_it(seed, 1200, Where::OnDisk { dir, reopen_every: 50 });
    }
}

#[test]
fn a_long_run_on_one_seed_stays_in_step() {
    run_one_seed(0xC0FFEE, 40_000);
}
