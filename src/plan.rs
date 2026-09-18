//! Turning a parsed statement into something that runs.
//!
//! Planning happens once. It looks every table and column name up, works
//! out whether the WHERE can go straight to a key or through an index,
//! and leaves behind a [`Statement`] holding numbers rather than names.
//! Running it then costs an array index and a B-tree lookup.
//!
//! That is the whole point of a prepared statement, and it is what the
//! phase 2 gate measures: a prepared lookup by key has to cost within
//! 10% of calling [`crate::Table::get_at`] directly.

use std::cmp::Ordering;

use crate::sql::{self, Agg, Cmp, ColDef, Expr, Func, Name, Project, Stmt};
use crate::{Column, Db, Error, IndexId, Result, Row, SortKey, TableId, Value};

/// Where a value comes from: which of the tables the query names, and
/// either the key a row is filed under or one of its stored columns.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Pick {
    Key(u8),
    Col(u8, usize),
}

impl Pick {
    fn side(&self) -> u8 {
        match self { Pick::Key(s) | Pick::Col(s, _) => *s }
    }
}

/// One condition, with its column already resolved.
#[derive(Debug, Clone)]
pub struct Test {
    pick: Pick,
    cmp: Cmp,
    value: Expr,
}

/// How the rows of the first table are found. Going straight to a key,
/// or through an index, is the difference between a lookup and a walk
/// over everything.
#[derive(Debug, Clone)]
enum Find {
    Key(Expr),
    Index { index: IndexId, value: Expr },
    Range { low: Option<(Cmp, Expr)>, high: Option<(Cmp, Expr)> },
    All,
}

#[derive(Debug, Clone)]
struct Where {
    find: Find,
    /// Conditions on the first table, which can be checked before the
    /// second is looked at.
    tests: Vec<Test>,
    /// Conditions that need both tables.
    after_join: Vec<Test>,
}

/// How the second table's matching rows are found, given a row of the
/// first.
#[derive(Debug, Clone)]
enum JoinBy {
    /// The joined column is that table's primary key: one lookup.
    Key,
    /// The joined column has an index: one lookup, however many rows.
    Index(IndexId),
    /// Neither, so every row has to be looked at. Right, and slow.
    Scan(usize),
}

#[derive(Debug, Clone)]
struct Joined {
    table: TableId,
    /// The value on the first table that is matched.
    left: Pick,
    by: JoinBy,
}

#[derive(Debug, Clone)]
struct Sort {
    pick: Pick,
    desc: bool,
}

#[derive(Debug, Clone)]
struct Counting {
    func: Func,
    /// What it is over. `COUNT(*)` has nothing.
    arg: Option<Pick>,
}

#[derive(Debug, Clone)]
enum What {
    Row(Vec<Pick>),
    Aggs(Vec<Counting>),
}

/// A lookup of one stored column by key, worked out once at prepare
/// time. Checking the shape of the plan on every call is the overhead a
/// prepared statement exists to remove, so it is done here and never
/// again.
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
    CreateIndex { name: String, table: String, column: String, if_missing: bool },
    DropIndex { name: String, if_there: bool },
    Insert { table: TableId, key: Expr, values: Vec<Expr> },
    Select {
        table: TableId,
        join: Option<Joined>,
        what: What,
        filter: Where,
        order: Vec<Sort>,
        limit: Option<usize>,
    },
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

/// Whether a statement reads or changes something.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind2 { Reads, Changes }

fn err(what: &str) -> Error { Error::Sql(what.to_string()) }

// ── Working out which table a name means ───────────────────────────────

/// The tables a query can name, in the order they were written.
struct Scope<'a> {
    db: &'a Db,
    sides: Vec<(String, TableId)>,
}

impl Scope<'_> {
    fn pick(&self, n: &Name) -> Result<Pick> {
        match &n.table {
            Some(want) => {
                let side = self
                    .sides
                    .iter()
                    .position(|(name, _)| name.eq_ignore_ascii_case(want))
                    .ok_or_else(|| err(&format!("the query has no table called {want}")))?;
                self.in_side(side as u8, &n.column)
                    .ok_or_else(|| err(&format!("{want} has no column called {}", n.column)))
            }
            None => {
                let mut found = None;
                for side in 0..self.sides.len() {
                    if let Some(p) = self.in_side(side as u8, &n.column) {
                        if found.is_some() {
                            return Err(err(&format!(
                                "both tables have a column called {}; say which one",
                                n.column
                            )));
                        }
                        found = Some(p);
                    }
                }
                found.ok_or_else(|| err(&format!("there is no column called {}", n.column)))
            }
        }
    }

    fn in_side(&self, side: u8, column: &str) -> Option<Pick> {
        let t = self.db.table(self.sides[side as usize].1);
        if column.eq_ignore_ascii_case(t.key_name()) { return Some(Pick::Key(side)); }
        t.column_of(column).map(|i| Pick::Col(side, i))
    }
}

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
        Stmt::CreateIndex { name, table, column, if_missing } => {
            // Checked here so that a name that is not there is caught at
            // prepare time like everything else.
            let id = db.table_id(&table)?;
            if db.table(id).column_of(&column).is_none() {
                return Err(err(&format!("{table} has no column called {column}")));
            }
            Plan::CreateIndex { name, table, column, if_missing }
        }
        Stmt::DropIndex { name, if_there } => Plan::DropIndex { name, if_there },
        Stmt::Insert { table, columns, values } => insert(db, table, columns, values)?,
        Stmt::Select { table, join, project, filter, order, limit } => {
            select(db, table, join, project, filter, order, limit)?
        }
        Stmt::Update { table, sets, filter } => update(db, table, sets, filter)?,
        Stmt::Delete { table, filter } => delete(db, table, filter)?,
    };
    let fast = fast_path(&plan);
    Ok(Statement { plan, params, fast })
}

