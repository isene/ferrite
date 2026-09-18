//! Turning a parsed statement into something that runs.
//!
//! Planning happens once. It looks every table and column name up, works
//! out whether the WHERE can go straight to a key, and leaves behind a
//! [`Statement`] holding numbers rather than names. Running it then costs
//! an array index and a B-tree lookup.
//!
//! That is the whole point of a prepared statement, and it is what the
//! phase 2 gate measures: a prepared lookup by key has to cost within
//! 10% of calling [`crate::Table::get_at`] directly.

use std::cmp::Ordering;

use crate::sql::{self, Cmp, ColDef, Expr, Project, Stmt};
use crate::{Column, Db, Error, Result, Row, TableId, Value};

/// Where a value comes from in a row: the key it is filed under, or one
/// of the stored columns.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Pick {
    Key,
    Col(usize),
}

/// One condition, with its column already resolved.
#[derive(Debug, Clone)]
pub struct Test {
    pick: Pick,
    cmp: Cmp,
    value: Expr,
}

/// How the rows to work on are found. Going straight to a key is the
/// difference between a lookup and a walk over the whole table.
#[derive(Debug, Clone)]
enum Find {
    Key(Expr),
    Range { low: Option<(Cmp, Expr)>, high: Option<(Cmp, Expr)> },
    All,
}

#[derive(Debug, Clone)]
struct Where {
    find: Find,
    tests: Vec<Test>,
}

#[derive(Debug, Clone)]
enum What {
    Row(Vec<Pick>),
    Count,
}

/// A lookup of one stored column by key, worked out once at prepare
/// time. Checking the shape of the plan on every call is exactly the
/// overhead a prepared statement exists to remove, so it is done here
/// and never again.
#[derive(Debug, Clone)]
struct Fast {
    table: TableId,
    col: usize,
    key: Expr,
}

/// A statement that has been planned and can be run many times.
#[derive(Debug, Clone)]
pub struct Statement {
    plan: Plan,
    /// How many `?` slots it expects.
    params: usize,
    fast: Option<Fast>,
}

#[derive(Debug, Clone)]
enum Plan {
    CreateTable { name: String, key: String, columns: Vec<Column>, if_missing: bool },
    Insert { table: TableId, key: Expr, values: Vec<Expr> },
    Select { table: TableId, what: What, filter: Where, limit: Option<usize> },
    Update { table: TableId, sets: Vec<(usize, Expr)>, filter: Where },
    Delete { table: TableId, filter: Where },
    Begin,
    Commit,
    Rollback,
}

/// What a statement did.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Rows, already projected.
    Rows(Vec<Row>),
    /// How many rows changed.
    Changed(usize),
    /// Nothing to report.
    Done,
}

impl Outcome {
    pub fn rows(&self) -> &[Row] {
        match self { Outcome::Rows(r) => r, _ => &[] }
    }
    pub fn changed(&self) -> usize {
        match self { Outcome::Changed(n) => *n, _ => 0 }
    }
}

fn err(what: &str) -> Error { Error::Sql(what.to_string()) }

// ── Planning ───────────────────────────────────────────────────────────

/// Work out how to run this SQL against these tables.
pub fn plan(db: &Db, sql: &str) -> Result<Statement> {
    let stmt = sql::parse(sql)?;
    let params = count_params(&stmt);
    let plan = match stmt {
        Stmt::Begin => Plan::Begin,
        Stmt::Commit => Plan::Commit,
        Stmt::Rollback => Plan::Rollback,
        Stmt::CreateTable { name, columns, if_missing } => create(name, columns, if_missing)?,
        Stmt::Insert { table, columns, values } => insert(db, table, columns, values)?,
        Stmt::Select { table, project, filter, limit } => select(db, table, project, filter, limit)?,
        Stmt::Update { table, sets, filter } => update(db, table, sets, filter)?,
        Stmt::Delete { table, filter } => delete(db, table, filter)?,
    };
    let fast = fast_path(&plan);
    Ok(Statement { plan, params, fast })
}

