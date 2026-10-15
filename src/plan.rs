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

use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use crate::sql::{self, Cmp, ColDef, Expr, Func, Name, OnConflict, Op, Project, Scalar, Select, Stmt};
use crate::{func, Column, Db, Error, ForeignKey, IndexId, Result, Row, SortKey, TableId, Value};

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

/// A value known without looking at any row: written into the query,
/// or a slot filled in when it runs.
#[derive(Debug, Clone, PartialEq)]
enum Arg {
    Lit(Value),
    Param(usize),
}

/// An expression with every name turned into a [`Pick`].
#[derive(Debug, Clone, PartialEq)]
enum E {
    Lit(Value),
    Param(usize),
    Pick(Pick),
    Not(Box<E>),
    IsNull(Box<E>, bool),
    Between { what: Box<E>, low: Box<E>, high: Box<E>, not: bool },
    Bin(Box<E>, Op, Box<E>),
    Coalesce(Vec<E>),
    /// The n-th aggregate of the statement, once it has been worked out.
    Agg(usize),
    In { what: Box<E>, list: Vec<E>, not: bool },
    Like { what: Box<E>, pattern: Box<E>, not: bool },
    Case { whens: Vec<(E, E)>, otherwise: Option<Box<E>> },
    Call(Scalar, Vec<E>),
}

impl E {
    /// Call `f` on this expression and on every one inside it.
    fn walk<'a>(&'a self, f: &mut dyn FnMut(&'a E)) {
        f(self);
        match self {
            E::Lit(_) | E::Param(_) | E::Pick(_) | E::Agg(_) => {}
            E::Not(e) | E::IsNull(e, _) => e.walk(f),
            E::Between { what, low, high, .. } => {
                what.walk(f);
                low.walk(f);
                high.walk(f);
            }
            E::Bin(a, _, b) | E::Like { what: a, pattern: b, .. } => {
                a.walk(f);
                b.walk(f);
            }
            E::Coalesce(v) | E::Call(_, v) => v.iter().for_each(|e| e.walk(f)),
            E::In { what, list, .. } => {
                what.walk(f);
                list.iter().for_each(|e| e.walk(f));
            }
            E::Case { whens, otherwise } => {
                for (when, then) in whens {
                    when.walk(f);
                    then.walk(f);
                }
                if let Some(e) = otherwise { e.walk(f); }
            }
        }
    }

    /// The furthest table this reaches into: 0 for the first, 1 for the
    /// joined one, 0 as well for something that needs no row at all.
    fn side(&self) -> u8 {
        let mut side = 0;
        self.walk(&mut |e| {
            if let E::Pick(p) = e { side = side.max(p.side()); }
        });
        side
    }

    fn needs_row(&self) -> bool {
        let mut found = false;
        self.walk(&mut |e| found |= matches!(e, E::Pick(_)));
        found
    }

    fn has_aggregate(&self) -> bool {
        let mut found = false;
        self.walk(&mut |e| found |= matches!(e, E::Agg(_)));
        found
    }
}

/// A value known without looking at any row. A sum of written-in
/// numbers, like `100 - 5`, is worked out here when it comes to a whole
/// number, so that it can still find rows by key or through an index.
fn arg_of(scope: &Scope, e: &Expr) -> Option<Arg> {
    match e {
        Expr::Lit(v) => Some(Arg::Lit(v.clone())),
        Expr::Param(n) => Some(Arg::Param(*n)),
        Expr::Bin(..) if e.is_constant() => {
            match scope.resolve(e, &mut None).and_then(|e| eval(&e, &[], &[], &[])) {
                Ok(v @ Value::Int(_)) => Some(Arg::Lit(v)),
                _ => None,
            }
        }
        _ => None,
    }
}

/// One condition of the plain shape, a column against a value. It is
/// nearly every condition there is, and it is checked without building
/// anything. Anything else is an [`E`] in `Where::other`.
#[derive(Debug, Clone)]
struct Test {
    pick: Pick,
    cmp: Cmp,
    arg: Arg,
}

/// How the rows of the first table are found. Going straight to a key,
/// or through an index, is the difference between a lookup and a walk
/// over everything.
#[derive(Debug, Clone)]
enum Find {
    Key(Arg),
    /// The index, and a value for each of its columns in its order.
    Index { index: IndexId, args: Vec<Arg> },
    Range { low: Option<(Cmp, Arg)>, high: Option<(Cmp, Arg)> },
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
    /// The conditions of any other shape, on the first table and on
    /// both. Nearly always empty, and one length test is all they cost
    /// then.
    other: Vec<E>,
    other_after: Vec<E>,
}

impl Where {
    fn is_plain(&self) -> bool { self.tests.is_empty() && self.other.is_empty() }
}

/// How the second table's matching rows are found, given a row of the
/// first.
#[derive(Debug, Clone)]
enum JoinBy {
    /// The joined column is that table's key: one lookup.
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
    expr: E,
    desc: bool,
}

#[derive(Debug, Clone, PartialEq)]
struct Counting {
    func: Func,
    /// What it is over. `COUNT(*)` has nothing.
    arg: Option<E>,
}

#[derive(Debug, Clone)]
enum What {
    /// Columns and keys, nothing worked out: the usual select list.
    Row(Vec<Pick>),
    /// A select list with something to work out in it.
    Exprs(Vec<E>),
    /// A select list with aggregates in it, or one under a GROUP BY. It
    /// gives one row back, or one for each group. `needs_row` is true
    /// when a plain column sits beside the aggregates. Without a GROUP
    /// BY that is answered from the last row seen, and with one from the
    /// first row of each group.
    Aggs { items: Vec<E>, aggs: Vec<Counting>, needs_row: bool },
}

/// A lookup of one stored column by key, worked out once at prepare
/// time. Checking the shape of the plan on every call is the overhead a
/// prepared statement exists to remove, so it is done here and never
/// again.
#[derive(Debug, Clone)]
struct Fast {
    table: TableId,
    col: usize,
    key: Arg,
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
    CreateTable {
        name: String,
        key: String,
        columns: Vec<Column>,
        foreign_keys: Vec<ForeignKey>,
        /// Unique indexes to make with the table, each named and over
        /// some of its columns.
        uniques: Vec<(String, Vec<usize>)>,
        if_missing: bool,
    },
    DropTable { name: String, if_there: bool },
    CreateIndex { name: String, table: String, columns: Vec<String>, unique: bool, if_missing: bool },
    DropIndex { name: String, if_there: bool },
    Insert { table: TableId, key: Option<E>, values: Vec<E>, on_conflict: OnConflict },
    Select {
        table: TableId,
        join: Option<Joined>,
        what: What,
        filter: Where,
        /// `SELECT DISTINCT`: no row comes back twice.
        distinct: bool,
        group: Vec<E>,
        order: Vec<Sort>,
        /// Worked out when the statement runs, since either can be a
        /// `?` slot.
        limit: Option<E>,
        offset: Option<E>,
    },
    /// A statement with a query inside it. The query inside is answered
    /// when the statement runs, and the rest is planned around its
    /// answer then.
    Later { stmt: Box<Stmt>, reads: bool },
    AddColumn { table: TableId, column: Column },
    /// `from_row` is true when a new value is worked out from the row
    /// itself, as in `SET a = a + 1`, so the row has to be fetched.
    Update { table: TableId, sets: Vec<(usize, E)>, filter: Where, from_row: bool },
    Delete { table: TableId, filter: Where },
    Begin,
    Commit,
    Rollback,
    Nothing,
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

struct Side {
    table: String,
    alias: Option<String>,
    id: TableId,
}

/// The tables a query can name, in the order they were written.
struct Scope<'a> {
    db: &'a Db,
    sides: Vec<Side>,
}