/// Recognise `SELECT one_stored_column FROM t WHERE key = x` with
/// nothing else to check.
fn fast_path(plan: &Plan) -> Option<Fast> {
    let Plan::Select { table, join: None, what: What::Row(picks), filter, order, limit } = plan
    else {
        return None;
    };
    if picks.len() != 1 || !filter.tests.is_empty() || !order.is_empty() || matches!(limit, Some(0))
    {
        return None;
    }
    let Pick::Col(0, col) = picks[0] else { return None };
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
    let values = slots.into_iter().map(|s| s.unwrap_or(Expr::Lit(Value::Null))).collect();
    Ok(Plan::Insert { table: id, key, values })
}

fn select(
    db: &Db,
    table: String,
    join: Option<sql::Join>,
    project: Project,
    filter: Vec<sql::Cond>,
    order: Vec<sql::Order>,
    limit: Option<usize>,
) -> Result<Plan> {
    let id = db.table_id(&table)?;
    let mut scope = Scope { db, sides: vec![(table.clone(), id)] };
    let joined_id = match &join {
        Some(j) => {
            let jid = db.table_id(&j.table)?;
            if j.table.eq_ignore_ascii_case(&table) {
                return Err(err("a table cannot be joined to itself yet"));
            }
            scope.sides.push((j.table.clone(), jid));
            Some(jid)
        }
        None => None,
    };

    let what = match project {
        Project::All => What::Row(
            scope
                .sides
                .iter()
                .enumerate()
                .flat_map(|(side, (_, tid))| {
                    let n = db.table(*tid).columns().len();
                    std::iter::once(Pick::Key(side as u8))
                        .chain((0..n).map(move |i| Pick::Col(side as u8, i)))
                })
                .collect(),
        ),
        Project::Columns(names) => {
            let mut picks = Vec::with_capacity(names.len());
            for n in &names {
                picks.push(scope.pick(n)?);
            }
            What::Row(picks)
        }
        Project::Aggs(aggs) => {
            let mut out = Vec::with_capacity(aggs.len());
            for Agg { func, arg } in &aggs {
                let arg = match arg {
                    Some(n) => Some(scope.pick(n)?),
                    None => None,
                };
                out.push(Counting { func: *func, arg });
            }
            What::Aggs(out)
        }
    };

    let joined = match (join, joined_id) {
        (Some(j), Some(jid)) => {
            let a = scope.pick(&j.left)?;
            let b = scope.pick(&j.right)?;
            // Whichever side names the second table is the one looked up.
            let (left, right) = if a.side() == 0 && b.side() == 1 {
                (a, b)
            } else if a.side() == 1 && b.side() == 0 {
                (b, a)
            } else {
                return Err(err(
                    "a join compares a column of one table with a column of the other",
                ));
            };
            let by = match right {
                Pick::Key(_) => JoinBy::Key,
                Pick::Col(_, col) => match db.index_on(jid, col) {
                    Some(index) => JoinBy::Index(index),
                    None => JoinBy::Scan(col),
                },
            };
            Some(Joined { table: jid, left, by })
        }
        _ => None,
    };

    let filter = where_of(db, &scope, filter)?;
    let mut sorts = Vec::with_capacity(order.len());
    for o in &order {
        sorts.push(Sort { pick: scope.pick(&o.name)?, desc: o.desc });
    }
    Ok(Plan::Select { table: id, join: joined, what, filter, order: sorts, limit })
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
    let scope = Scope { db, sides: vec![(table, id)] };
    let filter = where_of(db, &scope, filter)?;
    Ok(Plan::Update { table: id, sets: resolved, filter })
}

fn delete(db: &Db, table: String, filter: Vec<sql::Cond>) -> Result<Plan> {
    let id = db.table_id(&table)?;
    let scope = Scope { db, sides: vec![(table, id)] };
    let filter = where_of(db, &scope, filter)?;
    Ok(Plan::Delete { table: id, filter })
}