/// Recognise `SELECT one_stored_column FROM t WHERE key = x` with
/// nothing else to check.
fn fast_path(plan: &Plan) -> Option<Fast> {
    let Plan::Select { table, what: What::Row(picks), filter, limit } = plan else { return None };
    if picks.len() != 1 || !filter.tests.is_empty() || matches!(limit, Some(0)) { return None; }
    let Pick::Col(col) = picks[0] else { return None };
    let Find::Key(key) = &filter.find else { return None };
    Some(Fast { table: *table, col, key: key.clone() })
}

fn create(name: String, columns: Vec<ColDef>, if_missing: bool) -> Result<Plan> {
    let keys: Vec<&ColDef> = columns.iter().filter(|c| c.primary).collect();
    if keys.len() > 1 {
        return Err(err("a table can have one primary key"));
    }
    let key_def = keys.first().copied();
    if let Some(k) = key_def {
        if k.kind != crate::Kind::Int {
            return Err(err("the primary key has to be an integer"));
        }
    }
    let key = key_def.map(|k| k.name.clone()).unwrap_or_else(|| "rowid".to_string());
    let stored: Vec<Column> = columns
        .iter()
        .filter(|c| !c.primary)
        .map(|c| Column { name: c.name.clone(), kind: c.kind, null_ok: c.null_ok })
        .collect();
    Ok(Plan::CreateTable { name, key, columns: stored, if_missing })
}

fn insert(db: &Db, table: String, columns: Vec<String>, values: Vec<Expr>) -> Result<Plan> {
    let id = db.table_id(&table)?;
    let t = db.table(id);
    // No column list means every column, key first.
    let names: Vec<String> = if columns.is_empty() {
        std::iter::once(t.key_name().to_string())
            .chain(t.columns().iter().map(|c| c.name.clone()))
            .collect()
    } else {
        columns
    };
    if names.len() != values.len() {
        return Err(err("there are not as many values as columns"));
    }
    let mut key = None;
    let mut slots: Vec<Option<Expr>> = vec![None; t.columns().len()];
    for (name, value) in names.iter().zip(values) {
        if name.eq_ignore_ascii_case(t.key_name()) {
            key = Some(value);
        } else {
            let i = t
                .column_of(name)
                .ok_or_else(|| err(&format!("{table} has no column called {name}")))?;
            slots[i] = Some(value);
        }
    }
    let key = key.ok_or_else(|| err("the primary key has to be given"))?;
    let values = slots
        .into_iter()
        .map(|s| s.unwrap_or(Expr::Lit(Value::Null)))
        .collect();
    Ok(Plan::Insert { table: id, key, values })
}

fn select(
    db: &Db,
    table: String,
    project: Project,
    filter: Vec<sql::Cond>,
    limit: Option<usize>,
) -> Result<Plan> {
    let id = db.table_id(&table)?;
    let what = match project {
        Project::Count => What::Count,
        Project::All => What::Row(
            std::iter::once(Pick::Key)
                .chain((0..db.table(id).columns().len()).map(Pick::Col))
                .collect(),
        ),
        Project::Columns(names) => {
            let mut picks = Vec::with_capacity(names.len());
            for n in &names {
                picks.push(pick_of(db, id, n, &table)?);
            }
            What::Row(picks)
        }
    };
    let filter = where_of(db, id, filter, &table)?;
    Ok(Plan::Select { table: id, what, filter, limit })
}

fn update(db: &Db, table: String, sets: Vec<(String, Expr)>, filter: Vec<sql::Cond>) -> Result<Plan> {
    let id = db.table_id(&table)?;
    let t = db.table(id);
    let mut resolved = Vec::with_capacity(sets.len());
    for (name, value) in sets {
        if name.eq_ignore_ascii_case(t.key_name()) {
            return Err(err("the primary key cannot be changed"));
        }
        let i = t
            .column_of(&name)
            .ok_or_else(|| err(&format!("{table} has no column called {name}")))?;
        resolved.push((i, value));
    }
    let filter = where_of(db, id, filter, &table)?;
    Ok(Plan::Update { table: id, sets: resolved, filter })
}

fn delete(db: &Db, table: String, filter: Vec<sql::Cond>) -> Result<Plan> {
    let id = db.table_id(&table)?;
    let filter = where_of(db, id, filter, &table)?;
    Ok(Plan::Delete { table: id, filter })
}