impl Scope<'_> {
    fn pick(&self, n: &Name) -> Result<Pick> {
        match &n.table {
            Some(want) => {
                let side = self
                    .sides
                    .iter()
                    .position(|s| {
                        s.alias.as_ref().is_some_and(|a| a.eq_ignore_ascii_case(want))
                            || s.table.eq_ignore_ascii_case(want)
                    })
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
        let t = self.db.table(self.sides[side as usize].id);
        if column.eq_ignore_ascii_case(t.key_name()) { return Some(Pick::Key(side)); }
        t.column_of(column).map(|i| Pick::Col(side, i))
    }

    /// Turn names into picks. Aggregates are collected into `aggs` and
    /// stand in the result by number; where none are allowed, `aggs` is
    /// None and one is an error.
    fn resolve(&self, e: &Expr, aggs: &mut Option<&mut Vec<Counting>>) -> Result<E> {
        Ok(match e {
            Expr::Lit(v) => E::Lit(v.clone()),
            Expr::Param(n) => E::Param(*n),
            Expr::Column(n) => E::Pick(self.pick(n)?),
            Expr::Not(x) => E::Not(Box::new(self.resolve(x, aggs)?)),
            Expr::IsNull(x, not) => E::IsNull(Box::new(self.resolve(x, aggs)?), *not),
            Expr::Between { what, low, high, not } => E::Between {
                what: Box::new(self.resolve(what, aggs)?),
                low: Box::new(self.resolve(low, aggs)?),
                high: Box::new(self.resolve(high, aggs)?),
                not: *not,
            },
            Expr::Bin(a, op, b) => {
                E::Bin(Box::new(self.resolve(a, aggs)?), *op, Box::new(self.resolve(b, aggs)?))
            }
            Expr::Coalesce(v) => E::Coalesce(self.resolve_all(v, aggs)?),
            Expr::Agg(func, arg) => {
                let arg = match arg {
                    Some(x) => Some(self.resolve(x, &mut None)?),
                    None => None,
                };
                let Some(list) = aggs else {
                    return Err(err("an aggregate can only be in the select list"));
                };
                // The same aggregate written twice is worked out once.
                let counting = Counting { func: *func, arg };
                E::Agg(match list.iter().position(|c| *c == counting) {
                    Some(i) => i,
                    None => {
                        list.push(counting);
                        list.len() - 1
                    }
                })
            }
            Expr::In { what, list, not } => E::In {
                what: Box::new(self.resolve(what, aggs)?),
                list: self.resolve_all(list, aggs)?,
                not: *not,
            },
            Expr::Like { what, pattern, not } => E::Like {
                what: Box::new(self.resolve(what, aggs)?),
                pattern: Box::new(self.resolve(pattern, aggs)?),
                not: *not,
            },
            Expr::Case { whens, otherwise } => {
                let mut out = Vec::with_capacity(whens.len());
                for (when, then) in whens {
                    out.push((self.resolve(when, aggs)?, self.resolve(then, aggs)?));
                }
                let otherwise = match otherwise {
                    Some(x) => Some(Box::new(self.resolve(x, aggs)?)),
                    None => None,
                };
                E::Case { whens: out, otherwise }
            }
            Expr::Call(f, args) => E::Call(*f, self.resolve_all(args, aggs)?),
            Expr::Query(_) | Expr::InQuery { .. } => {
                return Err(err("a query inside another is answered before this one is planned"));
            }
        })
    }

    fn resolve_all(&self, v: &[Expr], aggs: &mut Option<&mut Vec<Counting>>) -> Result<Vec<E>> {
        let mut out = Vec::with_capacity(v.len());
        for x in v { out.push(self.resolve(x, aggs)?); }
        Ok(out)
    }

    /// A term of ORDER BY or GROUP BY. It can name an item of the select
    /// list, by the name the item was given or by its number, and then
    /// it means that item. GROUP BY looks for a real column of that name
    /// first, and ORDER BY for a named item first, as SQLite does.
    fn term(
        &self,
        e: &Expr,
        items: &[E],
        names: &[Option<String>],
        column_first: bool,
        aggs: &mut Option<&mut Vec<Counting>>,
    ) -> Result<E> {
        match e {
            Expr::Lit(Value::Int(n)) => {
                return usize::try_from(*n)
                    .ok()
                    .and_then(|n| items.get(n.wrapping_sub(1)))
                    .cloned()
                    .ok_or_else(|| err(&format!("the select list has no column number {n}")));
            }
            Expr::Column(n) if n.table.is_none() && !(column_first && self.pick(n).is_ok()) => {
                let named = names
                    .iter()
                    .position(|a| a.as_ref().is_some_and(|a| a.eq_ignore_ascii_case(&n.column)));
                if let Some(i) = named { return Ok(items[i].clone()); }
            }
            _ => {}
        }
        self.resolve(e, aggs)
    }
}

// ── Planning ───────────────────────────────────────────────────────────

/// Work out how to run this SQL against these tables.
pub fn plan(db: &Db, sql: &str) -> Result<Statement> {
    let stmt = sql::parse(sql)?;
    let params = stmt.params();
    if stmt.has_query() {
        // A query inside another is answered when the statement runs,
        // and the rest is planned around its answer then. What can be
        // checked now is checked now: with an empty answer standing in,
        // every name still has to be there.
        let mut probe = stmt.clone();
        for e in probe.exprs_mut() {
            e.settle(&mut |inner| check_inner(db, inner).map(|()| Vec::new()))?;
        }
        planned(db, probe)?;
        let reads = matches!(stmt, Stmt::Select(_));
        return Ok(Statement { plan: Plan::Later { stmt: Box::new(stmt), reads }, params, fast: None });
    }
    let plan = planned(db, stmt)?;
    let fast = fast_path(&plan);
    Ok(Statement { plan, params, fast })
}

/// Plan a statement that has no query inside it.
fn planned(db: &Db, stmt: Stmt) -> Result<Plan> {
    Ok(match stmt {
        Stmt::Begin => Plan::Begin,
        Stmt::Commit => Plan::Commit,
        Stmt::Rollback => Plan::Rollback,
        Stmt::Nothing => Plan::Nothing,
        Stmt::CreateTable { name, columns, key_columns, unique_sets, foreign_keys, if_missing } => {
            create(db, name, columns, key_columns, unique_sets, foreign_keys, if_missing)?
        }
        Stmt::DropTable { name, if_there } => Plan::DropTable { name, if_there },
        Stmt::CreateIndex { name, table, columns, unique, if_missing } => {
            // Checked here so that a name that is not there is caught at
            // prepare time like everything else.
            let id = db.table_id(&table)?;
            for column in &columns {
                if db.table(id).column_of(column).is_none() {
                    return Err(err(&format!("{table} has no column called {column}")));
                }
            }
            Plan::CreateIndex { name, table, columns, unique, if_missing }
        }
        Stmt::DropIndex { name, if_there } => Plan::DropIndex { name, if_there },
        Stmt::AddColumn { table, column } => Plan::AddColumn {
            table: db.table_id(&table)?,
            column: Column {
                name: column.name,
                kind: column.kind,
                null_ok: column.null_ok,
                default: column.default.unwrap_or(Value::Null),
            },
        },
        Stmt::Insert { table, columns, values, on_conflict } => {
            insert(db, table, columns, values, on_conflict)?
        }
        Stmt::Select(q) => select(db, q)?,
        Stmt::Update { table, sets, filter } => update(db, table, sets, filter)?,
        Stmt::Delete { table, filter } => delete(db, table, filter)?,
    })
}

/// Check a query that sits inside another, without running it. Its
/// names have to be there, and it has to give one column back.
fn check_inner(db: &Db, q: &Select) -> Result<()> {
    let mut q = q.clone();
    for e in q.exprs_mut() {
        e.settle(&mut |inner| check_inner(db, inner).map(|()| Vec::new()))?;
    }
    if let Plan::Select { what, .. } = select(db, q)? {
        let columns = match &what {
            What::Row(picks) => picks.len(),
            What::Exprs(items) | What::Aggs { items, .. } => items.len(),
        };
        if columns != 1 {
            return Err(err("a query inside another gives one column back"));
        }
    }
    Ok(())
}

/// The first column of every row a query inside another comes back
/// with.
fn answer(db: &Db, q: &Select, params: &[Value]) -> Result<Vec<Value>> {
    let mut q = q.clone();
    for e in q.exprs_mut() {
        e.settle(&mut |inner| answer(db, inner, params))?;
    }
    let inner = Statement { plan: select(db, q)?, params: 0, fast: None };
    Ok(match inner.query(db, params)? {
        Outcome::Rows(rows) => rows.into_iter().filter_map(|row| row.into_iter().next()).collect(),
        _ => Vec::new(),
    })
}

/// Answer the queries inside a statement, and plan what is left.
fn settled(db: &Db, stmt: &Stmt, params: &[Value]) -> Result<Plan> {
    let mut stmt = stmt.clone();
    for e in stmt.exprs_mut() {
        e.settle(&mut |inner| answer(db, inner, params))?;
    }
    planned(db, stmt)
}

/// Recognise `SELECT one_stored_column FROM t WHERE key = x` with
/// nothing else to check.
fn fast_path(plan: &Plan) -> Option<Fast> {
    let Plan::Select {
        table,
        join: None,
        what: What::Row(picks),
        filter,
        distinct: false,
        order,
        limit,
        offset: None,
        ..
    } = plan
    else {
        return None;
    };
    // A LIMIT written as a number other than zero cannot hide the one
    // row. Any other LIMIT is only known when the statement runs.
    let shows_a_row = match limit {
        None => true,
        Some(E::Lit(Value::Int(n))) => *n != 0,
        Some(_) => false,
    };
    if picks.len() != 1 || !filter.is_plain() || !order.is_empty() || !shows_a_row {
        return None;
    }
    let Pick::Col(0, col) = picks[0] else { return None };
    let Find::Key(key) = &filter.find else { return None };
    Some(Fast { table: *table, col, key: key.clone() })
}

fn create(
    db: &Db,
    name: String,
    columns: Vec<ColDef>,
    key_columns: Vec<String>,
    unique_sets: Vec<Vec<String>>,
    foreign_keys: Vec<sql::ForeignKey>,
    if_missing: bool,
) -> Result<Plan> {
    let flagged: Vec<String> = columns.iter().filter(|c| c.primary).map(|c| c.name.clone()).collect();
    if flagged.len() > 1 || (!flagged.is_empty() && !key_columns.is_empty()) {
        return Err(err("a table can have one primary key"));
    }
    let pk = if flagged.is_empty() { key_columns } else { flagged };
    for k in &pk {
        if !columns.iter().any(|c| c.name.eq_ignore_ascii_case(k)) {
            return Err(err(&format!("{name} has no column called {k}")));
        }
    }
    // One INTEGER PRIMARY KEY is the key the rows are filed under, as
    // in SQLite. Any other primary key is a unique index over stored
    // columns, and the rows are filed under a hidden rowid.
    let integer_key = pk.len() == 1
        && columns.iter().any(|c| c.name.eq_ignore_ascii_case(&pk[0]) && c.kind == crate::Kind::Int);
    let key = if integer_key { pk[0].clone() } else { "rowid".to_string() };
    let stored: Vec<&ColDef> = columns.iter().filter(|c| !(integer_key && c.name == key)).collect();
    let position = |n: &str| -> Result<usize> {
        stored
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(n))
            .ok_or_else(|| err(&format!("{n} is the key, or not a column of {name}")))
    };

    let mut out = Vec::with_capacity(stored.len());
    for c in &stored {
        let default = c.default.clone().unwrap_or(Value::Null);
        if default != Value::Null && !matches!(
            (&default, c.kind),
            (Value::Int(_), crate::Kind::Int)
                | (Value::Real(_), crate::Kind::Real)
                | (Value::Text(_), crate::Kind::Text)
                | (Value::Blob(_), crate::Kind::Blob)
        ) {
            return Err(err(&format!(
                "the default for {} is {}, and the column holds {}",
                c.name,
                default.type_name(),
                c.kind.name()
            )));
        }
        out.push(Column { name: c.name.clone(), kind: c.kind, null_ok: c.null_ok, default });
    }

    let mut uniques = Vec::new();
    if !integer_key && !pk.is_empty() {
        let mut cols = Vec::with_capacity(pk.len());
        for k in &pk { cols.push(position(k)?); }
        uniques.push((format!("{name}_pk"), cols));
    }
    for c in &stored {
        if c.unique {
            uniques.push((format!("{name}_unique_{}", c.name), vec![position(&c.name)?]));
        }
    }
    for (n, set) in unique_sets.iter().enumerate() {
        let mut cols = Vec::with_capacity(set.len());
        for k in set { cols.push(position(k)?); }
        uniques.push((format!("{name}_unique_{n}"), cols));
    }

    let mut fks = Vec::with_capacity(foreign_keys.len());
    for fk in foreign_keys {
        let parent = db.table_id(&fk.table)?;
        fks.push(ForeignKey { column: position(&fk.column)?, parent, cascade: fk.cascade });
    }
    Ok(Plan::CreateTable { name, key, columns: out, foreign_keys: fks, uniques, if_missing })
}

fn insert(
    db: &Db,
    table: String,
    columns: Vec<String>,
    values: Vec<Expr>,
    on_conflict: OnConflict,
) -> Result<Plan> {
    let id = db.table_id(&table)?;
    let t = db.table(id);
    let names: Vec<String> = if columns.is_empty() {
        let key = if t.key_name() == "rowid" { None } else { Some(t.key_name().to_string()) };
        key.into_iter().chain(t.columns().iter().map(|c| c.name.clone())).collect()
    } else {
        columns
    };
    if names.len() != values.len() {
        return Err(err("there are not as many values as columns"));
    }
    let scope = Scope { db, sides: Vec::new() };
    let mut key = None;
    let mut slots: Vec<Option<E>> = vec![None; t.columns().len()];
    for (name, value) in names.iter().zip(&values) {
        let e = scope.resolve(value, &mut None)?;
        if name.eq_ignore_ascii_case(t.key_name()) {
            key = Some(e);
        } else {
            let i = t
                .column_of(name)
                .ok_or_else(|| err(&format!("{table} has no column called {name}")))?;
            slots[i] = Some(e);
        }
    }
    let values = slots
        .into_iter()
        .zip(t.columns())
        .map(|(s, c)| s.unwrap_or_else(|| E::Lit(c.default.clone())))
        .collect();
    Ok(Plan::Insert { table: id, key, values, on_conflict })
}

fn select(db: &Db, q: Select) -> Result<Plan> {
    let Select { table, alias, join, distinct, project, names, filter, group, order, limit, offset } = q;
    let id = db.table_id(&table)?;
    let mut scope = Scope { db, sides: vec![Side { table: table.clone(), alias, id }] };
    let mut extra: Vec<Expr> = Vec::new();
    let mut joined = None;
    if let Some(j) = join {
        let jid = db.table_id(&j.table)?;
        if j.table.eq_ignore_ascii_case(&table) {
            return Err(err("a table cannot be joined to itself yet"));
        }
        scope.sides.push(Side { table: j.table.clone(), alias: j.alias.clone(), id: jid });
        // One column of each table being equal is the join. Anything
        // else in the ON is a plain condition.
        for part in j.on.conjuncts() {
            if joined.is_none() {
                if let Expr::Bin(a, Op::Cmp(Cmp::Eq), b) = &part {
                    if let (Expr::Column(x), Expr::Column(y)) = (a.as_ref(), b.as_ref()) {
                        let (a, b) = (scope.pick(x)?, scope.pick(y)?);
                        let (left, right) = if a.side() == 0 && b.side() == 1 {
                            (a, b)
                        } else if a.side() == 1 && b.side() == 0 {
                            (b, a)
                        } else {
                            extra.push(part);
                            continue;
                        };
                        let by = match right {
                            Pick::Key(_) => JoinBy::Key,
                            Pick::Col(_, col) => match db.index_on(jid, col) {
                                Some(index) => JoinBy::Index(index),
                                None => JoinBy::Scan(col),
                            },
                        };
                        joined = Some(Joined { table: jid, left, by });
                        continue;
                    }
                }
            }
            extra.push(part);
        }
        if joined.is_none() {
            return Err(err("a join is on a column of one table being equal to a column of the other"));
        }
    }

    let mut aggs = Vec::new();
    let items: Vec<E> = match &project {
        Project::All => scope
            .sides
            .iter()
            .enumerate()
            .flat_map(|(side, s)| {
                let t = db.table(s.id);
                // A hidden rowid is not a column anyone asked for.
                let key = if t.key_name() == "rowid" { None } else { Some(Pick::Key(side as u8)) };
                let n = t.columns().len();
                key.into_iter().chain((0..n).map(move |i| Pick::Col(side as u8, i)))
            })
            .map(E::Pick)
            .collect(),
        Project::Exprs(exprs) => scope.resolve_all(exprs, &mut Some(&mut aggs))?,
    };

    let mut groups = Vec::with_capacity(group.len());
    for g in &group {
        let e = scope.term(g, &items, &names, true, &mut None)?;
        if e.has_aggregate() {
            return Err(err("GROUP BY cannot be an aggregate"));
        }
        groups.push(e);
    }
    // With aggregates or a GROUP BY the rows are added up, and then an
    // ORDER BY can sort by an aggregate too.
    let adds_up = !aggs.is_empty() || !groups.is_empty();
    let mut sorts = Vec::with_capacity(order.len());
    for o in &order {
        let mut allowed = if adds_up { Some(&mut aggs) } else { None };
        sorts.push(Sort { expr: scope.term(&o.expr, &items, &names, false, &mut allowed)?, desc: o.desc });
    }

    let what = if adds_up {
        let needs_row = items.iter().any(E::needs_row);
        What::Aggs { items, aggs, needs_row }
    } else {
        let picks: Option<Vec<Pick>> =
            items.iter().map(|e| if let E::Pick(p) = e { Some(*p) } else { None }).collect();
        match picks {
            Some(picks) => What::Row(picks),
            None => What::Exprs(items),
        }
    };

    let filter = where_of(db, &scope, filter, extra)?;
    // A LIMIT or an OFFSET is a number or a `?` slot. No column of any
    // table can be named in one.
    let no_table = Scope { db, sides: Vec::new() };
    let limit = limit.map(|e| no_table.resolve(&e, &mut None)).transpose()?;
    let offset = offset.map(|e| no_table.resolve(&e, &mut None)).transpose()?;
    Ok(Plan::Select { table: id, join: joined, what, filter, distinct, group: groups, order: sorts, limit, offset })
}