/// Split the conditions into a way of finding rows and the tests still
/// to apply.
fn where_of(db: &Db, scope: &Scope, conds: Vec<sql::Cond>) -> Result<Where> {
    let mut items: Vec<(Pick, Cmp, Expr)> = Vec::with_capacity(conds.len());
    for c in conds {
        items.push((scope.pick(&c.column)?, c.cmp, c.value));
    }
    let first = scope.sides[0].1;

    // One `key = x` beats everything else. It finds at most one row, and
    // every other condition is then a test on that row. Dropping them
    // instead would answer `id = 4 AND id > 6` with row 4.
    let find = if let Some(i) =
        items.iter().position(|(p, c, _)| *p == Pick::Key(0) && *c == Cmp::Eq)
    {
        let (_, _, key) = items.remove(i);
        Find::Key(key)
    } else if let Some(i) = items.iter().position(|(p, c, _)| {
        *c == Cmp::Eq && matches!(p, Pick::Col(0, col) if db.index_on(first, *col).is_some())
    }) {
        let (pick, _, value) = items.remove(i);
        let Pick::Col(_, col) = pick else { unreachable!() };
        Find::Index { index: db.index_on(first, col).unwrap(), value }
    } else {
        let mut low = None;
        let mut high = None;
        let mut rest = Vec::new();
        for (pick, cmp, value) in items.drain(..) {
            match (pick, cmp) {
                (Pick::Key(0), Cmp::Gt) | (Pick::Key(0), Cmp::Ge) if low.is_none() => {
                    low = Some((cmp, value))
                }
                (Pick::Key(0), Cmp::Lt) | (Pick::Key(0), Cmp::Le) if high.is_none() => {
                    high = Some((cmp, value))
                }
                _ => rest.push((pick, cmp, value)),
            }
        }
        items = rest;
        if low.is_some() || high.is_some() {
            Find::Range { low, high }
        } else {
            Find::All
        }
    };

    let mut tests = Vec::new();
    let mut after_join = Vec::new();
    for (pick, cmp, value) in items {
        let t = Test { pick, cmp, value };
        if t.pick.side() == 0 { tests.push(t) } else { after_join.push(t) }
    }
    Ok(Where { find, tests, after_join })
}

fn count_params(stmt: &Stmt) -> usize {
    let mut most = 0;
    let mut see = |e: &Expr| {
        if let Expr::Param(n) = e {
            most = most.max(n + 1);
        }
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

/// How two values order inside a WHERE. Null orders against nothing, not
/// even itself, so any comparison with it comes out false, the way SQL
/// does it. Sorting needs a different answer, and that is [`SortKey`].
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

/// One row of the query: the key and row of each table it names.
type Sides<'a> = [(i64, &'a Row)];

fn take(pick: Pick, sides: &Sides) -> Value {
    match pick {
        Pick::Key(s) => Value::Int(sides[s as usize].0),
        Pick::Col(s, i) => sides[s as usize].1[i].clone(),
    }
}

fn passes(tests: &[Test], sides: &Sides, params: &[Value]) -> Result<bool> {
    for t in tests {
        let right = value_of(&t.value, params)?;
        let held;
        let left = match t.pick {
            Pick::Col(s, i) => &sides[s as usize].1[i],
            Pick::Key(_) => {
                held = take(t.pick, sides);
                &held
            }
        };
        match order(left, right) {
            Some(o) if t.cmp.holds(o) => {}
            _ => return Ok(false),
        }
    }
    Ok(true)
}