fn pick_of(db: &Db, id: TableId, name: &str, table: &str) -> Result<Pick> {
    let t = db.table(id);
    if name.eq_ignore_ascii_case(t.key_name()) { return Ok(Pick::Key); }
    t.column_of(name)
        .map(Pick::Col)
        .ok_or_else(|| err(&format!("{table} has no column called {name}")))
}

/// Split the conditions into a way of finding rows and a list of tests
/// still to apply. A single `key = ?` becomes a lookup; comparisons on
/// the key become a range; everything else stays a test.
fn where_of(db: &Db, id: TableId, conds: Vec<sql::Cond>, table: &str) -> Result<Where> {
    let mut items: Vec<(Pick, Cmp, Expr)> = Vec::with_capacity(conds.len());
    for c in conds {
        items.push((pick_of(db, id, &c.column, table)?, c.cmp, c.value));
    }
    // One `key = x` beats everything else. It finds at most one row, and
    // every other condition is then a test on that row. Dropping them
    // instead would answer `id = 4 AND id > 6` with row 4.
    if let Some(i) = items.iter().position(|(p, c, _)| *p == Pick::Key && *c == Cmp::Eq) {
        let (_, _, key) = items.remove(i);
        let tests = items
            .into_iter()
            .map(|(pick, cmp, value)| Test { pick, cmp, value })
            .collect();
        return Ok(Where { find: Find::Key(key), tests });
    }
    let mut low = None;
    let mut high = None;
    let mut tests = Vec::new();
    for (pick, cmp, value) in items {
        match (pick, cmp) {
            (Pick::Key, Cmp::Gt) | (Pick::Key, Cmp::Ge) if low.is_none() => low = Some((cmp, value)),
            (Pick::Key, Cmp::Lt) | (Pick::Key, Cmp::Le) if high.is_none() => high = Some((cmp, value)),
            _ => tests.push(Test { pick, cmp, value }),
        }
    }
    let find = if low.is_some() || high.is_some() {
        Find::Range { low, high }
    } else {
        Find::All
    };
    Ok(Where { find, tests })
}

fn count_params(stmt: &Stmt) -> usize {
    let mut most = 0;
    let mut see = |e: &Expr| {
        if let Expr::Param(n) = e { most = most.max(n + 1); }
    };
    match stmt {
        Stmt::Insert { values, .. } => values.iter().for_each(&mut see),
        Stmt::Select { filter, .. } | Stmt::Delete { filter, .. } => {
            filter.iter().for_each(|c| see(&c.value))
        }
        Stmt::Update { sets, filter, .. } => {
            sets.iter().for_each(|(_, e)| see(e));
            filter.iter().for_each(|c| see(&c.value));
        }
        _ => {}
    }
    most
}

// ── Comparing ──────────────────────────────────────────────────────────

/// How two values order. Null orders against nothing, not even itself,
/// so any comparison with it comes out false, the way SQL does it.
fn order(a: &Value, b: &Value) -> Option<Ordering> {
    match (a, b) {
        (Value::Null, _) | (_, Value::Null) => None,
        (Value::Int(x), Value::Int(y)) => Some(x.cmp(y)),
        (Value::Real(x), Value::Real(y)) => x.partial_cmp(y),
        (Value::Int(x), Value::Real(y)) => (*x as f64).partial_cmp(y),
        (Value::Real(x), Value::Int(y)) => x.partial_cmp(&(*y as f64)),
        (Value::Text(x), Value::Text(y)) => Some(x.cmp(y)),
        (Value::Blob(x), Value::Blob(y)) => Some(x.cmp(y)),
        // Different kinds sort by kind, the way SQLite does.
        _ => Some(rank(a).cmp(&rank(b))),
    }
}

fn rank(v: &Value) -> u8 {
    match v {
        Value::Null => 0,
        Value::Int(_) | Value::Real(_) => 1,
        Value::Text(_) => 2,
        Value::Blob(_) => 3,
    }
}

// ── Running ────────────────────────────────────────────────────────────

/// Whether a statement reads or changes something.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind2 { Reads, Changes }

impl Statement {
    /// Whether this one reads or changes something.
    pub fn kind(&self) -> Kind2 {
        match self.plan {
            Plan::Select { .. } => Kind2::Reads,
            _ => Kind2::Changes,
        }
    }

    /// How many `?` slots this statement wants.
    pub fn params(&self) -> usize { self.params }