fn update(db: &Db, table: String, sets: Vec<(String, Expr)>, filter: Option<Expr>) -> Result<Plan> {
    let id = db.table_id(&table)?;
    let t = db.table(id);
    let scope = Scope { db, sides: vec![Side { table, alias: None, id }] };
    let mut resolved = Vec::with_capacity(sets.len());
    for (name, value) in sets {
        if name.eq_ignore_ascii_case(t.key_name()) {
            return Err(err("the primary key cannot be changed"));
        }
        let i = t
            .column_of(&name)
            .ok_or_else(|| err(&format!("{} has no column called {name}", t.name())))?;
        resolved.push((i, scope.resolve(&value, &mut None)?));
    }
    let filter = where_of(db, &scope, filter, Vec::new())?;
    let from_row = resolved.iter().any(|(_, e)| e.needs_row());
    Ok(Plan::Update { table: id, sets: resolved, filter, from_row })
}

fn delete(db: &Db, table: String, filter: Option<Expr>) -> Result<Plan> {
    let id = db.table_id(&table)?;
    let scope = Scope { db, sides: vec![Side { table, alias: None, id }] };
    let filter = where_of(db, &scope, filter, Vec::new())?;
    Ok(Plan::Delete { table: id, filter })
}

/// Split the conditions into a way of finding rows and the tests still
/// to apply.
fn where_of(db: &Db, scope: &Scope, filter: Option<Expr>, extra: Vec<Expr>) -> Result<Where> {
    let mut parts = filter.map(Expr::conjuncts).unwrap_or_default();
    parts.extend(extra);
    let mut simple: Vec<(Pick, Cmp, Arg)> = Vec::new();
    let mut others: Vec<E> = Vec::new();
    for part in parts {
        match &part {
            Expr::Bin(a, Op::Cmp(c), b) => {
                if let (Expr::Column(n), Some(arg)) = (a.as_ref(), arg_of(scope, b)) {
                    simple.push((scope.pick(n)?, *c, arg));
                    continue;
                }
                if let (Some(arg), Expr::Column(n)) = (arg_of(scope, a), b.as_ref()) {
                    simple.push((scope.pick(n)?, c.flipped(), arg));
                    continue;
                }
            }
            Expr::Between { what, low, high, not: false } => {
                if let (Expr::Column(n), Some(lo), Some(hi)) =
                    (what.as_ref(), arg_of(scope, low), arg_of(scope, high))
                {
                    let p = scope.pick(n)?;
                    simple.push((p, Cmp::Ge, lo));
                    simple.push((p, Cmp::Le, hi));
                    continue;
                }
            }
            _ => {}
        }
        others.push(scope.resolve(&part, &mut None)?);
    }
    let first = scope.sides[0].id;

    // One `key = x` beats everything else. It finds at most one row, and
    // every other condition is then a test on that row. Dropping them
    // instead would answer `id = 4 AND id > 6` with row 4.
    let find = if let Some(i) =
        simple.iter().position(|(p, c, _)| *p == Pick::Key(0) && *c == Cmp::Eq)
    {
        let (_, _, key) = simple.remove(i);
        Find::Key(key)
    } else {
        let equal: Vec<usize> = simple
            .iter()
            .filter_map(|(p, c, _)| match p {
                Pick::Col(0, col) if *c == Cmp::Eq => Some(*col),
                _ => None,
            })
            .collect();
        match db.index_within(first, &equal) {
            Some(index) => {
                let mut args = Vec::new();
                for &col in db.index(index).columns() {
                    let i = simple
                        .iter()
                        .position(|(p, c, _)| *p == Pick::Col(0, col) && *c == Cmp::Eq)
                        .expect("the index was chosen for these columns");
                    args.push(simple.remove(i).2);
                }
                Find::Index { index, args }
            }
            None => {
                let mut low = None;
                let mut high = None;
                let mut rest = Vec::new();
                for (pick, cmp, value) in simple.drain(..) {
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
                simple = rest;
                if low.is_some() || high.is_some() {
                    Find::Range { low, high }
                } else {
                    Find::All
                }
            }
        }
    };

    let mut tests = Vec::new();
    let mut after_join = Vec::new();
    for (pick, cmp, arg) in simple {
        let t = Test { pick, cmp, arg };
        if pick.side() == 0 { tests.push(t) } else { after_join.push(t) }
    }
    let mut other = Vec::new();
    let mut other_after = Vec::new();
    for e in others {
        if e.side() == 0 { other.push(e) } else { other_after.push(e) }
    }
    Ok(Where { find, tests, after_join, other, other_after })
}

// ── Comparing and working out ──────────────────────────────────────────

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

/// True, false, or unknown, which is what null is inside a condition.
fn truth(v: &Value) -> Option<bool> {
    match v {
        Value::Null => None,
        Value::Int(i) => Some(*i != 0),
        Value::Real(r) => Some(*r != 0.0),
        Value::Text(_) => Some(match number(v) {
            Some(Num::I(i)) => i != 0,
            Some(Num::R(r)) => r != 0.0,
            None => false,
        }),
        Value::Blob(_) => Some(false),
    }
}

fn from_truth(t: Option<bool>) -> Value {
    match t {
        None => Value::Null,
        Some(b) => Value::Int(b as i64),
    }
}

fn compare(a: &Value, b: &Value, cmp: Cmp) -> Option<bool> {
    if *a == Value::Null || *b == Value::Null { return None; }
    Some(order(a, b).is_some_and(|o| cmp.holds(o)))
}

/// A value as a number, the way arithmetic sees it. Text that reads as
/// a number is that number, and anything else that is not null is 0.
enum Num { I(i64), R(f64) }

fn number(v: &Value) -> Option<Num> {
    match v {
        Value::Null => None,
        Value::Int(i) => Some(Num::I(*i)),
        Value::Real(r) => Some(Num::R(*r)),
        Value::Text(t) => {
            let t = t.trim();
            Some(match t.parse::<i64>() {
                Ok(i) => Num::I(i),
                Err(_) => Num::R(t.parse::<f64>().unwrap_or(0.0)),
            })
        }
        Value::Blob(_) => Some(Num::I(0)),
    }
}

fn arithmetic(a: &Value, op: Op, b: &Value) -> Value {
    let (Some(x), Some(y)) = (number(a), number(b)) else { return Value::Null };
    let whole = |i: Option<i64>, r: f64| i.map(Value::Int).unwrap_or(Value::Real(r));
    match (x, y) {
        (Num::I(p), Num::I(q)) => match op {
            Op::Add => whole(p.checked_add(q), p as f64 + q as f64),
            Op::Sub => whole(p.checked_sub(q), p as f64 - q as f64),
            Op::Mul => whole(p.checked_mul(q), p as f64 * q as f64),
            Op::Div => {
                if q == 0 { return Value::Null; }
                whole(p.checked_div(q), p as f64 / q as f64)
            }
            _ => Value::Null,
        },
        (x, y) => {
            let p = match x { Num::I(i) => i as f64, Num::R(r) => r };
            let q = match y { Num::I(i) => i as f64, Num::R(r) => r };
            match op {
                Op::Add => Value::Real(p + q),
                Op::Sub => Value::Real(p - q),
                Op::Mul => Value::Real(p * q),
                Op::Div => if q == 0.0 { Value::Null } else { Value::Real(p / q) },
                _ => Value::Null,
            }
        }
    }
}

// ── Running ────────────────────────────────────────────────────────────

/// One row of the query: the key and row of each table it names.
type Sides<'a> = [(i64, &'a Row)];

fn pick_of(pick: Pick, sides: &Sides) -> Value {
    match pick {
        Pick::Key(s) => sides.get(s as usize).map_or(Value::Null, |(k, _)| Value::Int(*k)),
        Pick::Col(s, i) => sides.get(s as usize).map_or(Value::Null, |(_, r)| r[i].clone()),
    }
}

/// The value of an expression, without copying it where it is a stored
/// column, a written-in value or a `?` slot. LIKE, IN and the functions
/// only look at a value, and a long text is not copied for that.
fn look<'a>(e: &'a E, sides: &Sides<'a>, params: &'a [Value], aggs: &[Value]) -> Result<Cow<'a, Value>> {
    Ok(match e {
        E::Lit(v) => Cow::Borrowed(v),
        E::Param(n) => Cow::Borrowed(params.get(*n).ok_or_else(|| err("a value is missing"))?),
        E::Pick(Pick::Col(s, i)) => match sides.get(*s as usize) {
            Some((_, row)) => Cow::Borrowed(&row[*i]),
            None => Cow::Owned(Value::Null),
        },
        _ => Cow::Owned(eval(e, sides, params, aggs)?),
    })
}

/// Work an expression out against a row.
fn eval(e: &E, sides: &Sides, params: &[Value], aggs: &[Value]) -> Result<Value> {
    Ok(match e {
        E::Lit(v) => v.clone(),
        E::Param(n) => params.get(*n).cloned().ok_or_else(|| err("a value is missing"))?,
        E::Pick(p) => pick_of(*p, sides),
        E::Agg(i) => aggs.get(*i).cloned().unwrap_or(Value::Null),
        E::Not(x) => from_truth(truth(&eval(x, sides, params, aggs)?).map(|b| !b)),
        E::IsNull(x, not) => Value::Int(((eval(x, sides, params, aggs)? == Value::Null) != *not) as i64),
        E::Between { what, low, high, not } => {
            let w = eval(what, sides, params, aggs)?;
            let above = compare(&w, &eval(low, sides, params, aggs)?, Cmp::Ge);
            let below = compare(&w, &eval(high, sides, params, aggs)?, Cmp::Le);
            let both = and3(above, below);
            from_truth(if *not { both.map(|b| !b) } else { both })
        }
        E::Coalesce(v) => {
            for x in v {
                let got = eval(x, sides, params, aggs)?;
                if got != Value::Null { return Ok(got); }
            }
            Value::Null
        }
        E::Bin(a, op, b) => {
            let a = eval(a, sides, params, aggs)?;
            match op {
                // AND and OR can be settled by one side, so the other is
                // only worked out when it has to be.
                Op::And => {
                    let ta = truth(&a);
                    if ta == Some(false) { return Ok(Value::Int(0)); }
                    from_truth(and3(ta, truth(&eval(b, sides, params, aggs)?)))
                }
                Op::Or => {
                    let ta = truth(&a);
                    if ta == Some(true) { return Ok(Value::Int(1)); }
                    from_truth(or3(ta, truth(&eval(b, sides, params, aggs)?)))
                }
                Op::Cmp(c) => from_truth(compare(&a, &eval(b, sides, params, aggs)?, *c)),
                _ => arithmetic(&a, *op, &eval(b, sides, params, aggs)?),
            }
        }
        E::In { what, list, not } => {
            // False until one of the list is equal. A null on either
            // side leaves it unknown, unless one is equal after all.
            let w = look(what, sides, params, aggs)?;
            let mut found = Some(false);
            for item in list {
                match compare(&w, &*look(item, sides, params, aggs)?, Cmp::Eq) {
                    Some(true) => {
                        found = Some(true);
                        break;
                    }
                    Some(false) => {}
                    None => found = None,
                }
            }
            from_truth(if *not { found.map(|b| !b) } else { found })
        }
        E::Like { what, pattern, not } => {
            let w = look(what, sides, params, aggs)?;
            let p = look(pattern, sides, params, aggs)?;
            match (func::text(&w), func::text(&p)) {
                (Some(text), Some(pattern)) => Value::Int((func::like(&pattern, &text) != *not) as i64),
                _ => Value::Null,
            }
        }
        E::Case { whens, otherwise } => {
            for (when, then) in whens {
                if truth(&eval(when, sides, params, aggs)?) == Some(true) {
                    return eval(then, sides, params, aggs);
                }
            }
            match otherwise {
                Some(x) => eval(x, sides, params, aggs)?,
                None => Value::Null,
            }
        }
        E::Call(f, args) => {
            // No function takes more than three values, so they sit in
            // an array and nothing is set aside for them.
            let mut values = [Cow::Owned(Value::Null), Cow::Owned(Value::Null), Cow::Owned(Value::Null)];
            for (slot, arg) in values.iter_mut().zip(args) {
                *slot = look(arg, sides, params, aggs)?;
            }
            func::call(*f, &values[..args.len().min(3)])?
        }
    })
}

/// A value that needs no row to work out, which is what an INSERT
/// stores. Nearly always it is a `?` slot or written into the
/// statement, and then it is read straight out.
#[inline]
fn alone(e: &E, params: &[Value]) -> Result<Value> {
    match e {
        E::Param(n) => params.get(*n).cloned().ok_or_else(|| err("a value is missing")),
        E::Lit(v) => Ok(v.clone()),
        _ => eval(e, &[], params, &[]),
    }
}