fn value_of<'a>(e: &'a Expr, params: &'a [Value]) -> Result<&'a Value> {
    match e {
        Expr::Lit(v) => Ok(v),
        Expr::Param(n) => params.get(*n).ok_or_else(|| err("a value is missing")),
    }
}

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
        let Plan::Select { table, join, what, filter, order, limit } = &self.plan else {
            return Err(err("this statement changes things; use run"));
        };
        // `COUNT(*)` with nothing else to check never needs the rows
        // themselves. The table knows how many it holds, and an index
        // knows how many hold a given value. Fetching each row to add
        // one to a counter was eight times slower than SQLite here.
        if let What::Aggs(aggs) = what {
            if join.is_none()
                && filter.tests.is_empty()
                && aggs.iter().all(|a| a.func == Func::Count && a.arg.is_none())
            {
                if let Some(n) = counted_without_rows(db, *table, filter, params)? {
                    if limit == &Some(0) { return Ok(Outcome::Rows(Vec::new())); }
                    return Ok(Outcome::Rows(vec![vec![Value::Int(n); aggs.len()]]));
                }
            }
        }
        // A query that asks only for the key, found through an index,
        // is already answered by the index: it holds the keys. Fetching
        // each row to read back the key it was filed under is a walk
        // down the row tree for nothing.
        if let What::Row(picks) = what {
            if let Some(rows) = keys_from_index(db, join, picks, filter, order, *limit, params)? {
                return Ok(Outcome::Rows(rows));
            }
        }
        match what {
            What::Aggs(aggs) => {
                let mut state: Vec<AggState> =
                    aggs.iter().map(|a| AggState::new(a.func, a.arg.is_none())).collect();
                visit(db, *table, join, filter, params, &mut |sides| {
                    for (s, a) in state.iter_mut().zip(aggs) {
                        match a.arg {
                            None => s.saw_row(),
                            Some(p) => s.saw(&take(p, sides)),
                        }
                    }
                    Ok(true)
                })?;
                // An aggregate gives one row back, and a LIMIT can only
                // hide it, never cut the counting short.
                if limit == &Some(0) {
                    return Ok(Outcome::Rows(Vec::new()));
                }
                Ok(Outcome::Rows(vec![state.iter().map(AggState::finish).collect()]))
            }
            What::Row(picks) => {
                let cap = limit.unwrap_or(usize::MAX);
                if order.is_empty() {
                    let mut out = Vec::new();
                    visit(db, *table, join, filter, params, &mut |sides| {
                        out.push(picks.iter().map(|p| take(*p, sides)).collect());
                        Ok(out.len() < cap)
                    })?;
                    out.truncate(cap);
                    return Ok(Outcome::Rows(out));
                }
                // With an ORDER BY, every matching row has to be found
                // before any of them can be left out, so the LIMIT waits
                // until the sorting is done.
                let mut rows: Vec<(Vec<SortKey>, Row)> = Vec::new();
                visit(db, *table, join, filter, params, &mut |sides| {
                    let keys = order.iter().map(|s| SortKey(take(s.pick, sides))).collect();
                    rows.push((keys, picks.iter().map(|p| take(*p, sides)).collect()));
                    Ok(true)
                })?;
                rows.sort_by(|a, b| {
                    for (i, s) in order.iter().enumerate() {
                        let got = a.0[i].cmp(&b.0[i]);
                        let got = if s.desc { got.reverse() } else { got };
                        if got != Ordering::Equal {
                            return got;
                        }
                    }
                    Ordering::Equal
                });
                rows.truncate(cap);
                Ok(Outcome::Rows(rows.into_iter().map(|(_, r)| r).collect()))
            }
        }
    }

    /// Run a statement that changes something.
    pub fn run(&self, db: &mut Db, params: &[Value]) -> Result<Outcome> {
        self.check(params)?;
        match &self.plan {
            Plan::Begin => { db.begin(); Ok(Outcome::Done) }
            Plan::Commit => { db.commit()?; Ok(Outcome::Done) }
            Plan::Rollback => { db.rollback(); Ok(Outcome::Done) }

            Plan::CreateTable { name, key, columns, if_missing } => {
                if *if_missing && db.table_id(name).is_ok() {
                    return Ok(Outcome::Done);
                }
                db.create_table_full(name, key, columns.clone())?;
                Ok(Outcome::Done)
            }

            Plan::CreateIndex { name, table, column, if_missing } => {
                if *if_missing && db.index_id(name).is_ok() {
                    return Ok(Outcome::Done);
                }
                let id = db.table_id(table)?;
                let col = db
                    .table(id)
                    .column_of(column)
                    .ok_or_else(|| err(&format!("{table} has no column called {column}")))?;
                db.create_index(name, id, col)?;
                Ok(Outcome::Done)
            }

            Plan::DropIndex { name, if_there } => {
                if *if_there && db.index_id(name).is_err() {
                    return Ok(Outcome::Done);
                }
                db.drop_index(name)?;
                Ok(Outcome::Done)
            }

            Plan::Insert { table, key, values } => {
                let key = match value_of(key, params)? {
                    Value::Int(i) => *i,
                    other => {
                        return Err(err(&format!(
                            "a key has to be an integer, not {}",
                            other.type_name()
                        )))
                    }
                };
                let row: Row = values
                    .iter()
                    .map(|e| value_of(e, params).cloned())
                    .collect::<Result<Row>>()?;
                db.insert(*table, key, row)?;
                Ok(Outcome::Changed(1))
            }

            Plan::Update { table, sets, filter } => {
                let keys = self.matching(db, *table, filter, params)?;
                for key in &keys {
                    for (col, e) in sets {
                        let v = value_of(e, params)?.clone();
                        db.update(*table, *key, *col, v)?;
                    }
                }
                Ok(Outcome::Changed(keys.len()))
            }

            Plan::Delete { table, filter } => {
                let keys = self.matching(db, *table, filter, params)?;
                for key in &keys {
                    db.delete(*table, *key)?;
                }
                Ok(Outcome::Changed(keys.len()))
            }

            Plan::Select { .. } => Err(err("this statement reads; use query")),
        }
    }

    /// The keys a change applies to, gathered before anything moves.
    fn matching(
        &self,
        db: &Db,
        table: TableId,
        filter: &Where,
        params: &[Value],
    ) -> Result<Vec<i64>> {
        let mut keys = Vec::new();
        visit(db, table, &None, filter, params, &mut |sides| {
            keys.push(sides[0].0);
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

/// The rows of a query that wants nothing but the key and finds them
/// through an index. None when the query is not of that shape.
#[allow(clippy::too_many_arguments)]
fn keys_from_index(
    db: &Db,
    join: &Option<Joined>,
    picks: &[Pick],
    filter: &Where,
    order: &[Sort],
    limit: Option<usize>,
    params: &[Value],
) -> Result<Option<Vec<Row>>> {
    if join.is_some() || !filter.tests.is_empty() || picks.is_empty() {
        return Ok(None);
    }
    let Find::Index { index, value } = &filter.find else { return Ok(None) };
    let on = db.index(*index).column();
    // The key, and the column the index is on. Both are in the index, so
    // neither needs the row.
    if !picks.iter().all(|p| *p == Pick::Key(0) || *p == Pick::Col(0, on)) {
        return Ok(None);
    }
    // The keys come out of the index in order, so sorting by the key is
    // free and anything else is not.
    let desc = match order {
        [] => false,
        [one] if one.pick == Pick::Key(0) => one.desc,
        _ => return Ok(None),
    };
    let value = value_of(value, params)?;
    let mut out = Vec::new();
    if *value == Value::Null { return Ok(Some(out)); }
    let Some(rows) = db.index(*index).rows_for(value) else { return Ok(Some(out)) };
    let cap = limit.unwrap_or(usize::MAX);
    let make = |key: &i64, held: &Value| -> Row {
        picks
            .iter()
            .map(|p| if *p == Pick::Key(0) { Value::Int(*key) } else { held.clone() })
            .collect()
    };
    if desc {
        rows.iter().rev().take(cap).for_each(|(k, (_, v))| out.push(make(k, v)));
    } else {
        rows.iter().take(cap).for_each(|(k, (_, v))| out.push(make(k, v)));
    }
    Ok(Some(out))
}

/// How many rows a WHERE picks out, when that can be answered without
/// looking at any of them. None means it has to be walked after all.
fn counted_without_rows(
    db: &Db,
    table: TableId,
    filter: &Where,
    params: &[Value],
) -> Result<Option<i64>> {
    Ok(Some(match &filter.find {
        Find::All => db.table(table).len() as i64,
        Find::Key(e) => match value_of(e, params)? {
            Value::Int(k) => db.table(table).get(*k).is_some() as i64,
            _ => 0,
        },
        Find::Index { index, value } => {
            let v = value_of(value, params)?;
            if *v == Value::Null {
                0
            } else {
                db.index(*index).rows_for(v).map_or(0, |r| r.len()) as i64
            }
        }
        // A range of keys has to be walked; a B-tree cannot say how many
        // lie between two points without stepping over them.
        Find::Range { .. } => return Ok(None),
    }))
}

// ── Aggregates ─────────────────────────────────────────────────────────

/// What one COUNT, SUM, MIN or MAX has seen so far.
struct AggState {
    func: Func,
    /// True for `COUNT(*)`, which counts rows rather than values.
    star: bool,
    rows: i64,
    /// Values that were not null, which is what COUNT of a column counts.
    seen: i64,
    /// A running total while every value has been a whole number.
    whole: Option<i64>,
    /// The total once a real has turned up, or the whole one grew too big.
    real: f64,
    best: Option<Value>,
}

impl AggState {
    fn new(func: Func, star: bool) -> AggState {
        AggState { func, star, rows: 0, seen: 0, whole: Some(0), real: 0.0, best: None }
    }

    fn saw_row(&mut self) { self.rows += 1; }

    fn saw(&mut self, v: &Value) {
        self.rows += 1;
        if *v == Value::Null { return; }
        self.seen += 1;
        match self.func {
            Func::Sum => match v {
                Value::Int(i) => {
                    self.real += *i as f64;
                    self.whole = self.whole.and_then(|w| w.checked_add(*i));
                }
                Value::Real(r) => {
                    self.real += r;
                    self.whole = None;
                }
                // SQLite adds up only what looks like a number.
                _ => {}
            },
            Func::Min | Func::Max => {
                let better = match &self.best {
                    None => true,
                    Some(b) => {
                        let got = SortKey(v.clone()).cmp(&SortKey(b.clone()));
                        if self.func == Func::Min {
                            got == Ordering::Less
                        } else {
                            got == Ordering::Greater
                        }
                    }
                };
                if better { self.best = Some(v.clone()); }
            }
            Func::Count => {}
        }
    }

    fn finish(&self) -> Value {
        match self.func {
            Func::Count => Value::Int(if self.star { self.rows } else { self.seen }),
            // A sum over nothing is nothing, not zero. A zero would be a
            // claim about data that is not there, and SQLite says so too.
            Func::Sum => {
                if self.seen == 0 { return Value::Null; }
                match self.whole {
                    Some(w) => Value::Int(w),
                    None => Value::Real(self.real),
                }
            }
            Func::Min | Func::Max => self.best.clone().unwrap_or(Value::Null),
        }
    }
}

// ── Walking the rows ───────────────────────────────────────────────────

/// Walk the rows a query picks out, stopping when `each` says to.
fn visit(
    db: &Db,
    table: TableId,
    join: &Option<Joined>,
    filter: &Where,
    params: &[Value],
    each: &mut dyn FnMut(&Sides) -> Result<bool>,
) -> Result<()> {
    let t = db.table(table);
    let mut go = |key: i64, row: &Row| -> Result<bool> {
        let one = [(key, row)];
        if !passes(&filter.tests, &one, params)? {
            return Ok(true);
        }
        let Some(j) = join else { return each(&one) };
        let value = take(j.left, &one);
        if value == Value::Null {
            // Null matches nothing, not even another null.
            return Ok(true);
        }
        let right = db.table(j.table);
        match &j.by {
            JoinBy::Key => {
                let Value::Int(k) = value else { return Ok(true) };
                let Some(r) = right.get(k) else { return Ok(true) };
                let both = [(key, row), (k, r)];
                if passes(&filter.after_join, &both, params)? {
                    return each(&both);
                }
                Ok(true)
            }
            JoinBy::Index(index) => {
                let Some(rows) = db.index(*index).rows_for(&value) else { return Ok(true) };
                for (k, (slot, _)) in rows {
                    let Some(r) = right.at_slot(*slot) else { continue };
                    let both = [(key, row), (*k, r)];
                    if passes(&filter.after_join, &both, params)? && !each(&both)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            JoinBy::Scan(col) => {
                for (k, r) in right.iter() {
                    if r.get(*col) != Some(&value) { continue; }
                    let both = [(key, row), (k, r)];
                    if passes(&filter.after_join, &both, params)? && !each(&both)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
        }
    };

    match &filter.find {
        Find::Key(e) => {
            let key = match value_of(e, params)? {
                Value::Int(i) => *i,
                _ => return Ok(()),
            };
            if let Some(row) = t.get(key) {
                go(key, row)?;
            }
        }
        Find::Index { index, value } => {
            let value = value_of(value, params)?;
            if *value == Value::Null { return Ok(()); }
            if let Some(rows) = db.index(*index).rows_for(value) {
                for (key, (slot, _)) in rows {
                    let Some(row) = t.at_slot(*slot) else { continue };
                    if !go(*key, row)? { break; }
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
                if !go(key, row)? { break; }
            }
        }
        Find::All => {
            for (key, row) in t.iter() {
                if !go(key, row)? { break; }
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

    // ── Phase 4 ────────────────────────────────────────────────────────

    #[test]
    fn order_by_sorts_both_ways_and_on_several_columns() {
        let mut db = kv();
        db.execute("UPDATE kv SET a = 0 WHERE id = 3", &[]).unwrap();
        let got: Vec<i64> = db
            .query("SELECT id FROM kv ORDER BY a DESC", &[])
            .unwrap()
            .rows()
            .iter()
            .map(|r| r[0].as_int().unwrap())
            .collect();
        // Rows 0 and 3 both hold zero. The sort is stable, so they keep
        // the order they were found in. SQL promises nothing about ties.
        assert_eq!(got, vec![9, 8, 7, 6, 5, 4, 2, 1, 0, 3]);
        let got: Vec<i64> = db
            .query("SELECT id FROM kv ORDER BY a, id DESC LIMIT 3", &[])
            .unwrap()
            .rows()
            .iter()
            .map(|r| r[0].as_int().unwrap())
            .collect();
        assert_eq!(got, vec![3, 0, 1]);
    }

    #[test]
    fn a_limit_after_an_order_by_takes_the_first_rows_of_the_sorted_lot() {
        let db = kv();
        let got: Vec<i64> = db
            .query("SELECT id FROM kv ORDER BY id DESC LIMIT 2", &[])
            .unwrap()
            .rows()
            .iter()
            .map(|r| r[0].as_int().unwrap())
            .collect();
        assert_eq!(got, vec![9, 8]);
    }

    #[test]
    fn nulls_sort_first() {
        let mut db = Db::new();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER)", &[]).unwrap();
        for (k, v) in [(1, "5"), (2, "NULL"), (3, "1")] {
            db.execute(&format!("INSERT INTO t (id, a) VALUES ({k}, {v})"), &[]).unwrap();
        }
        let got: Vec<i64> = db
            .query("SELECT id FROM t ORDER BY a", &[])
            .unwrap()
            .rows()
            .iter()
            .map(|r| r[0].as_int().unwrap())
            .collect();
        assert_eq!(got, vec![2, 3, 1]);
    }

    #[test]
    fn the_four_aggregates_answer_the_way_sqlite_does() {
        let mut db = Db::new();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b REAL)", &[]).unwrap();
        // Nothing in it yet.
        let out = db.query("SELECT COUNT(*), SUM(a), MIN(a), MAX(a) FROM t", &[]).unwrap();
        assert_eq!(
            out.rows()[0],
            vec![Value::Int(0), Value::Null, Value::Null, Value::Null]
        );
        for (k, a) in [(1, "3"), (2, "NULL"), (3, "-5"), (4, "10")] {
            db.execute(&format!("INSERT INTO t (id, a, b) VALUES ({k}, {a}, 1.5)"), &[]).unwrap();
        }
        let out = db
            .query("SELECT COUNT(*), COUNT(a), SUM(a), MIN(a), MAX(a) FROM t", &[])
            .unwrap();
        assert_eq!(
            out.rows()[0],
            vec![Value::Int(4), Value::Int(3), Value::Int(8), Value::Int(-5), Value::Int(10)]
        );
        // A real anywhere in the sum makes the answer a real.
        let out = db.query("SELECT SUM(b) FROM t", &[]).unwrap();
        assert_eq!(out.rows()[0], vec![Value::Real(6.0)]);
        // All null is the same as nothing, for a sum.
        let out = db.query("SELECT SUM(a) FROM t WHERE id = 2", &[]).unwrap();
        assert_eq!(out.rows()[0], vec![Value::Null]);
    }

    #[test]
    fn an_index_gives_the_same_answers_as_a_walk() {
        let mut db = kv();
        db.execute("UPDATE kv SET a = 50 WHERE id = 7", &[]).unwrap();
        let without: Vec<i64> = db
            .query("SELECT id FROM kv WHERE a = 50 ORDER BY id", &[])
            .unwrap()
            .rows()
            .iter()
            .map(|r| r[0].as_int().unwrap())
            .collect();
        assert_eq!(without, vec![5, 7]);
        db.execute("CREATE INDEX kv_a ON kv (a)", &[]).unwrap();
        let with: Vec<i64> = db
            .query("SELECT id FROM kv WHERE a = 50 ORDER BY id", &[])
            .unwrap()
            .rows()
            .iter()
            .map(|r| r[0].as_int().unwrap())
            .collect();
        assert_eq!(with, without);
    }

    #[test]
    fn asking_only_for_the_key_gives_the_same_rows_either_way() {
        let mut db = kv();
        db.execute("UPDATE kv SET a = 50 WHERE id = 7", &[]).unwrap();
        db.execute("UPDATE kv SET a = 50 WHERE id = 2", &[]).unwrap();
        let walked: Vec<i64> = db
            .query("SELECT id FROM kv WHERE a = 50", &[])
            .unwrap()
            .rows()
            .iter()
            .map(|r| r[0].as_int().unwrap())
            .collect();
        db.execute("CREATE INDEX kv_a ON kv (a)", &[]).unwrap();
        let covered: Vec<i64> = db
            .query("SELECT id FROM kv WHERE a = 50", &[])
            .unwrap()
            .rows()
            .iter()
            .map(|r| r[0].as_int().unwrap())
            .collect();
        assert_eq!(covered, walked);
        assert_eq!(covered, vec![2, 5, 7]);
        // Sorted the other way, and cut short.
        let down: Vec<i64> = db
            .query("SELECT id FROM kv WHERE a = 50 ORDER BY id DESC LIMIT 2", &[])
            .unwrap()
            .rows()
            .iter()
            .map(|r| r[0].as_int().unwrap())
            .collect();
        assert_eq!(down, vec![7, 5]);
        // Asking for the key twice gives it twice.
        let twice = db.query("SELECT id, id FROM kv WHERE a = 50", &[]).unwrap();
        assert_eq!(twice.rows()[0], vec![Value::Int(2), Value::Int(2)]);
        // Asking for anything else still goes to the rows.
        let c = db.query("SELECT c FROM kv WHERE a = 50", &[]).unwrap();
        assert_eq!(c.rows().len(), 3);
        assert_eq!(c.rows()[0], vec![Value::Text("row 2".into())]);
        // And a test on another column is still applied.
        let some = db.query("SELECT id FROM kv WHERE a = 50 AND id > 3", &[]).unwrap();
        let ids: Vec<i64> = some.rows().iter().map(|r| r[0].as_int().unwrap()).collect();
        assert_eq!(ids, vec![5, 7]);
    }

    #[test]
    fn the_index_hands_back_the_value_the_row_holds() {
        // 5 and 5.0 are equal in SQL, so they share one index entry.
        // Asking for the column has to give back what each row holds.
        let mut db = Db::new();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, r REAL)", &[]).unwrap();
        db.execute("INSERT INTO t (id, a, r) VALUES (1, 5, 5.0)", &[]).unwrap();
        db.execute("INSERT INTO t (id, a, r) VALUES (2, 5, 5.0)", &[]).unwrap();
        db.execute("CREATE INDEX t_r ON t (r)", &[]).unwrap();
        // Looking for the whole number finds the real, as SQL says.
        let out = db.query("SELECT id, r FROM t WHERE r = 5", &[]).unwrap();
        assert_eq!(out.rows().len(), 2);
        for row in out.rows() {
            assert_eq!(row[1], Value::Real(5.0), "the row holds a real, not a whole number");
        }
    }

    #[test]
    fn a_rollback_puts_the_index_back_too() {
        let mut db = kv();
        db.execute("CREATE INDEX kv_a ON kv (a)", &[]).unwrap();
        let through_index = |db: &Db, v: i64| -> Vec<i64> {
            db.query(&format!("SELECT id FROM kv WHERE a = {v}"), &[])
                .unwrap()
                .rows()
                .iter()
                .map(|r| r[0].as_int().unwrap())
                .collect()
        };
        db.execute("BEGIN", &[]).unwrap();
        db.execute("INSERT INTO kv (id, a) VALUES (99, 30)", &[]).unwrap();
        db.execute("UPDATE kv SET a = 30 WHERE id = 8", &[]).unwrap();
        db.execute("DELETE FROM kv WHERE id = 3", &[]).unwrap();
        assert_eq!(through_index(&db, 30), vec![8, 99]);
        assert_eq!(through_index(&db, 80), vec![]);
        db.execute("ROLLBACK", &[]).unwrap();
        assert_eq!(through_index(&db, 30), vec![3], "row 3 is back and 8 and 99 are gone");
        assert_eq!(through_index(&db, 80), vec![8], "row 8 has its old value again");
        // And the index agrees with a walk, which is the real test.
        db.execute("DROP INDEX kv_a", &[]).unwrap();
        assert_eq!(through_index(&db, 30), vec![3]);
        assert_eq!(through_index(&db, 80), vec![8]);
    }

    #[test]
    fn an_index_keeps_up_with_changes() {
        let mut db = kv();
        db.execute("CREATE INDEX kv_a ON kv (a)", &[]).unwrap();
        let ids = |db: &Db, v: i64| -> Vec<i64> {
            db.query(&format!("SELECT id FROM kv WHERE a = {v} ORDER BY id"), &[])
                .unwrap()
                .rows()
                .iter()
                .map(|r| r[0].as_int().unwrap())
                .collect()
        };
        assert_eq!(ids(&db, 30), vec![3]);
        db.execute("UPDATE kv SET a = 30 WHERE id = 8", &[]).unwrap();
        assert_eq!(ids(&db, 30), vec![3, 8]);
        db.execute("DELETE FROM kv WHERE id = 3", &[]).unwrap();
        assert_eq!(ids(&db, 30), vec![8]);
        db.execute("INSERT INTO kv (id, a) VALUES (99, 30)", &[]).unwrap();
        assert_eq!(ids(&db, 30), vec![8, 99]);
        db.execute("DROP INDEX kv_a", &[]).unwrap();
        assert_eq!(ids(&db, 30), vec![8, 99]);
    }

    #[test]
    fn an_index_is_refused_twice_and_missed_once() {
        let mut db = kv();
        db.execute("CREATE INDEX kv_a ON kv (a)", &[]).unwrap();
        assert!(db.execute("CREATE INDEX kv_a ON kv (a)", &[]).is_err());
        db.execute("CREATE INDEX IF NOT EXISTS kv_a ON kv (a)", &[]).unwrap();
        assert!(db.prepare("CREATE INDEX kv_z ON kv (nope)").is_err());
        assert!(db.prepare("CREATE INDEX kv_z ON nope (a)").is_err());
        db.execute("DROP INDEX kv_a", &[]).unwrap();
        assert!(db.execute("DROP INDEX kv_a", &[]).is_err());
        db.execute("DROP INDEX IF EXISTS kv_a", &[]).unwrap();
    }

    /// Two tables: orders pointing at customers.
    fn two() -> Db {
        let mut db = Db::new();
        db.execute("CREATE TABLE cust (id INTEGER PRIMARY KEY, name TEXT)", &[]).unwrap();
        db.execute("CREATE TABLE ord (id INTEGER PRIMARY KEY, who INTEGER, amount INTEGER)", &[])
            .unwrap();
        for (id, name) in [(1, "alice"), (2, "bob"), (3, "carol")] {
            db.execute(&format!("INSERT INTO cust (id, name) VALUES ({id}, '{name}')"), &[])
                .unwrap();
        }
        for (id, who, amount) in [(10, 1, 5), (11, 1, 7), (12, 2, 9), (13, 9, 1)] {
            db.execute(
                &format!("INSERT INTO ord (id, who, amount) VALUES ({id}, {who}, {amount})"),
                &[],
            )
            .unwrap();
        }
        db
    }

    #[test]
    fn a_join_through_a_key_matches_and_drops_the_rest() {
        let db = two();
        let out = db
            .query("SELECT ord.id, cust.name FROM ord JOIN cust ON ord.who = cust.id ORDER BY ord.id", &[])
            .unwrap();
        assert_eq!(out.rows().len(), 3, "the order pointing at nobody is left out");
        assert_eq!(out.rows()[0], vec![Value::Int(10), Value::Text("alice".into())]);
        assert_eq!(out.rows()[2], vec![Value::Int(12), Value::Text("bob".into())]);
    }

    #[test]
    fn a_join_reads_the_same_whichever_way_the_on_is_written() {
        let db = two();
        let a = db.query("SELECT ord.id FROM ord JOIN cust ON ord.who = cust.id", &[]).unwrap();
        let b = db.query("SELECT ord.id FROM ord JOIN cust ON cust.id = ord.who", &[]).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn a_join_can_be_filtered_on_either_side() {
        let db = two();
        let out = db
            .query("SELECT ord.id FROM ord JOIN cust ON ord.who = cust.id WHERE cust.name = 'alice'", &[])
            .unwrap();
        let ids: Vec<i64> = out.rows().iter().map(|r| r[0].as_int().unwrap()).collect();
        assert_eq!(ids, vec![10, 11]);
        let out = db
            .query("SELECT cust.name FROM ord JOIN cust ON ord.who = cust.id WHERE ord.amount > 6", &[])
            .unwrap();
        assert_eq!(out.rows().len(), 2);
    }

    #[test]
    fn a_join_onto_a_plain_column_works_with_and_without_an_index() {
        let mut db = two();
        let ask = "SELECT ord.id FROM cust JOIN ord ON cust.id = ord.who ORDER BY ord.id";
        let without: Vec<i64> =
            db.query(ask, &[]).unwrap().rows().iter().map(|r| r[0].as_int().unwrap()).collect();
        assert_eq!(without, vec![10, 11, 12]);
        db.execute("CREATE INDEX ord_who ON ord (who)", &[]).unwrap();
        let with: Vec<i64> =
            db.query(ask, &[]).unwrap().rows().iter().map(|r| r[0].as_int().unwrap()).collect();
        assert_eq!(with, without);
    }

    #[test]
    fn a_join_counts_and_sums_over_the_pairs() {
        let db = two();
        let out = db
            .query("SELECT COUNT(*), SUM(ord.amount) FROM ord JOIN cust ON ord.who = cust.id", &[])
            .unwrap();
        assert_eq!(out.rows()[0], vec![Value::Int(3), Value::Int(21)]);
    }

    #[test]
    fn a_name_in_both_tables_has_to_say_which() {
        let db = two();
        assert!(db.prepare("SELECT id FROM ord JOIN cust ON ord.who = cust.id").is_err());
        assert!(db.prepare("SELECT ord.id FROM ord JOIN cust ON ord.who = cust.id").is_ok());
        assert!(db.prepare("SELECT amount FROM ord JOIN cust ON ord.who = cust.id").is_ok());
        assert!(db.prepare("SELECT nope.id FROM ord JOIN cust ON ord.who = cust.id").is_err());
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