    /// True when this is a lookup of one stored column by key, which is
    /// the shape [`Statement::value`] can run.
    pub fn is_point_lookup(&self) -> bool { self.fast.is_some() }

    /// The fast path: one stored column out of one row, found by key.
    /// Nothing is checked here that prepare could check, and nothing is
    /// allocated.
    #[inline]
    pub fn value<'a>(&self, db: &'a Db, params: &[Value]) -> Result<Option<&'a Value>> {
        let Some(fast) = &self.fast else {
            return Err(err("this is not a lookup of one column by key"));
        };
        let key = match value_of(&fast.key, params)? {
            Value::Int(i) => *i,
            _ => return Ok(None),
        };
        Ok(db.table(fast.table).get_at(key, fast.col))
    }

    /// Run a statement that reads.
    pub fn query(&self, db: &Db, params: &[Value]) -> Result<Outcome> {
        self.check(params)?;
        match &self.plan {
            Plan::Select { table, what, filter, limit } => {
                let t = db.table(*table);
                match what {
                    // COUNT looks at every matching row and then gives
                    // one row back. A LIMIT applies to that one row, not
                    // to the counting, so it can only hide the answer.
                    What::Count => {
                        let mut count = 0usize;
                        visit(t, filter, params, |_, _| { count += 1; Ok(true) })?;
                        let rows = if limit == &Some(0) {
                            Vec::new()
                        } else {
                            vec![vec![Value::Int(count as i64)]]
                        };
                        Ok(Outcome::Rows(rows))
                    }
                    What::Row(picks) => {
                        let limit = limit.unwrap_or(usize::MAX);
                        let mut out = Vec::new();
                        visit(t, filter, params, |key, row| {
                            if out.len() >= limit { return Ok(false); }
                            out.push(picks.iter().map(|p| take(*p, key, row)).collect());
                            Ok(out.len() < limit)
                        })?;
                        Ok(Outcome::Rows(out))
                    }
                }
            }
            _ => Err(err("this statement changes things; use run")),
        }
    }

    /// Run a statement that changes something.
    pub fn run(&self, db: &mut Db, params: &[Value]) -> Result<Outcome> {
        self.check(params)?;
        match &self.plan {
            Plan::Begin => { db.begin(); Ok(Outcome::Done) }
            Plan::Commit => { db.commit(); Ok(Outcome::Done) }
            Plan::Rollback => { db.rollback(); Ok(Outcome::Done) }

            Plan::CreateTable { name, key, columns, if_missing } => {
                if *if_missing && db.table_id(name).is_ok() {
                    return Ok(Outcome::Done);
                }
                db.create_table_full(name, key, columns.clone())?;
                Ok(Outcome::Done)
            }

            Plan::Insert { table, key, values } => {
                let key = match value_of(key, params)? {
                    Value::Int(i) => *i,
                    other => return Err(err(&format!("a key has to be an integer, not {}", other.type_name()))),
                };
                let row: Row = values
                    .iter()
                    .map(|e| value_of(e, params).cloned())
                    .collect::<Result<Row>>()?;
                db.table_mut(*table).insert(key, row)?;
                db.note_insert(*table, key);
                Ok(Outcome::Changed(1))
            }

            Plan::Update { table, sets, filter } => {
                let keys = self.matching(db, *table, filter, params)?;
                for key in &keys {
                    db.note_change(*table, *key);
                }
                for key in &keys {
                    for (col, e) in sets {
                        let v = value_of(e, params)?.clone();
                        db.table_mut(*table).update(*key, *col, v)?;
                    }
                }
                Ok(Outcome::Changed(keys.len()))
            }

            Plan::Delete { table, filter } => {
                let keys = self.matching(db, *table, filter, params)?;
                for key in &keys {
                    db.note_change(*table, *key);
                    db.table_mut(*table).delete(*key);
                }
                Ok(Outcome::Changed(keys.len()))
            }

            Plan::Select { .. } => Err(err("this statement reads; use query")),
        }
    }

    /// The keys a change applies to, gathered before anything moves.
    fn matching(&self, db: &Db, table: TableId, filter: &Where, params: &[Value]) -> Result<Vec<i64>> {
        let mut keys = Vec::new();
        visit(db.table(table), filter, params, |key, _| {
            keys.push(key);
            Ok(true)
        })?;
        Ok(keys)
    }

    fn check(&self, params: &[Value]) -> Result<()> {
        if params.len() < self.params {
            return Err(err(&format!(
                "this statement wants {} values and got {}",
                self.params,
                params.len()
            )));
        }
        Ok(())
    }
}