fn and3(a: Option<bool>, b: Option<bool>) -> Option<bool> {
    match (a, b) {
        (Some(false), _) | (_, Some(false)) => Some(false),
        (Some(true), Some(true)) => Some(true),
        _ => None,
    }
}

fn or3(a: Option<bool>, b: Option<bool>) -> Option<bool> {
    match (a, b) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (Some(false), Some(false)) => Some(false),
        _ => None,
    }
}

#[inline]
fn passes(tests: &[Test], sides: &Sides, params: &[Value]) -> Result<bool> {
    for t in tests {
        let right = value_of(&t.arg, params)?;
        let held;
        let left = match t.pick {
            Pick::Col(s, i) => &sides[s as usize].1[i],
            Pick::Key(_) => {
                held = pick_of(t.pick, sides);
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

/// The conditions that had to be worked out in full.
#[inline]
fn holds(exprs: &[E], sides: &Sides, params: &[Value]) -> Result<bool> {
    for e in exprs {
        if truth(&eval(e, sides, params, &[])?) != Some(true) { return Ok(false); }
    }
    Ok(true)
}

fn value_of<'a>(a: &'a Arg, params: &'a [Value]) -> Result<&'a Value> {
    match a {
        Arg::Lit(v) => Ok(v),
        Arg::Param(n) => params.get(*n).ok_or_else(|| err("a value is missing")),
    }
}

/// The values of several arguments, or None when any is null, which
/// matches nothing.
fn values_of(args: &[Arg], params: &[Value]) -> Result<Option<Vec<Value>>> {
    let mut out = Vec::with_capacity(args.len());
    for a in args {
        let v = value_of(a, params)?;
        if *v == Value::Null { return Ok(None); }
        out.push(v.clone());
    }
    Ok(Some(out))
}

impl Statement {
    /// Whether this one reads or changes something.
    pub fn kind(&self) -> Kind2 {
        match self.plan {
            Plan::Select { .. } | Plan::Later { reads: true, .. } => Kind2::Reads,
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
            other => match key_of(other) {
                Some(key) => key,
                None => return Ok(None),
            },
        };
        Ok(db.table(fast.table).get_at(key, fast.col))
    }

    /// Run a statement that reads.
    pub fn query(&self, db: &Db, params: &[Value]) -> Result<Outcome> {
        self.check(params)?;
        if let Plan::Later { stmt, reads: true } = &self.plan {
            return self.settled(db, stmt, params)?.query(db, params);
        }
        let Plan::Select { table, join, what, filter, distinct, group, order, limit, offset } = &self.plan
        else {
            return Err(err("this statement changes things; use run"));
        };
        let (skip, take) = window(limit, offset, params)?;
        match what {
            What::Aggs { items, aggs, .. } if !group.is_empty() => Ok(Outcome::Rows(grouped(
                db, *table, join, filter, group, items, aggs, order, *distinct, skip, take, params,
            )?)),
            What::Aggs { items, aggs, needs_row } => {
                // An aggregate gives one row back. A LIMIT or an OFFSET
                // can only hide it, never cut the counting short.
                let hidden = take == 0 || skip > 0;
                // `COUNT(*)` with nothing else to check never needs the
                // rows themselves. The table knows how many it holds,
                // and an index knows how many hold a given value.
                // Fetching each row to add one to a counter was eight
                // times slower than SQLite here.
                if join.is_none()
                    && filter.is_plain()
                    && aggs.iter().all(|a| a.func == Func::Count && a.arg.is_none())
                {
                    if let Some(n) = counted_without_rows(db, *table, filter, params)? {
                        if hidden { return Ok(Outcome::Rows(Vec::new())); }
                        let counts = vec![Value::Int(n); aggs.len()];
                        let row = items.iter().map(|e| eval(e, &[], params, &counts)).collect::<Result<Row>>()?;
                        return Ok(Outcome::Rows(vec![row]));
                    }
                }
                let mut state: Vec<AggState> =
                    aggs.iter().map(|a| AggState::new(a.func, a.arg.is_none())).collect();
                let mut last: Vec<(i64, Row)> = Vec::new();
                visit(db, *table, join, filter, params, &mut |sides| {
                    for (s, a) in state.iter_mut().zip(aggs) {
                        match &a.arg {
                            None => s.saw_row(),
                            Some(e) => s.saw(&eval(e, sides, params, &[])?),
                        }
                    }
                    if *needs_row {
                        last = sides.iter().map(|(k, r)| (*k, (*r).clone())).collect();
                    }
                    Ok(true)
                })?;
                if hidden {
                    return Ok(Outcome::Rows(Vec::new()));
                }
                let done: Vec<Value> = state.iter().map(AggState::finish).collect();
                let sides: Vec<(i64, &Row)> = last.iter().map(|(k, r)| (*k, r)).collect();
                let row = items.iter().map(|e| eval(e, &sides, params, &done)).collect::<Result<Row>>()?;
                Ok(Outcome::Rows(vec![row]))
            }
            What::Row(picks) => {
                // A query that asks only for the key, found through an
                // index, is already answered by the index: it holds the
                // keys. Fetching each row to read back the key it was
                // filed under is a walk down the row tree for nothing.
                if !*distinct {
                    if let Some(rows) = keys_from_index(db, join, picks, filter, order, skip, take, params)? {
                        return Ok(Outcome::Rows(rows));
                    }
                }
                let rows = collect(db, *table, join, filter, order, *distinct, skip, take, params, |sides| {
                    Ok(picks.iter().map(|p| pick_of(*p, sides)).collect())
                })?;
                Ok(Outcome::Rows(rows))
            }
            What::Exprs(items) => {
                let rows = collect(db, *table, join, filter, order, *distinct, skip, take, params, |sides| {
                    items.iter().map(|e| eval(e, sides, params, &[])).collect()
                })?;
                Ok(Outcome::Rows(rows))
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
            Plan::Nothing => Ok(Outcome::Done),

            Plan::CreateTable { name, key, columns, foreign_keys, uniques, if_missing } => {
                if *if_missing && db.table_id(name).is_ok() {
                    return Ok(Outcome::Done);
                }
                // The table and its unique indexes are one commit, so a
                // crash cannot leave the one without the others.
                let alone = !db.in_transaction();
                if alone { db.begin(); }
                let made = (|| {
                    let id = db.create_table_full(name, key, columns.clone(), foreign_keys.clone())?;
                    for (n, cols) in uniques {
                        db.create_index_full(n, id, cols.clone(), true)?;
                    }
                    Ok(())
                })();
                if alone {
                    match made {
                        Ok(()) => db.commit()?,
                        Err(e) => { db.rollback(); return Err(e); }
                    }
                } else {
                    made?;
                }
                Ok(Outcome::Done)
            }

            Plan::DropTable { name, if_there } => {
                if *if_there && db.table_id(name).is_err() {
                    return Ok(Outcome::Done);
                }
                db.drop_table(name)?;
                Ok(Outcome::Done)
            }

            Plan::CreateIndex { name, table, columns, unique, if_missing } => {
                if *if_missing && db.index_id(name).is_ok() {
                    return Ok(Outcome::Done);
                }
                let id = db.table_id(table)?;
                let mut cols = Vec::with_capacity(columns.len());
                for column in columns {
                    cols.push(
                        db.table(id)
                            .column_of(column)
                            .ok_or_else(|| err(&format!("{table} has no column called {column}")))?,
                    );
                }
                db.create_index_full(name, id, cols, *unique)?;
                Ok(Outcome::Done)
            }

            Plan::DropIndex { name, if_there } => {
                if *if_there && db.index_id(name).is_err() {
                    return Ok(Outcome::Done);
                }
                db.drop_index(name)?;
                Ok(Outcome::Done)
            }

            Plan::Insert { table, key, values, on_conflict } => {
                let key = match key {
                    Some(e) => match alone(e, params)? {
                        Value::Int(i) => i,
                        Value::Null => db.next_key(*table),
                        other => {
                            return Err(err(&format!(
                                "a key has to be an integer, not {}",
                                other.type_name()
                            )))
                        }
                    },
                    None => db.next_key(*table),
                };
                let row: Row = values.iter().map(|e| alone(e, params)).collect::<Result<Row>>()?;
                match on_conflict {
                    OnConflict::Fail => {}
                    OnConflict::Ignore => {
                        if !db.clashes(*table, key, &row).is_empty() {
                            return Ok(Outcome::Changed(0));
                        }
                    }
                    OnConflict::Replace => {
                        let old = db.clashes(*table, key, &row);
                        if !old.is_empty() {
                            // The rows in the way go, then the new one
                            // comes; if it is refused after all, they
                            // come back.
                            return whole(db, |db| {
                                for k in old { db.delete(*table, k)?; }
                                db.insert(*table, key, row)?;
                                Ok(Outcome::Changed(1))
                            });
                        }
                    }
                }
                db.insert(*table, key, row)?;
                Ok(Outcome::Changed(1))
            }

            Plan::Update { table, sets, filter, from_row } => {
                let keys = self.matching(db, *table, filter, params)?;
                let change = |db: &mut Db| {
                    for key in &keys {
                        if !*from_row {
                            for (col, e) in sets {
                                db.update(*table, *key, *col, eval(e, &[], params, &[])?)?;
                            }
                            continue;
                        }
                        // Every new value is worked out from the row as
                        // it was, so `SET a = b, b = a` swaps them.
                        let fresh: Vec<Value> = {
                            let Some(row) = db.table(*table).get(*key) else { continue };
                            let one = [(*key, row)];
                            sets.iter().map(|(_, e)| eval(e, &one, params, &[])).collect::<Result<_>>()?
                        };
                        for ((col, _), v) in sets.iter().zip(fresh) {
                            db.update(*table, *key, *col, v)?;
                        }
                    }
                    Ok(Outcome::Changed(keys.len()))
                };
                // One row changing one value cannot half fail, and the
                // bookkeeping for taking it back is real work on the
                // commonest write there is.
                if keys.len() == 1 && sets.len() == 1 { change(db) } else { whole(db, change) }
            }

            Plan::Delete { table, filter } => {
                let keys = self.matching(db, *table, filter, params)?;
                let change = |db: &mut Db| {
                    let mut gone = 0;
                    for key in &keys {
                        if db.delete(*table, *key)? { gone += 1; }
                    }
                    Ok(Outcome::Changed(gone))
                };
                if keys.len() == 1 && !db.table(*table).is_pointed_at() {
                    change(db)
                } else {
                    whole(db, change)
                }
            }

            Plan::AddColumn { table, column } => {
                db.add_column(*table, column.clone())?;
                Ok(Outcome::Done)
            }

            Plan::Later { stmt, .. } => self.settled(db, stmt, params)?.run(db, params),

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

    /// This statement with its inner queries answered, ready to run.
    /// It stays out of `run` and `query`, which would otherwise set room
    /// aside for a whole second statement on every call.
    #[cold]
    #[inline(never)]
    fn settled(&self, db: &Db, stmt: &Stmt, params: &[Value]) -> Result<Box<Statement>> {
        let plan = settled(db, stmt, params)?;
        Ok(Box::new(Statement { plan, params: self.params, fast: None }))
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

/// Run a change that touches several rows so that it either all
/// happens or none of it does, whether or not a transaction is open.
fn whole(db: &mut Db, change: impl FnOnce(&mut Db) -> Result<Outcome>) -> Result<Outcome> {
    let mark = db.mark();
    match change(db) {
        Ok(out) => { db.release(mark)?; Ok(out) }
        Err(e) => { db.undo_to(mark); Err(e) }
    }
}

/// How many rows to step over and how many to give back after them. A
/// LIMIT below zero is no limit, the way SQLite has it.
fn window(limit: &Option<E>, offset: &Option<E>, params: &[Value]) -> Result<(usize, usize)> {
    let whole = |e: &Option<E>| -> Result<Option<i64>> {
        match e {
            None => Ok(None),
            Some(e) => match eval(e, &[], params, &[])? {
                Value::Int(i) => Ok(Some(i)),
                _ => Err(err("LIMIT and OFFSET take a whole number")),
            },
        }
    };
    let take = match whole(limit)? {
        Some(n) if n >= 0 => n as usize,
        _ => usize::MAX,
    };
    let skip = whole(offset)?.map_or(0, |n| n.max(0) as usize);
    Ok((skip, take))
}

/// A row as something that can be told apart from another, for DISTINCT.
fn told_apart(row: &Row) -> Vec<SortKey> { row.iter().cloned().map(SortKey).collect() }

/// Put rows in the order asked for, step over the first `skip` and keep
/// the next `take`. Each row comes with the values it is sorted by.
fn in_order(mut rows: Vec<(Vec<SortKey>, Row)>, order: &[Sort], skip: usize, take: usize) -> Vec<Row> {
    if !order.is_empty() {
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
    }
    rows.into_iter().skip(skip).take(take).map(|(_, r)| r).collect()
}

/// The rows a query gives back, made by `make` from each match, in the
/// order asked for and cut to the limit.
#[allow(clippy::too_many_arguments)]
fn collect(
    db: &Db,
    table: TableId,
    join: &Option<Joined>,
    filter: &Where,
    order: &[Sort],
    distinct: bool,
    skip: usize,
    take: usize,
    params: &[Value],
    make: impl Fn(&Sides) -> Result<Row>,
) -> Result<Vec<Row>> {
    let cap = skip.saturating_add(take);
    // The rows given back so far, when none may come back twice.
    let mut seen = BTreeSet::new();
    if order.is_empty() {
        let mut out = Vec::new();
        visit(db, table, join, filter, params, &mut |sides| {
            let row = make(sides)?;
            if !distinct || seen.insert(told_apart(&row)) { out.push(row); }
            Ok(out.len() < cap)
        })?;
        out.truncate(cap);
        if skip > 0 { out.drain(..skip.min(out.len())); }
        return Ok(out);
    }
    // With an ORDER BY, every matching row has to be found before any
    // of them can be left out, so the LIMIT waits until the sorting is
    // done.
    let mut rows: Vec<(Vec<SortKey>, Row)> = Vec::new();
    visit(db, table, join, filter, params, &mut |sides| {
        let row = make(sides)?;
        if distinct && !seen.insert(told_apart(&row)) { return Ok(true); }
        let mut keys = Vec::with_capacity(order.len());
        for s in order { keys.push(SortKey(eval(&s.expr, sides, params, &[])?)); }
        rows.push((keys, row));
        Ok(true)
    })?;
    Ok(in_order(rows, order, skip, take))
}

/// One row for each group of a GROUP BY. A plain column beside the
/// aggregates is read from the first row of its group.
#[allow(clippy::too_many_arguments)]
fn grouped(
    db: &Db,
    table: TableId,
    join: &Option<Joined>,
    filter: &Where,
    group: &[E],
    items: &[E],
    aggs: &[Counting],
    order: &[Sort],
    distinct: bool,
    skip: usize,
    take: usize,
    params: &[Value],
) -> Result<Vec<Row>> {
    // What each group has added up so far, and the first row it saw.
    type Group = (Vec<AggState>, Vec<(i64, Row)>);
    let mut groups: BTreeMap<Vec<SortKey>, Group> = BTreeMap::new();
    visit(db, table, join, filter, params, &mut |sides| {
        let mut key = Vec::with_capacity(group.len());
        for g in group { key.push(SortKey(eval(g, sides, params, &[])?)); }
        let (state, _) = groups.entry(key).or_insert_with(|| {
            (
                aggs.iter().map(|a| AggState::new(a.func, a.arg.is_none())).collect(),
                sides.iter().map(|(k, r)| (*k, (*r).clone())).collect(),
            )
        });
        for (s, a) in state.iter_mut().zip(aggs) {
            match &a.arg {
                None => s.saw_row(),
                Some(e) => s.saw(&eval(e, sides, params, &[])?),
            }
        }
        Ok(true)
    })?;
    let mut seen = BTreeSet::new();
    let mut rows = Vec::with_capacity(groups.len());
    for (state, first) in groups.into_values() {
        let done: Vec<Value> = state.iter().map(AggState::finish).collect();
        let sides: Vec<(i64, &Row)> = first.iter().map(|(k, r)| (*k, r)).collect();
        let row = items.iter().map(|e| eval(e, &sides, params, &done)).collect::<Result<Row>>()?;
        if distinct && !seen.insert(told_apart(&row)) { continue; }
        let mut keys = Vec::with_capacity(order.len());
        for s in order { keys.push(SortKey(eval(&s.expr, &sides, params, &done)?)); }
        rows.push((keys, row));
    }
    Ok(in_order(rows, order, skip, take))
}

/// The rows of a query that wants nothing but the key and finds them
/// through an index on one column. None when the query is not of that
/// shape.
#[allow(clippy::too_many_arguments)]
fn keys_from_index(
    db: &Db,
    join: &Option<Joined>,
    picks: &[Pick],
    filter: &Where,
    order: &[Sort],
    skip: usize,
    take: usize,
    params: &[Value],
) -> Result<Option<Vec<Row>>> {
    if join.is_some() || !filter.is_plain() || picks.is_empty() {
        return Ok(None);
    }
    let Find::Index { index, args } = &filter.find else { return Ok(None) };
    if args.len() != 1 { return Ok(None); }
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
        [one] if one.expr == E::Pick(Pick::Key(0)) => one.desc,
        _ => return Ok(None),
    };
    let value = value_of(&args[0], params)?;
    let mut out = Vec::new();
    if *value == Value::Null { return Ok(Some(out)); }
    let Some((held, rows)) = db.index(*index).filed(value) else { return Ok(Some(out)) };
    let make = |(key, _): (i64, u32)| -> Row {
        picks
            .iter()
            .map(|p| if *p == Pick::Key(0) { Value::Int(key) } else { held.clone() })
            .collect()
    };
    if desc {
        rows.iter().rev().skip(skip).take(take).for_each(|h| out.push(make(h)));
    } else {
        rows.iter().skip(skip).take(take).for_each(|h| out.push(make(h)));
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
        Find::Key(e) => match key_of(value_of(e, params)?) {
            Some(k) => db.table(table).get(k).is_some() as i64,
            None => 0,
        },
        Find::Index { index, args } => match values_of(args, params)? {
            None => 0,
            Some(values) => db.index(*index).rows_for_all(&values).map_or(0, |r| r.len()) as i64,
        },
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

/// Past this a real number is no key at all: 2 to the power of 63.
const PAST_EVERY_KEY: f64 = 9_223_372_036_854_775_808.0;

/// The key a value names, if it names one. A real that is a whole number
/// names the row the integer does, as it would in any other column.
fn key_of(v: &Value) -> Option<i64> {
    match v {
        Value::Int(i) => Some(*i),
        Value::Real(r) if r.fract() == 0.0 && r.abs() < PAST_EVERY_KEY => Some(*r as i64),
        _ => None,
    }
}

/// The first key that `key > v` or `key >= v` lets through. None when
/// no key gets through: null answers nothing, and every number comes
/// before every text, the way [`order`] has it.
fn first_key(cmp: Cmp, v: &Value) -> Option<i64> {
    match v {
        Value::Int(i) if cmp == Cmp::Gt => i.checked_add(1),
        Value::Int(i) => Some(*i),
        Value::Real(r) => {
            let at = if cmp == Cmp::Gt { r.floor() + 1.0 } else { r.ceil() };
            // A number below every key turns into the lowest key.
            (at < PAST_EVERY_KEY).then_some(at as i64)
        }
        _ => None,
    }
}

/// The key that `key < v` or `key <= v` stops in front of. None when no
/// key gets through.
fn end_key(cmp: Cmp, v: &Value) -> Option<i64> {
    match v {
        Value::Int(i) if cmp == Cmp::Lt => Some(*i),
        Value::Int(i) => Some(i.saturating_add(1)),
        Value::Real(r) => {
            let at = if cmp == Cmp::Lt { r.ceil() } else { r.floor() + 1.0 };
            (at > -PAST_EVERY_KEY).then_some(at as i64)
        }
        Value::Null => None,
        Value::Text(_) | Value::Blob(_) => Some(i64::MAX),
    }
}

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
        if !passes(&filter.tests, &one, params)? || !holds(&filter.other, &one, params)? {
            return Ok(true);
        }
        let Some(j) = join else { return each(&one) };
        let value = pick_of(j.left, &one);
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
                if passes(&filter.after_join, &both, params)? && holds(&filter.other_after, &both, params)? {
                    return each(&both);
                }
                Ok(true)
            }
            JoinBy::Index(index) => {
                let Some(rows) = db.index(*index).rows_for(&value) else { return Ok(true) };
                for (k, slot) in rows.iter() {
                    let Some(r) = right.at_slot(slot) else { continue };
                    let both = [(key, row), (k, r)];
                    if (passes(&filter.after_join, &both, params)? && holds(&filter.other_after, &both, params)?) && !each(&both)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            JoinBy::Scan(col) => {
                for (k, r) in right.iter() {
                    if r.get(*col) != Some(&value) { continue; }
                    let both = [(key, row), (k, r)];
                    if (passes(&filter.after_join, &both, params)? && holds(&filter.other_after, &both, params)?) && !each(&both)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
        }
    };

    match &filter.find {
        Find::Key(e) => {
            let Some(key) = key_of(value_of(e, params)?) else { return Ok(()) };
            if let Some(row) = t.get(key) {
                go(key, row)?;
            }
        }
        Find::Index { index, args } => {
            let Some(values) = values_of(args, params)? else { return Ok(()) };
            if let Some(rows) = db.index(*index).rows_for_all(&values) {
                for (key, slot) in rows.iter() {
                    let Some(row) = t.at_slot(slot) else { continue };
                    if !go(key, row)? { break; }
                }
            }
        }
        Find::Range { low, high } => {
            let from = match low {
                Some((cmp, e)) => match first_key(*cmp, value_of(e, params)?) {
                    Some(key) => key,
                    None => return Ok(()),
                },
                None => i64::MIN,
            };
            let to = match high {
                Some((cmp, e)) => match end_key(*cmp, value_of(e, params)?) {
                    Some(key) => key,
                    None => return Ok(()),
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

    fn ids(db: &Db, sql: &str) -> Vec<i64> {
        db.query(sql, &[]).unwrap().rows().iter().map(|r| r[0].as_int().unwrap()).collect()
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
        assert!(!db.prepare("SELECT a + 1 FROM kv WHERE id = 1").unwrap().is_point_lookup());
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
        assert_eq!(ids(&db, "SELECT id FROM kv WHERE id >= 3 AND id < 6"), vec![3, 4, 5]);
        assert_eq!(ids(&db, "SELECT id FROM kv WHERE id BETWEEN 3 AND 5"), vec![3, 4, 5]);
        assert_eq!(ids(&db, "SELECT id FROM kv WHERE 3 <= id AND 6 > id"), vec![3, 4, 5]);
    }

    #[test]
    fn a_test_on_another_column_walks_and_filters() {
        let db = kv();
        assert_eq!(ids(&db, "SELECT id FROM kv WHERE a > 70"), vec![8, 9]);
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
            assert_eq!(ids(&db, sql), want, "{sql}");
        }
    }

    #[test]
    fn or_not_is_null_and_brackets_all_mean_what_they_say() {
        let mut db = kv();
        db.execute("UPDATE kv SET a = NULL WHERE id = 5", &[]).unwrap();
        assert_eq!(ids(&db, "SELECT id FROM kv WHERE id = 1 OR id = 8"), vec![1, 8]);
        assert_eq!(ids(&db, "SELECT id FROM kv WHERE (id = 1 OR id = 8) AND a > 50"), vec![8]);
        assert_eq!(ids(&db, "SELECT id FROM kv WHERE a IS NULL"), vec![5]);
        assert_eq!(ids(&db, "SELECT id FROM kv WHERE a IS NOT NULL AND id > 6"), vec![7, 8, 9]);
        assert_eq!(ids(&db, "SELECT id FROM kv WHERE NOT id < 8"), vec![8, 9]);
        assert_eq!(ids(&db, "SELECT id FROM kv WHERE id NOT BETWEEN 1 AND 8"), vec![0, 9]);
        // Null in an OR is unknown, and unknown OR true is true.
        assert_eq!(ids(&db, "SELECT id FROM kv WHERE a > 40 OR id = 5"), vec![5, 6, 7, 8, 9]);
        // But NOT of unknown is still unknown, so row 5 is left out.
        assert_eq!(ids(&db, "SELECT id FROM kv WHERE NOT a > 40"), vec![0, 1, 2, 3, 4]);
        assert_eq!(ids(&db, "SELECT id FROM kv WHERE a + 5 > 80"), vec![8, 9]);
        assert_eq!(ids(&db, "SELECT id FROM kv WHERE COALESCE(a, 999) > 85"), vec![5, 9]);
        assert_eq!(ids(&db, "SELECT id FROM kv WHERE c = 'row 3' OR c = 'row 4'"), vec![3, 4]);
    }

    #[test]
    fn expressions_come_out_in_the_select_list() {
        let mut db = kv();
        db.execute("UPDATE kv SET a = NULL WHERE id = 5", &[]).unwrap();
        let out = db.query("SELECT a + 1, a * 2.0, id / 3, COALESCE(a, id), a IS NULL, 7 / 0 FROM kv WHERE id = 5", &[]).unwrap();
        assert_eq!(
            out.rows()[0],
            vec![Value::Null, Value::Null, Value::Int(1), Value::Int(5), Value::Int(1), Value::Null]
        );
        let out = db.query("SELECT a + 1, a * 2.0, id / 3, COALESCE(a, id), a IS NULL FROM kv WHERE id = 7", &[]).unwrap();
        assert_eq!(
            out.rows()[0],
            vec![Value::Int(71), Value::Real(140.0), Value::Int(2), Value::Int(70), Value::Int(0)]
        );
        let out = db.query("SELECT COUNT(*) > 0, COUNT(*) FROM kv WHERE id > 100", &[]).unwrap();
        assert_eq!(out.rows()[0], vec![Value::Int(0), Value::Int(0)]);
        let out = db.query("SELECT COUNT(*) > 0 FROM kv WHERE id = 3", &[]).unwrap();
        assert_eq!(out.rows()[0], vec![Value::Int(1)]);
        // A plain column beside an aggregate comes from the last row.
        let out = db.query("SELECT id, MAX(a) FROM kv WHERE id < 3", &[]).unwrap();
        assert_eq!(out.rows()[0], vec![Value::Int(2), Value::Int(20)]);
        assert!(db.prepare("SELECT a FROM kv WHERE COUNT(*) > 1").is_err());
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
    fn update_changes_only_what_it_matched_and_can_work_from_the_row() {
        let mut db = kv();
        let n = db
            .execute("UPDATE kv SET a = ?1 WHERE id = ?2", &[Value::Int(999), Value::Int(3)])
            .unwrap();
        assert_eq!(n.changed(), 1);
        let out = db.execute("SELECT a FROM kv WHERE id = 3", &[]).unwrap();
        assert_eq!(out.rows(), &[vec![Value::Int(999)]]);
        let out = db.execute("SELECT a FROM kv WHERE id = 4", &[]).unwrap();
        assert_eq!(out.rows(), &[vec![Value::Int(40)]]);
        db.execute("UPDATE kv SET a = 1 - a, b = a * 1.0 WHERE id = 4", &[]).unwrap();
        let out = db.execute("SELECT a, b FROM kv WHERE id = 4", &[]).unwrap();
        assert_eq!(out.rows(), &[vec![Value::Int(-39), Value::Real(40.0)]]);
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
        for sql in ["SELECT id FROM t WHERE a = 5", "SELECT id FROM t WHERE a <> 5", "SELECT id FROM t WHERE a = NULL"] {
            let keys = ids(&db, sql);
            assert!(!keys.contains(&1), "{sql} should not find the null row");
        }
    }

    #[test]
    fn a_missing_column_is_null_or_its_default() {
        let mut db = Db::new();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, c TEXT, d INTEGER DEFAULT 7, e TEXT DEFAULT 'x')", &[]).unwrap();
        db.execute("INSERT INTO t (id, c) VALUES (1, 'x')", &[]).unwrap();
        let out = db.execute("SELECT a, d, e FROM t WHERE id = 1", &[]).unwrap();
        assert_eq!(out.rows(), &[vec![Value::Null, Value::Int(7), Value::Text("x".into())]]);
        assert!(db.execute("CREATE TABLE bad (id INTEGER PRIMARY KEY, a INTEGER DEFAULT 'no')", &[]).is_err());
    }

    #[test]
    fn a_key_left_out_is_handed_out() {
        let mut db = kv();
        let n = db.execute("INSERT INTO kv (a) VALUES (1)", &[]).unwrap();
        assert_eq!(n.changed(), 1);
        assert_eq!(db.last_insert_key(), 10);
        db.execute("INSERT INTO kv (id, a) VALUES (NULL, 2)", &[]).unwrap();
        assert_eq!(db.last_insert_key(), 11);
        db.execute("INSERT INTO kv (id, a) VALUES (50, 2)", &[]).unwrap();
        db.execute("INSERT INTO kv (a) VALUES (3)", &[]).unwrap();
        assert_eq!(db.last_insert_key(), 51);
        // A key is never handed out twice while the database is open.
        db.execute("DELETE FROM kv WHERE id = 51", &[]).unwrap();
        db.execute("INSERT INTO kv (a) VALUES (3)", &[]).unwrap();
        assert_eq!(db.last_insert_key(), 52);
    }

    #[test]
    fn a_text_key_is_a_unique_index_over_a_hidden_rowid() {
        let mut db = Db::new();
        db.execute("CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL, at INTEGER)", &[]).unwrap();
        db.execute("INSERT INTO settings (key, value) VALUES ('a', '1')", &[]).unwrap();
        assert!(db.execute("INSERT INTO settings (key, value) VALUES ('a', '2')", &[]).is_err());
        let n = db.execute("INSERT OR IGNORE INTO settings (key, value) VALUES ('a', '2')", &[]).unwrap();
        assert_eq!(n.changed(), 0);
        let n = db.execute("INSERT OR REPLACE INTO settings (key, value) VALUES ('a', '3')", &[]).unwrap();
        assert_eq!(n.changed(), 1);
        db.execute("INSERT INTO settings VALUES ('b', '4', 9)", &[]).unwrap();
        let out = db.query("SELECT * FROM settings ORDER BY key", &[]).unwrap();
        assert_eq!(
            out.rows(),
            &[
                vec![Value::Text("a".into()), Value::Text("3".into()), Value::Null],
                vec![Value::Text("b".into()), Value::Text("4".into()), Value::Int(9)],
            ]
        );
        // The lookup goes through the index, and rowid is there to ask for.
        let out = db.query("SELECT value, rowid FROM settings WHERE key = 'a'", &[]).unwrap();
        assert_eq!(out.rows(), &[vec![Value::Text("3".into()), Value::Int(2)]]);
        assert_eq!(db.query("SELECT COUNT(*) FROM settings WHERE key = 'zzz'", &[]).unwrap().rows()[0], vec![Value::Int(0)]);
        db.execute("UPDATE settings SET value = '5' WHERE key = 'b'", &[]).unwrap();
        assert!(db.execute("UPDATE settings SET key = 'a' WHERE key = 'b'", &[]).is_err());
        db.execute("DELETE FROM settings WHERE key = 'a'", &[]).unwrap();
        assert_eq!(db.query("SELECT COUNT(*) FROM settings", &[]).unwrap().rows()[0], vec![Value::Int(1)]);
    }

    #[test]
    fn a_key_over_two_columns_works_the_same_way() {
        let mut db = Db::new();
        db.execute(
            "CREATE TABLE w (date TEXT NOT NULL, hour INTEGER, data TEXT, PRIMARY KEY(date, hour))",
            &[],
        )
        .unwrap();
        db.execute("INSERT INTO w (date, hour, data) VALUES ('mon', 1, 'a')", &[]).unwrap();
        db.execute("INSERT INTO w (date, hour, data) VALUES ('mon', 2, 'b')", &[]).unwrap();
        db.execute("INSERT INTO w (date, hour, data) VALUES ('tue', 1, 'c')", &[]).unwrap();
        assert!(db.execute("INSERT INTO w (date, hour, data) VALUES ('mon', 1, 'x')", &[]).is_err());
        db.execute("INSERT OR REPLACE INTO w (date, hour, data) VALUES ('mon', 1, 'x')", &[]).unwrap();
        let out = db.query("SELECT data FROM w WHERE date = 'mon' AND hour = 1", &[]).unwrap();
        assert_eq!(out.rows(), &[vec![Value::Text("x".into())]]);
        let out = db.query("SELECT data FROM w WHERE hour = 1 AND date = 'mon' AND data <> 'q'", &[]).unwrap();
        assert_eq!(out.rows(), &[vec![Value::Text("x".into())]]);
        assert_eq!(db.query("SELECT COUNT(*) FROM w", &[]).unwrap().rows()[0], vec![Value::Int(3)]);
        // Half a key is not the key: this walks, and finds two.
        assert_eq!(db.query("SELECT COUNT(*) FROM w WHERE date = 'mon'", &[]).unwrap().rows()[0], vec![Value::Int(2)]);
        // Part of the key null: nothing to clash with.
        db.execute("INSERT INTO w (date, data) VALUES ('mon', 'n')", &[]).unwrap();
        db.execute("INSERT INTO w (date, data) VALUES ('mon', 'n')", &[]).unwrap();
        assert_eq!(db.query("SELECT COUNT(*) FROM w WHERE hour IS NULL", &[]).unwrap().rows()[0], vec![Value::Int(2)]);
    }

    #[test]
    fn a_unique_column_and_a_unique_index_are_kept() {
        let mut db = Db::new();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER UNIQUE, b INTEGER, c INTEGER, UNIQUE(b, c))", &[]).unwrap();
        db.execute("INSERT INTO t (id, a, b, c) VALUES (1, 1, 1, 1)", &[]).unwrap();
        assert!(db.execute("INSERT INTO t (id, a, b, c) VALUES (2, 1, 2, 2)", &[]).is_err());
        assert!(db.execute("INSERT INTO t (id, a, b, c) VALUES (2, 2, 1, 1)", &[]).is_err());
        db.execute("INSERT INTO t (id, a, b, c) VALUES (2, 2, 1, 2)", &[]).unwrap();
        // A replace on a clash in a unique index takes the old row out.
        db.execute("INSERT OR REPLACE INTO t (id, a, b, c) VALUES (3, 2, 9, 9)", &[]).unwrap();
        assert_eq!(ids(&db, "SELECT id FROM t ORDER BY id"), vec![1, 3]);
        db.execute("CREATE UNIQUE INDEX t_c ON t (c)", &[]).unwrap();
        assert!(db.execute("INSERT INTO t (id, a, b, c) VALUES (4, 4, 4, 9)", &[]).is_err());
        db.execute("CREATE UNIQUE INDEX t_b ON t (b)", &[]).unwrap();
        db.execute("INSERT INTO t (id, a, b, c) VALUES (5, 5, 7, 5)", &[]).unwrap();
        assert!(db.execute("INSERT INTO t (id, a, b, c) VALUES (6, 6, 7, 6)", &[]).is_err());
        db.execute("DROP INDEX t_b", &[]).unwrap();
        db.execute("INSERT INTO t (id, a, b, c) VALUES (6, 6, 7, 6)", &[]).unwrap();
        assert!(db.execute("CREATE UNIQUE INDEX t_b ON t (b)", &[]).is_err(), "two rows hold b = 7");
        assert!(db.index_id("t_b").is_err());
    }

    #[test]
    fn a_foreign_key_cascades_through_sql() {
        let mut db = Db::new();
        db.execute_batch(
            "CREATE TABLE calendars (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL);
             CREATE TABLE events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                calendar_id INTEGER NOT NULL,
                title TEXT NOT NULL,
                FOREIGN KEY(calendar_id) REFERENCES calendars(id) ON DELETE CASCADE
             );
             CREATE INDEX IF NOT EXISTS idx_events_calendar ON events(calendar_id);
             PRAGMA foreign_keys = ON;
             INSERT INTO calendars (name) VALUES ('home');
             INSERT INTO calendars (name) VALUES ('work');
             INSERT INTO events (calendar_id, title) VALUES (1, 'a');
             INSERT INTO events (calendar_id, title) VALUES (1, 'b');
             INSERT INTO events (calendar_id, title) VALUES (2, 'c');",
        )
        .unwrap();
        assert!(db.execute("INSERT INTO events (calendar_id, title) VALUES (9, 'x')", &[]).is_err());
        let n = db.execute("DELETE FROM calendars WHERE id = 1", &[]).unwrap();
        assert_eq!(n.changed(), 1, "the cascade is not counted");
        assert_eq!(ids(&db, "SELECT id FROM events"), vec![3]);
        let out = db
            .query(
                "SELECT e.title, c.name FROM events e JOIN calendars c ON c.id = e.calendar_id WHERE c.name = 'work' AND (e.title IS NULL OR e.title > 'b')",
                &[],
            )
            .unwrap();
        assert_eq!(out.rows(), &[vec![Value::Text("c".into()), Value::Text("work".into())]]);
        assert!(db.execute("DROP TABLE calendars", &[]).is_err());
        db.execute("DROP TABLE events", &[]).unwrap();
        db.execute("DROP TABLE IF EXISTS events", &[]).unwrap();
        db.execute("DROP TABLE calendars", &[]).unwrap();
    }

    #[test]
    fn an_index_over_two_columns_answers_like_a_walk() {
        let mut db = kv();
        db.execute("UPDATE kv SET c = 'same' WHERE id > 5", &[]).unwrap();
        db.execute("UPDATE kv SET a = 0 WHERE id = 7 OR id = 9", &[]).unwrap();
        let ask = "SELECT id FROM kv WHERE a = 0 AND c = 'same' ORDER BY id";
        let walked = ids(&db, ask);
        assert_eq!(walked, vec![7, 9]);
        db.execute("CREATE INDEX kv_ac ON kv (a, c)", &[]).unwrap();
        assert_eq!(ids(&db, ask), walked);
        assert_eq!(ids(&db, "SELECT id FROM kv WHERE c = 'same' AND a = 0 AND id > 7"), vec![9]);
        assert_eq!(db.query("SELECT COUNT(*) FROM kv WHERE a = 0 AND c = 'same'", &[]).unwrap().rows()[0], vec![Value::Int(2)]);
        db.execute("UPDATE kv SET a = 5 WHERE id = 7", &[]).unwrap();
        assert_eq!(ids(&db, ask), vec![9]);
        db.execute("DELETE FROM kv WHERE id = 9", &[]).unwrap();
        assert_eq!(ids(&db, ask), Vec::<i64>::new());
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
        assert!(db.prepare("INSERT INTO kv (a) VALUES (b)").is_err());
        assert!(db.prepare("SELECT a FROM kv ORDER BY nope").is_err());
        assert!(db.prepare("SELECT x.a FROM kv").is_err());
    }

    // ── Phase 4 ────────────────────────────────────────────────────────

    #[test]
    fn order_by_sorts_both_ways_and_on_several_columns() {
        let mut db = kv();
        db.execute("UPDATE kv SET a = 0 WHERE id = 3", &[]).unwrap();
        // Rows 0 and 3 both hold zero. The sort is stable, so they keep
        // the order they were found in. SQL promises nothing about ties.
        assert_eq!(ids(&db, "SELECT id FROM kv ORDER BY a DESC"), vec![9, 8, 7, 6, 5, 4, 2, 1, 0, 3]);
        assert_eq!(ids(&db, "SELECT id FROM kv ORDER BY a, id DESC LIMIT 3"), vec![3, 0, 1]);
        assert_eq!(ids(&db, "SELECT id FROM kv ORDER BY 0 - id LIMIT 2"), vec![9, 8]);
    }

    #[test]
    fn a_limit_after_an_order_by_takes_the_first_rows_of_the_sorted_lot() {
        let db = kv();
        assert_eq!(ids(&db, "SELECT id FROM kv ORDER BY id DESC LIMIT 2"), vec![9, 8]);
    }

    #[test]
    fn nulls_sort_first() {
        let mut db = Db::new();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER)", &[]).unwrap();
        for (k, v) in [(1, "5"), (2, "NULL"), (3, "1")] {
            db.execute(&format!("INSERT INTO t (id, a) VALUES ({k}, {v})"), &[]).unwrap();
        }
        assert_eq!(ids(&db, "SELECT id FROM t ORDER BY a"), vec![2, 3, 1]);
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
        // And an aggregate can be over an expression.
        let out = db.query("SELECT SUM(a * 2), MAX(COALESCE(a, 100)) FROM t", &[]).unwrap();
        assert_eq!(out.rows()[0], vec![Value::Int(16), Value::Int(100)]);
    }

    #[test]
    fn an_index_gives_the_same_answers_as_a_walk() {
        let mut db = kv();
        db.execute("UPDATE kv SET a = 50 WHERE id = 7", &[]).unwrap();
        let without = ids(&db, "SELECT id FROM kv WHERE a = 50 ORDER BY id");
        assert_eq!(without, vec![5, 7]);
        db.execute("CREATE INDEX kv_a ON kv (a)", &[]).unwrap();
        assert_eq!(ids(&db, "SELECT id FROM kv WHERE a = 50 ORDER BY id"), without);
    }

    #[test]
    fn asking_only_for_the_key_gives_the_same_rows_either_way() {
        let mut db = kv();
        db.execute("UPDATE kv SET a = 50 WHERE id = 7", &[]).unwrap();
        db.execute("UPDATE kv SET a = 50 WHERE id = 2", &[]).unwrap();
        let walked = ids(&db, "SELECT id FROM kv WHERE a = 50");
        db.execute("CREATE INDEX kv_a ON kv (a)", &[]).unwrap();
        let covered = ids(&db, "SELECT id FROM kv WHERE a = 50");
        assert_eq!(covered, walked);
        assert_eq!(covered, vec![2, 5, 7]);
        // Sorted the other way, and cut short.
        assert_eq!(ids(&db, "SELECT id FROM kv WHERE a = 50 ORDER BY id DESC LIMIT 2"), vec![7, 5]);
        // Asking for the key twice gives it twice.
        let twice = db.query("SELECT id, id FROM kv WHERE a = 50", &[]).unwrap();
        assert_eq!(twice.rows()[0], vec![Value::Int(2), Value::Int(2)]);
        // Asking for anything else still goes to the rows.
        let c = db.query("SELECT c FROM kv WHERE a = 50", &[]).unwrap();
        assert_eq!(c.rows().len(), 3);
        assert_eq!(c.rows()[0], vec![Value::Text("row 2".into())]);
        // And a test on another column is still applied.
        assert_eq!(ids(&db, "SELECT id FROM kv WHERE a = 50 AND id > 3"), vec![5, 7]);
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
        let through_index = |db: &Db, v: i64| -> Vec<i64> { ids(db, &format!("SELECT id FROM kv WHERE a = {v}")) };
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
        let at = |db: &Db, v: i64| -> Vec<i64> { ids(db, &format!("SELECT id FROM kv WHERE a = {v} ORDER BY id")) };
        assert_eq!(at(&db, 30), vec![3]);
        db.execute("UPDATE kv SET a = 30 WHERE id = 8", &[]).unwrap();
        assert_eq!(at(&db, 30), vec![3, 8]);
        db.execute("DELETE FROM kv WHERE id = 3", &[]).unwrap();
        assert_eq!(at(&db, 30), vec![8]);
        db.execute("INSERT INTO kv (id, a) VALUES (99, 30)", &[]).unwrap();
        assert_eq!(at(&db, 30), vec![8, 99]);
        db.execute("DROP INDEX kv_a", &[]).unwrap();
        assert_eq!(at(&db, 30), vec![8, 99]);
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
        let c = db.query("SELECT o.id FROM ord o JOIN cust AS c ON c.id = o.who", &[]).unwrap();
        assert_eq!(a, b);
        assert_eq!(a, c);
        assert!(db.prepare("SELECT x FROM ord JOIN cust ON ord.who > cust.id").is_err());
        // A condition in the ON that is not the join is still applied.
        let d = ids(&db, "SELECT ord.id FROM ord JOIN cust ON ord.who = cust.id AND cust.name = 'bob'");
        assert_eq!(d, vec![12]);
    }

    #[test]
    fn a_join_can_be_filtered_on_either_side() {
        let db = two();
        assert_eq!(
            ids(&db, "SELECT ord.id FROM ord JOIN cust ON ord.who = cust.id WHERE cust.name = 'alice'"),
            vec![10, 11]
        );
        let out = db
            .query("SELECT cust.name FROM ord JOIN cust ON ord.who = cust.id WHERE ord.amount > 6", &[])
            .unwrap();
        assert_eq!(out.rows().len(), 2);
        assert_eq!(
            ids(&db, "SELECT ord.id FROM ord JOIN cust ON ord.who = cust.id WHERE cust.name = 'alice' OR ord.amount > 8"),
            vec![10, 11, 12]
        );
    }

    #[test]
    fn a_join_onto_a_plain_column_works_with_and_without_an_index() {
        let mut db = two();
        let ask = "SELECT ord.id FROM cust JOIN ord ON cust.id = ord.who ORDER BY ord.id";
        let without = ids(&db, ask);
        assert_eq!(without, vec![10, 11, 12]);
        db.execute("CREATE INDEX ord_who ON ord (who)", &[]).unwrap();
        assert_eq!(ids(&db, ask), without);
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
    fn a_table_has_at_most_one_primary_key() {
        let mut db = Db::new();
        assert!(db.execute("CREATE TABLE t (a INTEGER PRIMARY KEY, b INTEGER PRIMARY KEY)", &[]).is_err());
        assert!(db.execute("CREATE TABLE t (a INTEGER PRIMARY KEY, b INTEGER, PRIMARY KEY(b))", &[]).is_err());
        assert!(db.execute("CREATE TABLE t (a INTEGER, PRIMARY KEY(nope))", &[]).is_err());
        db.execute("CREATE TABLE t (a INTEGER PRIMARY KEY)", &[]).unwrap();
        assert!(db.execute("CREATE TABLE t (a INTEGER PRIMARY KEY)", &[]).is_err());
        db.execute("CREATE TABLE IF NOT EXISTS t (a INTEGER PRIMARY KEY)", &[]).unwrap();
        // No key at all is fine: the rows still have a rowid.
        db.execute("CREATE TABLE u (a INTEGER, b TEXT)", &[]).unwrap();
        db.execute("INSERT INTO u VALUES (1, 'x')", &[]).unwrap();
        db.execute("INSERT INTO u (b) VALUES ('y')", &[]).unwrap();
        let out = db.query("SELECT rowid, a, b FROM u ORDER BY rowid", &[]).unwrap();
        assert_eq!(out.rows()[1], vec![Value::Int(2), Value::Null, Value::Text("y".into())]);
        let out = db.query("SELECT * FROM u WHERE rowid = 1", &[]).unwrap();
        assert_eq!(out.rows()[0], vec![Value::Int(1), Value::Text("x".into())]);
    }

    fn rows(db: &Db, sql: &str) -> Vec<Row> { db.query(sql, &[]).unwrap().rows().to_vec() }
    fn int(i: i64) -> Value { Value::Int(i) }
    fn text(s: &str) -> Value { Value::Text(s.to_string()) }
    const NONE: Vec<i64> = Vec::new();

    #[test]
    fn a_key_is_found_by_a_number_that_is_not_whole() {
        let db = kv();
        // The key answers as any other column of numbers would.
        for (key, plain) in [
            ("id > 2.5", vec![3, 4, 5, 6, 7, 8, 9]),
            ("id >= 2.5", vec![3, 4, 5, 6, 7, 8, 9]),
            ("id > 7.0", vec![8, 9]),
            ("id >= 7.0", vec![7, 8, 9]),
            ("id < 2.5", vec![0, 1, 2]),
            ("id <= 2.5", vec![0, 1, 2]),
            ("id < 2.0", vec![0, 1]),
            ("id <= 2.0", vec![0, 1, 2]),
            ("id BETWEEN 1.5 AND 3.5", vec![2, 3]),
            ("id = 2.0", vec![2]),
            ("id = 2.5", vec![]),
            // Every number comes before every text, and null answers nothing.
            ("id < 'abc' AND id > 7", vec![8, 9]),
            ("id > 'abc'", vec![]),
            ("id > NULL", vec![]),
            ("id < NULL", vec![]),
        ] {
            assert_eq!(ids(&db, &format!("SELECT id FROM kv WHERE {key}")), plain, "{key}");
            // The same question of a plain column: a is ten times the key.
            let same = key.replace("id", "a / 10");
            assert_eq!(ids(&db, &format!("SELECT id FROM kv WHERE {same}")), plain, "{same}");
        }
        // A number too big to be a key is above every key, or below.
        let big = Value::Real(1e30);
        let small = Value::Real(-1e30);
        for (sql, with, want) in [
            ("SELECT id FROM kv WHERE id > ?1 AND id < 1.5", &small, vec![0, 1]),
            ("SELECT id FROM kv WHERE id < ?1 AND id > 7", &big, vec![8, 9]),
            ("SELECT id FROM kv WHERE id > ?1", &big, vec![]),
            ("SELECT id FROM kv WHERE id < ?1", &small, vec![]),
            ("SELECT id FROM kv WHERE id = ?1", &big, vec![]),
        ] {
            let got = db.query(sql, std::slice::from_ref(with)).unwrap();
            assert_eq!(got.rows(), want.into_iter().map(|i| vec![int(i)]).collect::<Vec<Row>>(), "{sql}");
        }
        assert_eq!(rows(&db, "SELECT COUNT(*) FROM kv WHERE id = 3.0"), vec![vec![int(1)]]);
        let s = db.prepare("SELECT a FROM kv WHERE id = ?1").unwrap();
        assert_eq!(s.value(&db, &[Value::Real(4.0)]).unwrap(), Some(&int(40)));
        assert_eq!(s.value(&db, &[Value::Real(4.5)]).unwrap(), None);
        let s = db.prepare("SELECT id FROM kv WHERE id > ?1 AND id <= ?2").unwrap();
        assert_eq!(s.query(&db, &[Value::Real(6.5), Value::Real(8.0)]).unwrap().rows(), &[vec![int(7)], vec![int(8)]]);
    }

    #[test]
    fn like_and_in_pick_rows() {
        let db = kv();
        assert_eq!(ids(&db, "SELECT id FROM kv WHERE c LIKE 'ROW 3'"), vec![3]);
        assert_eq!(ids(&db, "SELECT id FROM kv WHERE c LIKE '%w 1' OR c LIKE 'r_w 2'"), vec![1, 2]);
        assert_eq!(ids(&db, "SELECT id FROM kv WHERE c NOT LIKE 'row%'"), NONE);
        assert_eq!(ids(&db, "SELECT id FROM kv WHERE a LIKE '5%'"), vec![5]);
        assert_eq!(ids(&db, "SELECT id FROM kv WHERE a IN (10, 30, 999)"), vec![1, 3]);
        assert_eq!(ids(&db, "SELECT id FROM kv WHERE id NOT IN (0, 1, 2, 3, 4, 5, 6)"), vec![7, 8, 9]);
        // A null in the list leaves NOT IN unknown for every row.
        assert_eq!(ids(&db, "SELECT id FROM kv WHERE a IN (10, NULL)"), vec![1]);
        assert_eq!(ids(&db, "SELECT id FROM kv WHERE a NOT IN (10, NULL)"), NONE);
        assert_eq!(ids(&db, "SELECT id FROM kv WHERE a IN ()"), NONE);
        assert_eq!(ids(&db, "SELECT id FROM kv WHERE a NOT IN () AND id < 2"), vec![0, 1]);
        let s = db.prepare("SELECT id FROM kv WHERE c LIKE ?1 AND id IN (?2, ?3)").unwrap();
        assert_eq!(s.query(&db, &[text("row%"), int(4), int(6)]).unwrap().rows(), &[vec![int(4)], vec![int(6)]]);
    }

    #[test]
    fn case_and_the_text_functions_work_things_out() {
        let db = kv();
        assert_eq!(
            rows(
                &db,
                "SELECT CASE WHEN a < 20 THEN 'low' WHEN a < 40 THEN 'mid' END, substr(c, 5), instr(c, 'w') \
                 FROM kv WHERE id IN (1, 3, 5)"
            ),
            vec![
                vec![text("low"), text("1"), int(3)],
                vec![text("mid"), text("3"), int(3)],
                vec![Value::Null, text("5"), int(3)],
            ]
        );
        assert_eq!(
            rows(
                &db,
                "SELECT SUM(CASE WHEN a >= 50 THEN 1 ELSE 0 END), \
                        SUM(CASE id WHEN 2 THEN 5 WHEN 3 THEN 7 ELSE 0 END) FROM kv"
            ),
            vec![vec![int(5), int(12)]]
        );
        assert_eq!(
            rows(&db, r#"SELECT json_extract('{"a":{"b":[1,"two"]}}', '$.a.b[1]') FROM kv WHERE id = 0"#),
            vec![vec![text("two")]]
        );
        assert!(db.query("SELECT json_extract(c, '$.a') FROM kv", &[]).is_err());
    }

    #[test]
    fn group_by_gives_one_row_for_each_group() {
        let mut db = kv();
        db.execute("UPDATE kv SET c = CASE WHEN id < 3 THEN 'x' WHEN id < 7 THEN 'y' ELSE 'z' END", &[])
            .unwrap();
        assert_eq!(
            rows(&db, "SELECT c, COUNT(*) AS n, SUM(a) FROM kv GROUP BY c"),
            vec![
                vec![text("x"), int(3), int(30)],
                vec![text("y"), int(4), int(180)],
                vec![text("z"), int(3), int(240)],
            ]
        );
        // The ORDER BY can use the name an item was given, its number,
        // or an aggregate of its own.
        assert_eq!(
            rows(&db, "SELECT c, COUNT(*) AS n FROM kv GROUP BY c ORDER BY n DESC, c DESC"),
            vec![vec![text("y"), int(4)], vec![text("z"), int(3)], vec![text("x"), int(3)]]
        );
        assert_eq!(
            rows(&db, "SELECT c, MAX(a) FROM kv GROUP BY 1 ORDER BY 2 DESC LIMIT 2"),
            vec![vec![text("z"), int(90)], vec![text("y"), int(60)]]
        );
        assert_eq!(
            rows(&db, "SELECT c FROM kv GROUP BY c ORDER BY SUM(a) DESC LIMIT 1 OFFSET 1"),
            vec![vec![text("y")]]
        );
        // A plain column comes from the first row of its group.
        assert_eq!(
            rows(&db, "SELECT id, c FROM kv GROUP BY c"),
            vec![vec![int(0), text("x")], vec![int(3), text("y")], vec![int(7), text("z")]]
        );
        // The WHERE is asked before the grouping, and a group can be
        // something worked out.
        assert_eq!(
            rows(&db, "SELECT id / 5, COUNT(*) FROM kv WHERE id > 1 GROUP BY id / 5"),
            vec![vec![int(0), int(3)], vec![int(1), int(5)]]
        );
        assert_eq!(rows(&db, "SELECT c, COUNT(*) FROM kv WHERE id > 99 GROUP BY c"), Vec::<Row>::new());
        assert!(db.prepare("SELECT c FROM kv GROUP BY COUNT(*)").is_err());
        assert!(db.prepare("SELECT c FROM kv GROUP BY 2").is_err());
        assert!(db.prepare("SELECT c FROM kv ORDER BY COUNT(*)").is_err());
    }

    #[test]
    fn distinct_limit_and_offset_cut_the_rows() {
        let mut db = kv();
        db.execute("UPDATE kv SET a = id / 4", &[]).unwrap();
        assert_eq!(ids(&db, "SELECT DISTINCT a FROM kv"), vec![0, 1, 2]);
        assert_eq!(ids(&db, "SELECT DISTINCT a FROM kv ORDER BY a DESC LIMIT 2"), vec![2, 1]);
        assert_eq!(ids(&db, "SELECT DISTINCT a FROM kv LIMIT 1 OFFSET 1"), vec![1]);
        assert_eq!(rows(&db, "SELECT DISTINCT a, a + 1 FROM kv WHERE id < 5").len(), 2);
        assert_eq!(ids(&db, "SELECT id FROM kv LIMIT 3 OFFSET 2"), vec![2, 3, 4]);
        assert_eq!(ids(&db, "SELECT id FROM kv ORDER BY id DESC LIMIT 3 OFFSET 8"), vec![1, 0]);
        assert_eq!(ids(&db, "SELECT id FROM kv LIMIT -1 OFFSET 8"), vec![8, 9]);
        assert_eq!(ids(&db, "SELECT id FROM kv LIMIT 2 OFFSET 99"), NONE);
        assert_eq!(ids(&db, "SELECT id FROM kv LIMIT 0"), NONE);
        let s = db.prepare("SELECT id FROM kv ORDER BY id LIMIT ?1 OFFSET ?2").unwrap();
        assert_eq!(s.query(&db, &[int(2), int(5)]).unwrap().rows(), &[vec![int(5)], vec![int(6)]]);
        assert!(s.query(&db, &[text("two"), int(0)]).is_err());
        assert!(db.prepare("SELECT id FROM kv LIMIT a").is_err());
        // An aggregate gives one row, which a LIMIT or an OFFSET can hide.
        assert_eq!(rows(&db, "SELECT COUNT(*) FROM kv LIMIT 5"), vec![vec![int(10)]]);
        assert_eq!(rows(&db, "SELECT COUNT(*) FROM kv LIMIT 1 OFFSET 1"), Vec::<Row>::new());
        assert_eq!(rows(&db, "SELECT SUM(a) FROM kv WHERE id > 7 LIMIT 0"), Vec::<Row>::new());
        // The keys an index holds are cut the same way.
        db.execute("CREATE INDEX kv_a ON kv (a)", &[]).unwrap();
        assert_eq!(ids(&db, "SELECT id FROM kv WHERE a = 0 LIMIT 2 OFFSET 1"), vec![1, 2]);
        assert_eq!(ids(&db, "SELECT id FROM kv WHERE a = 0 ORDER BY id DESC LIMIT 2 OFFSET 1"), vec![2, 1]);
        assert_eq!(ids(&db, "SELECT DISTINCT a FROM kv WHERE a = 1"), vec![1]);
    }

    #[test]
    fn a_query_inside_another_is_answered_first() {
        let mut db = kv();
        assert_eq!(ids(&db, "SELECT id FROM kv WHERE id > (SELECT MAX(id) FROM kv) - 3"), vec![7, 8, 9]);
        assert_eq!(ids(&db, "SELECT id FROM kv WHERE a IN (SELECT a FROM kv WHERE id < 2)"), vec![0, 1]);
        assert_eq!(ids(&db, "SELECT id FROM kv WHERE a NOT IN (SELECT a FROM kv WHERE id > 1)"), vec![0, 1]);
        assert_eq!(
            ids(&db, "SELECT id FROM kv WHERE id IN (SELECT id FROM kv WHERE a > (SELECT MIN(a) FROM kv) + 70)"),
            vec![8, 9]
        );
        // A query that finds no row stands for null.
        assert_eq!(ids(&db, "SELECT id FROM kv WHERE a = (SELECT a FROM kv WHERE id = 99)"), NONE);
        assert_eq!(rows(&db, "SELECT (SELECT COUNT(*) FROM kv) FROM kv WHERE id = 0"), vec![vec![int(10)]]);

        // Prepared once, it is answered again each time it runs.
        let s = db.prepare("SELECT id FROM kv WHERE id = (SELECT MAX(id) FROM kv WHERE a < ?1)").unwrap();
        assert!(!s.is_point_lookup());
        assert_eq!(s.kind(), Kind2::Reads);
        assert_eq!(s.query(&db, &[int(35)]).unwrap().rows(), &[vec![int(3)]]);
        db.execute("DELETE FROM kv WHERE id = 3", &[]).unwrap();
        assert_eq!(s.query(&db, &[int(35)]).unwrap().rows(), &[vec![int(2)]]);

        // A change can lean on one too, also a change to the table asked.
        let out = db.execute("DELETE FROM kv WHERE id IN (SELECT id FROM kv WHERE a >= 70)", &[]).unwrap();
        assert_eq!(out.changed(), 3);
        db.execute("UPDATE kv SET a = (SELECT MAX(a) FROM kv) + 1 WHERE id = 0", &[]).unwrap();
        db.execute("INSERT INTO kv (id, a) VALUES ((SELECT MAX(id) FROM kv) + 1, 5)", &[]).unwrap();
        assert_eq!(rows(&db, "SELECT id, a FROM kv WHERE id IN (0, 7)"), vec![vec![int(0), int(61)], vec![int(7), int(5)]]);

        // What can be checked at prepare time is checked then.
        assert!(db.prepare("SELECT id FROM kv WHERE a IN (SELECT nope FROM kv)").is_err());
        assert!(db.prepare("SELECT id FROM kv WHERE a IN (SELECT id, a FROM kv)").is_err());
        assert!(db.prepare("SELECT id FROM kv WHERE nope IN (SELECT id FROM kv)").is_err());
        assert!(db.prepare("SELECT id FROM kv WHERE a = (SELECT * FROM kv)").is_err());
    }

    #[test]
    fn a_sum_of_numbers_still_finds_rows_by_key() {
        let db = kv();
        assert_eq!(ids(&db, "SELECT id FROM kv WHERE id > 10 - 3"), vec![8, 9]);
        assert_eq!(ids(&db, "SELECT id FROM kv WHERE id BETWEEN 1 + 1 AND 2 * 2"), vec![2, 3, 4]);
        // Worked out at prepare time, it is a lookup by key like any other.
        assert!(db.prepare("SELECT a FROM kv WHERE id = 2 * 3").unwrap().is_point_lookup());
        // Only a whole number is taken that way. The rest is asked of each row.
        assert!(!db.prepare("SELECT a FROM kv WHERE id = 7 / 2.0").unwrap().is_point_lookup());
        assert_eq!(ids(&db, "SELECT id FROM kv WHERE id > 15 / 2.0"), vec![8, 9]);
    }

    #[test]
    fn alter_table_adds_a_column_to_every_row() {
        let mut db = kv();
        db.execute("ALTER TABLE kv ADD COLUMN n INTEGER DEFAULT 7", &[]).unwrap();
        db.execute("ALTER TABLE kv ADD COLUMN note TEXT", &[]).unwrap();
        assert_eq!(rows(&db, "SELECT id, n, note FROM kv WHERE id = 2"), vec![vec![int(2), int(7), Value::Null]]);
        db.execute("INSERT INTO kv (id, a) VALUES (50, 1)", &[]).unwrap();
        db.execute("UPDATE kv SET note = 'hi' WHERE id = 50", &[]).unwrap();
        assert_eq!(rows(&db, "SELECT n, note FROM kv WHERE id = 50"), vec![vec![int(7), text("hi")]]);
        assert_eq!(rows(&db, "SELECT * FROM kv WHERE id = 2")[0].len(), 6);
        db.execute("CREATE INDEX kv_n ON kv (n)", &[]).unwrap();
        assert_eq!(rows(&db, "SELECT COUNT(*) FROM kv WHERE n = 7"), vec![vec![int(11)]]);

        // A name already there, a NOT NULL with nothing to put in the old
        // rows, a default of the wrong kind, a table that is not there.
        assert!(db.execute("ALTER TABLE kv ADD COLUMN n TEXT", &[]).is_err());
        assert!(db.execute("ALTER TABLE kv ADD COLUMN id TEXT", &[]).is_err());
        assert!(db.execute("ALTER TABLE kv ADD COLUMN m INTEGER NOT NULL", &[]).is_err());
        assert!(db.execute("ALTER TABLE kv ADD COLUMN m INTEGER DEFAULT 'x'", &[]).is_err());
        assert!(db.execute("ALTER TABLE nope ADD COLUMN m INTEGER", &[]).is_err());
        // And not inside a transaction, where a rollback could not take
        // the column out of the rows again.
        db.execute("BEGIN", &[]).unwrap();
        assert!(db.execute("ALTER TABLE kv ADD COLUMN m INTEGER", &[]).is_err());
        db.execute("ROLLBACK", &[]).unwrap();
        db.execute("ALTER TABLE kv ADD COLUMN m INTEGER NOT NULL DEFAULT 0", &[]).unwrap();
        assert_eq!(rows(&db, "SELECT m FROM kv WHERE id = 50"), vec![vec![int(0)]]);
    }
}
