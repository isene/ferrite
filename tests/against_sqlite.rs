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
//!
//! Where SQL leaves an answer open, the generator stays away. A plain
//! column beside an aggregate, a DISTINCT sorted by something it does
//! not show, a LIMIT with no total order: each engine may pick its own
//! row there, and both are right.

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

/// A few values in brackets, for IN. Sometimes none, sometimes a null
/// among them, which leaves NOT IN unknown for every row.
fn a_list(rng: &mut Rng, one: fn(&mut Rng) -> String) -> String {
    let many = rng.below(5);
    let values: Vec<String> = (0..many).map(|_| one(rng)).collect();
    format!("({})", values.join(", "))
}

/// A query that gives one column back, to stand behind IN.
fn a_column_query(rng: &mut Rng) -> (&'static str, String) {
    match rng.below(6) {
        0 => ("id", "SELECT ref FROM u".to_string()),
        1 => ("id", format!("SELECT ref FROM u WHERE tag = {}", c_value(rng))),
        2 => ("id", "SELECT ref FROM u WHERE ref IS NOT NULL AND id > (SELECT MIN(id) FROM u)".to_string()),
        3 => ("a", "SELECT v FROM s".to_string()),
        4 => ("c", "SELECT tag FROM u".to_string()),
        _ => ("a", format!("SELECT a FROM t WHERE id < {}", rng.below(30))),
    }
}

/// A query that gives one value back, with the column to hold it against.
/// Each finds one row at most or adds rows up, so that neither engine
/// has a first row to choose.
fn a_value_query(rng: &mut Rng) -> (&'static str, String) {
    match rng.below(7) {
        0 => ("id", format!("(SELECT COALESCE(MAX(id), 0) FROM t) - {}", rng.below(12))),
        1 => ("a", "(SELECT MIN(a) FROM t)".to_string()),
        2 => ("a", "(SELECT COUNT(*) FROM u)".to_string()),
        3 => ("id", format!("(SELECT ref FROM u WHERE id = {})", rng.below(30))),
        4 => ("c", "(SELECT MAX(c) FROM t)".to_string()),
        5 => ("b", "(SELECT MAX(b) FROM t)".to_string()),
        _ => ("a", format!("(SELECT SUM(v) FROM s WHERE n < {})", rng.below(12))),
    }
}

const PATTERNS: &[&str] =
    &["'a%'", "'%a'", "'%et%'", "'_eta'", "'ALPHA'", "''", "'%'", "'it_s'", "'%l%a'", "'_____'", "'%m_a'", "'d%t%'"];