fn value_of<'a>(e: &'a Expr, params: &'a [Value]) -> Result<&'a Value> {
    match e {
        Expr::Lit(v) => Ok(v),
        Expr::Param(n) => params.get(*n).ok_or_else(|| err("a value is missing")),
    }
}

fn take(pick: Pick, key: i64, row: &Row) -> Value {
    match pick {
        Pick::Key => Value::Int(key),
        Pick::Col(i) => row[i].clone(),
    }
}

fn passes(tests: &[Test], key: i64, row: &Row, params: &[Value]) -> Result<bool> {
    for t in tests {
        let left = match t.pick {
            Pick::Key => Value::Int(key),
            Pick::Col(i) => row[i].clone(),
        };
        let right = value_of(&t.value, params)?;
        match order(&left, right) {
            Some(o) if t.cmp.holds(o) => {}
            _ => return Ok(false),
        }
    }
    Ok(true)
}

/// Walk the rows a WHERE picks out, stopping when `each` says to.
fn visit(
    t: &crate::Table,
    filter: &Where,
    params: &[Value],
    mut each: impl FnMut(i64, &Row) -> Result<bool>,
) -> Result<()> {
    match &filter.find {
        Find::Key(e) => {
            let key = match value_of(e, params)? {
                Value::Int(i) => *i,
                _ => return Ok(()),
            };
            if let Some(row) = t.get(key) {
                if passes(&filter.tests, key, row, params)? {
                    each(key, row)?;
                }
            }
        }
        Find::Range { low, high } => {
            let from = match low {
                Some((Cmp::Gt, e)) => match value_of(e, params)? {
                    Value::Int(i) => i.saturating_add(1),
                    _ => return Ok(()),
                },
                Some((_, e)) => match value_of(e, params)? {
                    Value::Int(i) => *i,
                    _ => return Ok(()),
                },
                None => i64::MIN,
            };
            let to = match high {
                Some((Cmp::Lt, e)) => match value_of(e, params)? {
                    Value::Int(i) => *i,
                    _ => return Ok(()),
                },
                Some((_, e)) => match value_of(e, params)? {
                    Value::Int(i) => i.saturating_add(1),
                    _ => return Ok(()),
                },
                None => i64::MAX,
            };
            for (key, row) in t.range(from, to) {
                if passes(&filter.tests, key, row, params)? && !each(key, row)? {
                    break;
                }
            }
        }
        Find::All => {
            for (key, row) in t.iter() {
                if passes(&filter.tests, key, row, params)? && !each(key, row)? {
                    break;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kv() -> Db {
        let mut db = Db::new();
        db.execute(
            "CREATE TABLE kv (id INTEGER PRIMARY KEY, a INTEGER, b REAL, c TEXT)",
            &[],
        )
        .unwrap();
        for i in 0..10i64 {
            db.execute(
                "INSERT INTO kv (id, a, b, c) VALUES (?1, ?2, ?3, ?4)",
                &[Value::Int(i), Value::Int(i * 10), Value::Real(i as f64), Value::Text(format!("row {i}"))],
            )
            .unwrap();
        }
        db
    }

    #[test]
    fn a_lookup_by_key_finds_one_row() {
        let db = kv();
        let out = db
            .prepare("SELECT a FROM kv WHERE id = ?1")
            .unwrap()
            .query(&db, &[Value::Int(3)])
            .unwrap();
        assert_eq!(out.rows(), &[vec![Value::Int(30)]]);
    }

    #[test]
    fn the_fast_path_gives_the_same_answer() {
        let db = kv();
        let s = db.prepare("SELECT a FROM kv WHERE id = ?1").unwrap();
        assert!(s.is_point_lookup());
        assert_eq!(s.value(&db, &[Value::Int(4)]).unwrap(), Some(&Value::Int(40)));
        assert_eq!(s.value(&db, &[Value::Int(99)]).unwrap(), None);
    }

    #[test]
    fn the_fast_path_refuses_what_it_cannot_do() {
        let db = kv();
        assert!(!db.prepare("SELECT a FROM kv").unwrap().is_point_lookup());
        assert!(!db.prepare("SELECT a, c FROM kv WHERE id = 1").unwrap().is_point_lookup());
        assert!(db.prepare("SELECT a FROM kv").unwrap().value(&db, &[]).is_err());
    }

    #[test]
    fn star_gives_the_key_and_then_the_columns() {
        let db = kv();
        let out = db.query("SELECT * FROM kv WHERE id = 2", &[]).unwrap();
        assert_eq!(
            out.rows()[0],
            vec![Value::Int(2), Value::Int(20), Value::Real(2.0), Value::Text("row 2".into())]
        );
    }

    #[test]
    fn a_range_on_the_key_does_not_walk_the_table() {
        let db = kv();
        let out = db.query("SELECT id FROM kv WHERE id >= 3 AND id < 6", &[]).unwrap();
        let keys: Vec<i64> = out.rows().iter().map(|r| r[0].as_int().unwrap()).collect();
        assert_eq!(keys, vec![3, 4, 5]);
    }

    #[test]
    fn a_test_on_another_column_walks_and_filters() {
        let db = kv();
        let out = db.query("SELECT id FROM kv WHERE a > 70", &[]).unwrap();
        let keys: Vec<i64> = out.rows().iter().map(|r| r[0].as_int().unwrap()).collect();
        assert_eq!(keys, vec![8, 9]);
    }

    #[test]
    fn a_second_condition_on_the_key_still_counts() {
        let db = kv();
        for (sql, want) in [
            ("SELECT id FROM kv WHERE id = 4 AND id > 6", vec![]),
            ("SELECT id FROM kv WHERE id > 6 AND id = 4", vec![]),
            ("SELECT id FROM kv WHERE id = 4 AND id > 2", vec![4]),
            ("SELECT id FROM kv WHERE id > 6 AND id < 3", vec![]),
            ("SELECT id FROM kv WHERE id >= 7 AND id <= 8", vec![7, 8]),
        ] {
            let out = db.query(sql, &[]).unwrap();
            let keys: Vec<i64> = out.rows().iter().map(|r| r[0].as_int().unwrap()).collect();
            assert_eq!(keys, want, "{sql}");
        }
    }

    #[test]
    fn limit_stops_early() {
        let db = kv();
        let out = db.query("SELECT id FROM kv LIMIT 3", &[]).unwrap();
        assert_eq!(out.rows().len(), 3);
    }

    #[test]
    fn a_limit_does_not_cut_a_count_short() {
        let db = kv();
        let out = db.query("SELECT COUNT(*) FROM kv LIMIT 2", &[]).unwrap();
        assert_eq!(out.rows(), &[vec![Value::Int(10)]]);
        let out = db.query("SELECT COUNT(*) FROM kv LIMIT 0", &[]).unwrap();
        assert!(out.rows().is_empty());
    }

    #[test]
    fn count_counts() {
        let db = kv();
        let out = db.query("SELECT COUNT(*) FROM kv WHERE id >= 5", &[]).unwrap();
        assert_eq!(out.rows(), &[vec![Value::Int(5)]]);
    }

    #[test]
    fn update_changes_only_what_it_matched() {
        let mut db = kv();
        let n = db
            .execute("UPDATE kv SET a = ?1 WHERE id = ?2", &[Value::Int(999), Value::Int(3)])
            .unwrap();
        assert_eq!(n.changed(), 1);
        let out = db.execute("SELECT a FROM kv WHERE id = 3", &[]).unwrap();
        assert_eq!(out.rows(), &[vec![Value::Int(999)]]);
        let out = db.execute("SELECT a FROM kv WHERE id = 4", &[]).unwrap();
        assert_eq!(out.rows(), &[vec![Value::Int(40)]]);
    }

    #[test]
    fn delete_takes_the_matching_rows_out() {
        let mut db = kv();
        let n = db.execute("DELETE FROM kv WHERE id < 3", &[]).unwrap();
        assert_eq!(n.changed(), 3);
        let out = db.execute("SELECT COUNT(*) FROM kv", &[]).unwrap();
        assert_eq!(out.rows(), &[vec![Value::Int(7)]]);
    }

    #[test]
    fn null_compares_to_nothing() {
        let mut db = Db::new();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER)", &[]).unwrap();
        db.execute("INSERT INTO t (id, a) VALUES (1, NULL)", &[]).unwrap();
        db.execute("INSERT INTO t (id, a) VALUES (2, 5)", &[]).unwrap();
        for sql in ["SELECT id FROM t WHERE a = 5", "SELECT id FROM t WHERE a <> 5"] {
            let out = db.execute(sql, &[]).unwrap();
            let keys: Vec<i64> = out.rows().iter().map(|r| r[0].as_int().unwrap()).collect();
            assert!(!keys.contains(&1), "{sql} should not find the null row");
        }
    }

    #[test]
    fn a_missing_column_is_null() {
        let mut db = Db::new();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, c TEXT)", &[]).unwrap();
        db.execute("INSERT INTO t (id, c) VALUES (1, 'x')", &[]).unwrap();
        let out = db.execute("SELECT a FROM t WHERE id = 1", &[]).unwrap();
        assert_eq!(out.rows(), &[vec![Value::Null]]);
    }

    #[test]
    fn rollback_puts_everything_back() {
        let mut db = kv();
        db.execute("BEGIN", &[]).unwrap();
        db.execute("UPDATE kv SET a = 0 WHERE id = 1", &[]).unwrap();
        db.execute("DELETE FROM kv WHERE id = 2", &[]).unwrap();
        db.execute("INSERT INTO kv (id, a) VALUES (99, 1)", &[]).unwrap();
        db.execute("ROLLBACK", &[]).unwrap();
        let out = db.execute("SELECT a FROM kv WHERE id = 1", &[]).unwrap();
        assert_eq!(out.rows(), &[vec![Value::Int(10)]]);
        let out = db.execute("SELECT COUNT(*) FROM kv", &[]).unwrap();
        assert_eq!(out.rows(), &[vec![Value::Int(10)]]);
        let out = db.execute("SELECT id FROM kv WHERE id = 99", &[]).unwrap();
        assert!(out.rows().is_empty());
    }

    #[test]
    fn commit_keeps_everything() {
        let mut db = kv();
        db.execute("BEGIN", &[]).unwrap();
        db.execute("UPDATE kv SET a = 0 WHERE id = 1", &[]).unwrap();
        db.execute("COMMIT", &[]).unwrap();
        db.execute("ROLLBACK", &[]).unwrap();
        let out = db.execute("SELECT a FROM kv WHERE id = 1", &[]).unwrap();
        assert_eq!(out.rows(), &[vec![Value::Int(0)]]);
    }

    #[test]
    fn a_statement_says_how_many_values_it_wants() {
        let db = kv();
        let s = db.prepare("SELECT a FROM kv WHERE id = ?1 AND a = ?2").unwrap();
        assert_eq!(s.params(), 2);
        assert!(s.query(&db, &[Value::Int(1)]).is_err());
    }

    #[test]
    fn planning_catches_names_that_are_not_there() {
        let db = kv();
        assert!(db.prepare("SELECT a FROM nope WHERE id = 1").is_err());
        assert!(db.prepare("SELECT nope FROM kv").is_err());
        assert!(db.prepare("UPDATE kv SET nope = 1").is_err());
        assert!(db.prepare("UPDATE kv SET id = 1").is_err());
        assert!(db.prepare("INSERT INTO kv (nope) VALUES (1)").is_err());
        assert!(db.prepare("INSERT INTO kv (a) VALUES (1)").is_err());
    }

    #[test]
    fn a_table_needs_one_integer_key() {
        let mut db = Db::new();
        assert!(db.execute("CREATE TABLE t (a INTEGER PRIMARY KEY, b INTEGER PRIMARY KEY)", &[]).is_err());
        assert!(db.execute("CREATE TABLE t (a TEXT PRIMARY KEY)", &[]).is_err());
        db.execute("CREATE TABLE t (a INTEGER PRIMARY KEY)", &[]).unwrap();
        assert!(db.execute("CREATE TABLE t (a INTEGER PRIMARY KEY)", &[]).is_err());
        db.execute("CREATE TABLE IF NOT EXISTS t (a INTEGER PRIMARY KEY)", &[]).unwrap();
    }
}
