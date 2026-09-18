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

/// Four tables: a plain one, one pointing at it, one with a text key
/// and a unique column, and one with a key over two columns.
const SCHEMA: &str = "\
    CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, a INTEGER, b REAL, c TEXT DEFAULT 'dflt');\
    CREATE TABLE u (id INTEGER PRIMARY KEY, ref INTEGER, tag TEXT, \
                    FOREIGN KEY(ref) REFERENCES t(id) ON DELETE CASCADE);\
    CREATE TABLE s (k TEXT PRIMARY KEY, v INTEGER, n INTEGER UNIQUE);\
    CREATE TABLE w (d TEXT NOT NULL, h INTEGER, x INTEGER DEFAULT 7, PRIMARY KEY(d, h));";

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
    fn pick<'a>(&mut self, of: &[&'a str]) -> &'a str { of[self.below(of.len() as u64) as usize] }
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

/// A plain condition on one of the columns of `t`.
fn a_plain_condition(rng: &mut Rng) -> String {
    let cmp = rng.pick(&["=", "<>", "<", "<=", ">", ">="]);
    match rng.below(4) {
        0 => format!("id {cmp} {}", rng.below(30)),
        1 => format!("a {cmp} {}", rng.below(40) as i64 - 20),
        2 => format!("b {cmp} {}.{}", rng.below(20) as i64 - 10, rng.below(10)),
        _ => format!("c {cmp} {}", c_value(rng)),
    }
}