/// A condition of any shape the parser knows.
fn a_condition(rng: &mut Rng) -> String {
    match rng.below(19) {
        // The key against a number that is not whole, or against a text.
        18 => match rng.below(4) {
            0 => format!("id BETWEEN {}.5 AND {}.{}", rng.below(15), 10 + rng.below(15), rng.pick(&["0", "5"])),
            1 => format!("id {} 'alpha'", rng.pick(&["<", ">", "="])),
            _ => format!(
                "id {} {}.{}",
                rng.pick(&["=", "<>", "<", "<=", ">", ">="]),
                rng.below(30),
                rng.pick(&["0", "5"])
            ),
        },
        12 => {
            let not = rng.pick(&["", "NOT "]);
            match rng.below(4) {
                0 => format!("a {not}LIKE {}", rng.pick(&["'1%'", "'-_'", "'%0'", "'_'", "'-1_'"])),
                1 => format!("id {not}LIKE {}", rng.pick(&["'%2'", "'1_'", "'_'"])),
                _ => format!("c {not}LIKE {}", rng.pick(PATTERNS)),
            }
        }
        13 => {
            let not = rng.pick(&["", "NOT "]);
            match rng.below(3) {
                0 => format!("a {not}IN {}", a_list(rng, a_value)),
                1 => format!("c {not}IN {}", a_list(rng, c_value)),
                _ => format!("id {not}IN {}", a_list(rng, |rng| rng.below(30).to_string())),
            }
        }
        14 => {
            let (column, query) = a_column_query(rng);
            format!("{column} {}IN ({query})", rng.pick(&["", "NOT "]))
        }
        15 => {
            let (column, query) = a_value_query(rng);
            format!("{column} {} {query}", rng.pick(&["=", "<>", "<", "<=", ">", ">="]))
        }
        16 => match rng.below(3) {
            0 => format!("CASE WHEN a > {} THEN b ELSE a END > 1", rng.below(10) as i64 - 5),
            1 => "CASE a WHEN 1 THEN 1 WHEN 2 THEN 0 WHEN 3 THEN NULL ELSE a END".to_string(),
            _ => format!("CASE WHEN c IS NULL THEN a < 0 WHEN c LIKE {} THEN 1 END", rng.pick(PATTERNS)),
        },
        17 => match rng.below(4) {
            0 => format!("instr(c, {}) {} {}", rng.pick(&["'a'", "'ta'", "''", "'s'"]), rng.pick(&["=", ">"]), rng.below(4)),
            1 => format!("substr(c, {}, {}) = {}", rng.below(4), rng.below(4), rng.pick(&["'al'", "'eta'", "'t'", "''"])),
            2 => format!("substr(c, -{}) = {}", rng.below(4), rng.pick(&["'ta'", "'a'", "'s'", "'lta'"])),
            _ => format!("instr(a, '1') = {}", rng.below(3)),
        },
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

/// Texts that are JSON. Every one starts with a curly bracket, which no
/// other text in these tables does, so a query can tell them apart.
const JSON: &[&str] = &[
    r#"{"platform":"irc","n":3,"list":[1,2.5,"x y"],"deep":{"k":null,"ok":true}}"#,
    r#"{ "platform" : "mail", "n" : -7, "list" : [ ], "deep" : { "k" : "it is", "ok" : false } }"#,
    r#"{"n":1e2,"list":[[1,2],{"a":"b"}],"esc":"a\"b\\c\n\u00e9"}"#,
    r#"{}"#,
];

const PATHS: &[&str] = &[
    "$", "$.platform", "$.n", "$.list", "$.list[0]", "$.list[1]", "$.list[2]", "$.list[9]", "$.deep",
    "$.deep.k", "$.deep.ok", "$.esc", "$.nope", "$.list[1].a", "$.\"platform\"",
];

/// A statement that changes something.
fn a_change(rng: &mut Rng) -> String {
    // Now and then, put an index up or take one down. Both engines then
    // have the choice of using it, and both have to answer the same.
    if rng.one_in(40) {
        return match rng.below(12) {
            // A column put on the end of a table that has rows in it. The
            // second try at the same name is refused by both.
            8 => "ALTER TABLE u ADD COLUMN extra INTEGER DEFAULT 5".into(),
            9 => "ALTER TABLE s ADD COLUMN note TEXT".into(),
            10 => "ALTER TABLE w ADD COLUMN y REAL NOT NULL DEFAULT 1.5".into(),
            11 => "ALTER TABLE w ADD COLUMN z INTEGER NOT NULL".into(),
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
    match rng.below(23) {
        // A text that is JSON, for json_extract to read.
        20 => format!(
            "INSERT OR REPLACE INTO u (id, ref, tag) VALUES ({}, NULL, '{}')",
            rng.below(30),
            rng.pick(JSON)
        ),
        // The added columns, which are not there at first. Until they
        // are, both engines refuse these.
        21 => format!("UPDATE u SET extra = {} WHERE id < {}", rng.below(9), rng.below(30)),
        22 => format!("UPDATE s SET note = {} WHERE v > {}", c_value(rng), rng.below(20)),
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

/// A LIMIT, with or without an OFFSET, for a query whose order is total.
fn a_window(rng: &mut Rng) -> String {
    match rng.below(5) {
        0 => String::new(),
        1 => format!(" LIMIT {}", rng.below(6)),
        2 => format!(" LIMIT {} OFFSET {}", rng.below(6), rng.below(8)),
        3 => format!(" LIMIT -1 OFFSET {}", rng.below(8)),
        _ => format!(" LIMIT 1 + {} OFFSET {} * 2", rng.below(4), rng.below(4)),
    }
}

/// A statement that reads.
fn a_query(rng: &mut Rng) -> Query {
    match rng.below(21) {
        // Groups, in no order: one row for each value of something.
        10 => {
            let (what, by) = match rng.below(8) {
                0 => ("a, COUNT(*), SUM(b)", "a"),
                1 => ("c, COUNT(*) AS cnt, SUM(CASE WHEN a > 0 THEN 1 ELSE 0 END) AS pos", "c"),
                2 => ("a, c, COUNT(*), MIN(b), MAX(id)", "a, c"),
                3 => ("a + 1, COUNT(b)", "a + 1"),
                4 => ("a IS NULL, COUNT(*), SUM(a)", "a IS NULL"),
                5 => ("b, COUNT(*)", "b"),
                6 => ("c, MAX(a) - MIN(a), COUNT(c)", "1"),
                _ => ("substr(c, 1, 1) AS first, COUNT(*)", "first"),
            };
            Query { sql: format!("SELECT {what} FROM t{} GROUP BY {by}", a_where(rng)), ordered: false }
        }
        // Groups in an order, which the key of the group makes total,
        // so a LIMIT and an OFFSET can cut them. Also over a join.
        11 => {
            let sql = match rng.below(6) {
                0 => format!("SELECT c, COUNT(*) AS n, MIN(a), MAX(b) FROM t{} GROUP BY c ORDER BY n DESC, c", a_where(rng)),
                1 => format!("SELECT a, COUNT(*) FROM t{} GROUP BY 1 ORDER BY 2 DESC, 1", a_where(rng)),
                2 => format!("SELECT a, SUM(id) FROM t{} GROUP BY a ORDER BY SUM(id), a DESC", a_where(rng)),
                3 => "SELECT t.id, COUNT(*), MAX(u.id) FROM t JOIN u ON t.id = u.ref GROUP BY t.id ORDER BY t.id".to_string(),
                4 => "SELECT u.tag, COUNT(*), SUM(t.a) FROM u JOIN t ON u.ref = t.id GROUP BY u.tag ORDER BY u.tag".to_string(),
                _ => format!("SELECT d, COUNT(*), SUM(x) FROM w WHERE h < {} GROUP BY d ORDER BY d DESC", rng.below(5)),
            };
            Query { sql: format!("{sql}{}", a_window(rng)), ordered: true }
        }
        // No row twice. Sorted by everything asked for, the order is
        // total, and a window can be cut out of it.
        12 => match rng.below(6) {
            0 => Query { sql: format!("SELECT DISTINCT a FROM t{}", a_where(rng)), ordered: false },
            1 => Query { sql: format!("SELECT DISTINCT c, a IS NULL FROM t{}", a_where(rng)), ordered: false },
            2 => Query { sql: "SELECT DISTINCT u.tag FROM u JOIN t ON u.ref = t.id".into(), ordered: false },
            3 => Query {
                sql: format!("SELECT DISTINCT c, a FROM t{} ORDER BY c, a DESC{}", a_where(rng), a_window(rng)),
                ordered: true,
            },
            4 => Query {
                sql: format!("SELECT DISTINCT a FROM t{} ORDER BY a{}", a_where(rng), a_window(rng)),
                ordered: true,
            },
            _ => Query { sql: format!("SELECT DISTINCT ref, tag FROM u ORDER BY 2 DESC, 1{}", a_window(rng)), ordered: true },
        },
        // A window out of sorted rows.
        13 => {
            let what = rng.pick(&["id", "id, a", "*", "c, id"]);
            let by = rng.pick(&["id", "id DESC", "a, id", "c DESC, id", "b DESC, id DESC"]);
            Query { sql: format!("SELECT {what} FROM t{} ORDER BY {by}{}", a_where(rng), a_window(rng)), ordered: true }
        }
        // Things worked out for each row: CASE, the text functions, and
        // what LIKE, IN and a query inside say when the answer is null.
        14 => {
            let (column, query) = a_column_query(rng);
            let what = match rng.below(7) {
                0 => format!("substr(c, {}, {}), substr(c, {})", rng.below(8) as i64 - 3, rng.below(7) as i64 - 2, rng.below(8) as i64 - 4),
                1 => format!("instr(c, {}), instr(a, '-'), substr(a, 1, 1)", rng.pick(&["'a'", "'ta'", "''", "'eta'"])),
                2 => "CASE WHEN a > 0 THEN 'pos' WHEN a < 0 THEN 'neg' ELSE c END, CASE c WHEN 'beta' THEN 1 WHEN '' THEN 2 END".to_string(),
                3 => format!("c LIKE {}, c NOT LIKE {}, a LIKE '1%'", rng.pick(PATTERNS), rng.pick(PATTERNS)),
                4 => format!("a IN {}, c NOT IN {}", a_list(rng, a_value), a_list(rng, c_value)),
                5 => format!("{column} IN ({query}), {column} NOT IN ({query})"),
                _ => "(SELECT COUNT(*) FROM u), a - (SELECT MIN(a) FROM t)".to_string(),
            };
            Query { sql: format!("SELECT id, {what} FROM t{} ORDER BY id", a_where(rng)), ordered: true }
        }
        // JSON, written into the query and kept in a text column, and
        // the columns that are added along the way.
        15 => {
            let sql = match rng.below(6) {
                0 => format!("SELECT json_extract('{}', '{}') FROM s", rng.pick(JSON), rng.pick(PATHS)),
                1..=2 => format!(
                    "SELECT id, json_extract(tag, '{}'), json_extract(tag, '{}') FROM u WHERE tag LIKE '{{%' ORDER BY id",
                    rng.pick(PATHS),
                    rng.pick(PATHS)
                ),
                3 => format!(
                    "SELECT id FROM u WHERE CASE WHEN tag LIKE '{{%' THEN json_extract(tag, '$.n') > {} END ORDER BY id",
                    rng.below(4)
                ),
                4 => "SELECT id FROM u WHERE instr(tag, '\"platform\":\"irc\"') > 0 ORDER BY id".to_string(),
                _ => "SELECT id, extra FROM u ORDER BY id".to_string(),
            };
            Query { sql, ordered: true }
        }
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
        let touches_rows =
            !sql.starts_with("CREATE") && !sql.starts_with("DROP") && !sql.starts_with("ALTER");
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