/// A condition of any shape the parser knows.
fn a_condition(rng: &mut Rng) -> String {
    match rng.below(12) {
        0..=3 => a_plain_condition(rng),
        4 => format!("{} IS {}NULL", rng.pick(&["a", "b", "c"]), rng.pick(&["", "NOT "])),
        5 => {
            let lo = rng.below(40) as i64 - 20;
            format!("{} {}BETWEEN {lo} AND {}", rng.pick(&["a", "id"]), rng.pick(&["", "NOT "]), lo + rng.below(10) as i64)
        }
        6 => format!("({} OR {})", a_plain_condition(rng), a_plain_condition(rng)),
        7 => format!("NOT {}", a_plain_condition(rng)),
        8 => format!("a + {} {} {}", rng.below(5), rng.pick(&["<", ">", "="]), rng.below(40) as i64 - 20),
        9 => format!("COALESCE(a, {}) {} {}", rng.below(10), rng.pick(&["<", ">="]), rng.below(20) as i64 - 10),
        10 => format!("a * 2 {} b", rng.pick(&["<", ">", "="])),
        _ => format!("{} = {} OR {}", rng.pick(&["c", "c"]), c_value(rng), a_plain_condition(rng)),
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

fn a_conflict(rng: &mut Rng) -> &'static str {
    match rng.below(3) {
        0 => "",
        1 => " OR IGNORE",
        _ => " OR REPLACE",
    }
}

/// A statement that changes something.
fn a_change(rng: &mut Rng) -> String {
    // Now and then, put an index up or take one down. Both engines then
    // have the choice of using it, and both have to answer the same.
    if rng.one_in(40) {
        return match rng.below(8) {
            0 => "CREATE INDEX ix_a ON t (a)".into(),
            1 => "DROP INDEX ix_a".into(),
            2 => "CREATE INDEX ix_ref ON u (ref)".into(),
            3 => "DROP INDEX ix_ref".into(),
            4 => "CREATE INDEX ix_ac ON t (a, c)".into(),
            5 => "DROP INDEX ix_ac".into(),
            6 => "CREATE UNIQUE INDEX ix_v ON s (v)".into(),
            _ => "DROP INDEX ix_v".into(),
        };
    }
    match rng.below(20) {
        // The second table, which the first one is joined to. Its ref
        // has to point at a row of t, or both engines refuse it.
        0..=2 => format!(
            "INSERT INTO u (id, ref, tag) VALUES ({}, {}, {})",
            rng.below(30),
            if rng.one_in(9) { "NULL".to_string() } else { rng.below(30).to_string() },
            c_value(rng)
        ),
        3 => format!("UPDATE u SET ref = {}", rng.below(30)),
        4 => format!("DELETE FROM u WHERE id {} {}", rng.pick(&["=", "<", ">"]), rng.below(30)),
        // A text key and a unique column.
        5..=6 => format!(
            "INSERT{} INTO s (k, v, n) VALUES ('k{}', {}, {})",
            a_conflict(rng),
            rng.below(12),
            rng.below(20),
            if rng.one_in(4) { "NULL".to_string() } else { rng.below(12).to_string() }
        ),
        7 => format!("UPDATE s SET {} = {} WHERE k = 'k{}'", rng.pick(&["v", "n"]), rng.below(12), rng.below(12)),
        8 => format!("DELETE FROM s WHERE {} = {}", rng.pick(&["v", "n"]), rng.below(12)),
        // A key over two columns, and a default.
        9..=10 => format!(
            "INSERT{} INTO w (d, h{}) VALUES ('d{}', {}{})",
            a_conflict(rng),
            if rng.one_in(2) { ", x" } else { "" },
            rng.below(5),
            if rng.one_in(5) { "NULL".to_string() } else { rng.below(4).to_string() },
            if rng.one_in(2) { format!(", {}", rng.below(50)) } else { String::new() }
        ),
        11 => format!("DELETE FROM w WHERE d = 'd{}' AND h = {}", rng.below(5), rng.below(4)),
        // The first table.
        12..=13 => format!(
            "INSERT INTO t (id, a, b, c) VALUES ({}, {}, {}, {})",
            rng.below(30),
            a_value(rng),
            b_value(rng),
            c_value(rng)
        ),
        // Without a key, and without every column.
        14 => format!("INSERT INTO t (a, b) VALUES ({}, {})", a_value(rng), b_value(rng)),
        15 => format!("UPDATE t SET a = {}, c = {}{}", a_value(rng), c_value(rng), a_where(rng)),
        16 => format!("UPDATE t SET a = 1 - a{}", a_where(rng)),
        17 => format!("UPDATE t SET a = a + {}, b = b * 2{}", rng.below(5), a_where(rng)),
        _ => format!("DELETE FROM t{}", a_where(rng)),
    }
}

/// A question to put to both engines. When the rows come back in an
/// order the query asked for, they are compared in that order.
struct Query {
    sql: String,
    ordered: bool,
}

/// A statement that reads.
fn a_query(rng: &mut Rng) -> Query {
    match rng.below(14) {
        // Aggregates, which give one row.
        0..=1 => {
            let what = rng.pick(&[
                "COUNT(*)",
                "COUNT(a)",
                "SUM(a)",
                "SUM(b)",
                "MIN(a), MAX(a)",
                "MIN(c), MAX(c)",
                "COUNT(*), SUM(a), MIN(b), MAX(c)",
                "COUNT(*) > 0",
                "SUM(a * 2), MAX(COALESCE(a, 100))",
            ]);
            Query { sql: format!("SELECT {what} FROM t{}", a_where(rng)), ordered: false }
        }
        // A join, one way round or the other, with or without short names.
        2..=3 => {
            let (from, on) = match rng.below(4) {
                0 => ("t JOIN u", "t.id = u.ref"),
                1 => ("u JOIN t", "u.ref = t.id"),
                2 => ("t x JOIN u y", "y.ref = x.id"),
                _ => ("u AS y JOIN t AS x", "x.id = y.ref"),
            };
            let (tn, un) = if from.contains(" x") || from.contains("AS y") { ("x", "y") } else { ("t", "u") };
            let what = match rng.below(4) {
                0 => format!("{tn}.id, {un}.id"),
                1 => format!("{tn}.a, {un}.tag"),
                2 => format!("{un}.id, {tn}.c"),
                _ => "COUNT(*)".to_string(),
            };
            let filter = if rng.one_in(3) {
                format!(" WHERE {tn}.a {} {}", rng.pick(&["<", ">", "="]), rng.below(20) as i64 - 10)
            } else if rng.one_in(3) {
                format!(" WHERE {un}.tag = {} OR {tn}.a IS NULL", c_value(rng))
            } else {
                String::new()
            };
            if what == "COUNT(*)" {
                return Query { sql: format!("SELECT {what} FROM {from} ON {on}{filter}"), ordered: false };
            }
            // Both keys on the end make the order total, so the two
            // engines cannot differ over how ties are laid out.
            Query {
                sql: format!("SELECT {what} FROM {from} ON {on}{filter} ORDER BY {tn}.id, {un}.id"),
                ordered: true,
            }
        }
        // A join on a plain column rather than a key.
        4 => Query {
            sql: format!(
                "SELECT t.id, u.id FROM t JOIN u ON t.a = u.ref{} ORDER BY t.id, u.id",
                if rng.one_in(2) { format!(" WHERE t.id < {}", rng.below(30)) } else { String::new() }
            ),
            ordered: true,
        },
        // Plain rows, sorted.
        5..=6 => {
            let what = rng.pick(&["*", "id", "a", "c", "id, a", "COALESCE(a, id), a + 1, id * 2"]);
            let by = rng.pick(&["a", "b", "c", "id", "COALESCE(a, 0)", "a + b"]);
            let dir = if rng.one_in(2) { " DESC" } else { "" };
            let limit = if rng.one_in(3) { format!(" LIMIT {}", rng.below(6)) } else { String::new() };
            Query {
                sql: format!("SELECT {what} FROM t{} ORDER BY {by}{dir}, id{limit}", a_where(rng)),
                ordered: true,
            }
        }
        // The text-keyed table and the two-column one.
        7..=8 => {
            let sql = match rng.below(6) {
                0 => "SELECT * FROM s ORDER BY k".to_string(),
                1 => format!("SELECT v, n FROM s WHERE k = 'k{}'", rng.below(12)),
                2 => format!("SELECT k FROM s WHERE n = {} OR v = {} ORDER BY k", rng.below(12), rng.below(20)),
                3 => "SELECT * FROM w ORDER BY d, h".to_string(),
                4 => format!("SELECT x FROM w WHERE d = 'd{}' AND h = {}", rng.below(5), rng.below(4)),
                _ => format!("SELECT COUNT(*), SUM(x) FROM w WHERE d = 'd{}'", rng.below(5)),
            };
            Query { sql, ordered: true }
        }
        // Two equalities, which a two-column index can answer.
        9 => Query {
            sql: format!("SELECT id FROM t WHERE a = {} AND c = {}", rng.below(40) as i64 - 20, c_value(rng)),
            ordered: false,
        },
        // Plain rows, in no order anyone asked for. No LIMIT here: a
        // limit with nothing to sort by takes whichever rows the engine
        // happened to walk first, and an index changes that. Both
        // answers are right and they are not the same.
        _ => {
            let what = rng.pick(&["*", "id", "a", "c", "id, a", "a, b, c", "a IS NULL, c"]);
            Query { sql: format!("SELECT {what} FROM t{}", a_where(rng)), ordered: false }
        }
    }
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

/// Every table, as both engines hold it, has to match.
fn same_tables(conn: &Connection, db: &Db, seed: u64, round: usize, when: &str) {
    for table in ["t", "u", "s", "w"] {
        let ask = format!("SELECT * FROM {table}");
        let mine = sorted(db.query(&ask, &[]).unwrap().rows().to_vec());
        let theirs = sorted(sqlite_rows(conn, &ask).unwrap());
        assert!(
            agree(&theirs, &mine),
            "seed {seed}, round {round}: {when}, a different {table}\n  \
             sqlite:  {theirs:?}\n  ferrite: {mine:?}"
        );
    }
}

fn run_it(seed: u64, rounds: usize, place: Where) {
    let conn = Connection::open_in_memory().expect("sqlite");
    conn.execute_batch("PRAGMA foreign_keys = ON").expect("sqlite pragma");
    conn.execute_batch(SCHEMA).expect("sqlite schema");
    let mut db = match &place {
        Where::Memory => Db::new(),
        Where::OnDisk { dir, .. } => {
            let _ = std::fs::remove_dir_all(dir);
            Db::open(dir).expect("open")
        }
    };
    db.execute_batch(SCHEMA).expect("ferrite schema");

    let mut rng = Rng::new(seed);
    let mut queries = 0usize;
    let mut reopens = 0usize;
    let mut refused = 0usize;

    for round in 0..rounds {
        // Close it and open it again. Everything committed has to come
        // back, or the log has lost something.
        if let Where::OnDisk { dir, reopen_every } = &place {
            if round > 0 && round % reopen_every == 0 {
                db.flush().expect("flush");
                drop(db);
                db = Db::open(dir).expect("reopen");
                reopens += 1;
                same_tables(&conn, &db, seed, round, "after reopening");
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
        if theirs.is_err() {
            refused += 1;
            // A statement that failed has to have changed nothing, on
            // both sides alike.
            same_tables(&conn, &db, seed, round, &format!("after {sql} was refused"));
        }
        // How many rows changed is only a question for statements that
        // change rows. After a CREATE INDEX, SQLite reports whatever the
        // count happened to be beforehand.
        let touches_rows = !sql.starts_with("CREATE") && !sql.starts_with("DROP");
        if touches_rows {
            if let (Ok(n), Ok(out)) = (&theirs, &mine) {
                assert_eq!(
                    *n,
                    out.changed(),
                    "seed {seed}, round {round}: different number of rows changed\n  {sql}"
                );
            }
        }
        // A key handed out has to be the same key.
        if theirs.is_ok() && sql.starts_with("INSERT INTO t (a, b)") {
            assert_eq!(
                conn.last_insert_rowid(),
                db.last_insert_key(),
                "seed {seed}, round {round}: a different key was handed out\n  {sql}"
            );
        }

        // Every few changes, ask both the same questions.
        if round % 3 == 0 {
            for _ in 0..3 {
                let q = a_query(&mut rng);
                let theirs = sqlite_rows(&conn, &q.sql);
                let mine = db.query(&q.sql, &[]);
                match (theirs, mine) {
                    (Ok(theirs), Ok(mine)) => {
                        let mut mine = mine.rows().to_vec();
                        let mut theirs = theirs;
                        // A query that asked for an order is checked in
                        // that order. One that did not is checked as a
                        // set, because neither engine promised an order.
                        if !q.ordered {
                            mine = sorted(mine);
                            theirs = sorted(theirs);
                        }
                        assert!(
                            agree(&theirs, &mine),
                            "seed {seed}, round {round}: different answers\n  {}\n  \
                             sqlite:  {theirs:?}\n  ferrite: {mine:?}",
                            q.sql
                        );
                        queries += 1;
                    }
                    (Err(t), Ok(_)) => panic!("seed {seed}: sqlite refused {}: {t}", q.sql),
                    (Ok(_), Err(m)) => panic!("seed {seed}: ferrite refused {}: {m}", q.sql),
                    (Err(_), Err(_)) => {}
                }
            }
        }
    }
    same_tables(&conn, &db, seed, rounds, "at the end");
    assert!(queries > rounds / 2, "hardly any queries were compared");
    assert!(refused > 0, "nothing was ever refused, so the refusals were not tested");
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
