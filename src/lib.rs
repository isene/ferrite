//! ferrite: an embedded SQL database that keeps its tables in memory.
//!
//! A table holds its rows in a B-tree map under an integer primary key,
//! and the API is plain Rust calls. SQL compiles down to exactly those
//! calls, so whatever they cost is the floor for everything above.
//!
//! [`Db::new`] keeps everything in memory and forgets it when the
//! program ends. [`Db::open`] keeps it in a directory, and **defaults to
//! NORMAL durability**: a commit is written at once, so a crashed
//! program loses nothing, but a power cut can lose the last few commits.
//! [`Durability::Full`] fsyncs every commit and costs about ten times as
//! much on writes.
//!
//! The one design decision that shapes the rest: a caller holds a
//! [`TableId`] and reaches its table in one array index. Looking a table
//! up by name costs a hash of the name, and doing that on every row
//! would show up in the numbers. SQLite avoids the same cost by letting
//! you prepare a statement once, so this is the fair comparison.
//!
//! ```
//! use ferrite::{Db, Kind, Value};
//!
//! let mut db = Db::new();
//! let kv = db.create_table("kv", &[("a", Kind::Int), ("c", Kind::Text)]).unwrap();
//! db.insert(kv, 1, vec![Value::Int(7), Value::Text("hello".into())]).unwrap();
//! assert_eq!(db.table(kv).get(1).unwrap()[0], Value::Int(7));
//! ```

pub mod log;
pub mod plan;
pub mod sql;

pub use log::Durability;
pub use plan::{Outcome, Statement};

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use log::{Change, Store};

// ── Values ─────────────────────────────────────────────────────────────

/// What a value in a column can be. SQLite's five, and no more.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

/// What a column is allowed to hold. Null is allowed in any column that
/// says so, so it is not a kind of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Int,
    Real,
    Text,
    Blob,
}

impl Kind {
    pub fn name(&self) -> &'static str {
        match self {
            Kind::Int => "integer",
            Kind::Real => "real",
            Kind::Text => "text",
            Kind::Blob => "blob",
        }
    }
}

impl Value {
    /// The name SQL uses for this kind of value.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Int(_) => "integer",
            Value::Real(_) => "real",
            Value::Text(_) => "text",
            Value::Blob(_) => "blob",
        }
    }

    /// True when this value may sit in a column of that kind.
    fn fits(&self, kind: Kind) -> bool {
        matches!(
            (self, kind),
            (Value::Null, _)
                | (Value::Int(_), Kind::Int)
                | (Value::Real(_), Kind::Real)
                | (Value::Text(_), Kind::Text)
                | (Value::Blob(_), Kind::Blob)
        )
    }

    pub fn as_int(&self) -> Option<i64> {
        match self { Value::Int(i) => Some(*i), _ => None }
    }
    pub fn as_real(&self) -> Option<f64> {
        match self { Value::Real(r) => Some(*r), _ => None }
    }
    pub fn as_text(&self) -> Option<&str> {
        match self { Value::Text(t) => Some(t), _ => None }
    }
}

/// A value wrapped so that it can be sorted and used as a key.
///
/// [`Value`] on its own has no total order: a real can be NaN, and null
/// compares to nothing at all. That is right inside a WHERE, where
/// anything against null is false. It is wrong for ORDER BY and for an
/// index, which have to put every value somewhere. Here null comes
/// first, then numbers, then text, then blobs, which is the order SQLite
/// uses.
#[derive(Debug, Clone, PartialEq)]
pub struct SortKey(pub Value);

impl Eq for SortKey {}

impl SortKey {
    fn rank(&self) -> u8 {
        match self.0 {
            Value::Null => 0,
            Value::Int(_) | Value::Real(_) => 1,
            Value::Text(_) => 2,
            Value::Blob(_) => 3,
        }
    }
}

impl Ord for SortKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        match (&self.0, &other.0) {
            (Value::Int(a), Value::Int(b)) => a.cmp(b),
            (Value::Real(a), Value::Real(b)) => a.total_cmp(b),
            (Value::Int(a), Value::Real(b)) => (*a as f64).total_cmp(b),
            (Value::Real(a), Value::Int(b)) => a.total_cmp(&(*b as f64)),
            (Value::Text(a), Value::Text(b)) => a.cmp(b),
            (Value::Blob(a), Value::Blob(b)) => a.cmp(b),
            _ => match self.rank().cmp(&other.rank()) {
                Ordering::Equal => Ordering::Equal,
                other => other,
            },
        }
    }
}

impl PartialOrd for SortKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> { Some(self.cmp(other)) }
}

// ── What can go wrong ──────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// A table by that name is already here.
    TableExists(String),
    /// No table by that name.
    NoTable(String),
    /// A row with that key is already here.
    KeyExists(i64),
    /// No row with that key.
    NoKey(i64),
    /// The row had the wrong number of values.
    WrongWidth { want: usize, got: usize },
    /// A value of the wrong kind for its column.
    WrongKind { column: String, want: &'static str, got: &'static str },
    /// A column number past the end of the row.
    NoColumn(usize),
    /// A null in a column that does not allow one.
    NotNull(String),
    /// The SQL could not be read, or asks for something that is not there.
    Sql(String),
    /// Something went wrong with the files on disk.
    Disk(String),
    /// An index by that name is already here.
    IndexExists(String),
    /// No index by that name.
    NoIndex(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::TableExists(n) => write!(f, "there is already a table called {n}"),
            Error::NoTable(n) => write!(f, "there is no table called {n}"),
            Error::KeyExists(k) => write!(f, "there is already a row with key {k}"),
            Error::NoKey(k) => write!(f, "there is no row with key {k}"),
            Error::WrongWidth { want, got } => {
                write!(f, "the table has {want} columns and the row has {got}")
            }
            Error::WrongKind { column, want, got } => {
                write!(f, "column {column} holds {want}, and this is {got}")
            }
            Error::NoColumn(i) => write!(f, "there is no column {i}"),
            Error::NotNull(c) => write!(f, "column {c} cannot be empty"),
            Error::Sql(s) => write!(f, "{s}"),
            Error::Disk(s) => write!(f, "the database files: {s}"),
            Error::IndexExists(n) => write!(f, "there is already an index called {n}"),
            Error::NoIndex(n) => write!(f, "there is no index called {n}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

// ── Tables ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Column {
    pub name: String,
    pub kind: Kind,
    pub null_ok: bool,
}

/// A row is its values in column order. The primary key is not among
/// them: it is the key the row is filed under.
pub type Row = Vec<Value>;

/// One table, with its rows under an integer primary key.
///
/// The rows live in a `BTreeMap`, which keeps them in key order. That
/// costs a little against a hash map on a single lookup and pays for
/// itself the moment anything asks for a range, an ORDER BY or the next
/// key. Phase 5 is where that trade gets measured rather than assumed.
#[derive(Debug, Clone)]
pub struct Table {
    name: String,
    /// True once the table has been thrown away. The slot stays, because
    /// the log names tables by their number.
    dropped: bool,
    /// What the primary key is called in SQL. The key is not stored in
    /// the row; it is what the row is filed under.
    key: String,
    columns: Vec<Column>,
    rows: BTreeMap<i64, Row>,
    /// Which indexes have to be kept up as rows change. Empty is the
    /// usual case, and one test of that keeps the cost off the hot path.
    watchers: Vec<usize>,
}

impl Table {
    pub fn name(&self) -> &str { &self.name }
    pub fn key_name(&self) -> &str { &self.key }
    pub fn columns(&self) -> &[Column] { &self.columns }
    pub fn len(&self) -> usize { self.rows.len() }
    pub fn is_empty(&self) -> bool { self.rows.is_empty() }

    /// Which column that name is, if any.
    pub fn column_of(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }

    /// The row under that key.
    #[inline]
    pub fn get(&self, key: i64) -> Option<&Row> { self.rows.get(&key) }

    /// One value out of the row under that key.
    #[inline]
    pub fn get_at(&self, key: i64, column: usize) -> Option<&Value> {
        self.rows.get(&key)?.get(column)
    }

    /// Put a new row in. It is an error if the key is taken, so nothing
    /// is overwritten by accident.
    pub fn insert(&mut self, key: i64, row: Row) -> Result<()> {
        self.check(&row)?;
        if self.rows.contains_key(&key) {
            return Err(Error::KeyExists(key));
        }
        self.rows.insert(key, row);
        Ok(())
    }

    /// Put a row in, over whatever was there. Gives back the old row.
    pub fn put(&mut self, key: i64, row: Row) -> Result<Option<Row>> {
        self.check(&row)?;
        Ok(self.rows.insert(key, row))
    }

    /// Change one value in an existing row.
    pub fn update(&mut self, key: i64, column: usize, value: Value) -> Result<()> {
        let col = self.columns.get(column).ok_or(Error::NoColumn(column))?;
        if !value.fits(col.kind) {
            return Err(Error::WrongKind {
                column: col.name.clone(),
                want: col.kind.name(),
                got: value.type_name(),
            });
        }
        if value == Value::Null && !col.null_ok {
            return Err(Error::NotNull(col.name.clone()));
        }
        let row = self.rows.get_mut(&key).ok_or(Error::NoKey(key))?;
        row[column] = value;
        Ok(())
    }

    /// Take a row out. True when there was one.
    pub fn delete(&mut self, key: i64) -> bool { self.rows.remove(&key).is_some() }

    /// Every row in key order.
    pub fn iter(&self) -> impl Iterator<Item = (i64, &Row)> {
        self.rows.iter().map(|(k, r)| (*k, r))
    }

    /// The rows whose keys fall in a range, in order. A range that ends
    /// before it starts holds nothing, rather than being a mistake:
    /// `WHERE id > 6 AND id < 3` is a fair question with no answer.
    pub fn range(&self, from: i64, to: i64) -> impl Iterator<Item = (i64, &Row)> {
        self.rows.range(from..to.max(from)).map(|(k, r)| (*k, r))
    }

    /// Put a row in that has already been looked over.
    pub(crate) fn insert_known_good(&mut self, key: i64, row: Row) {
        self.rows.insert(key, row);
    }

    /// Would this row go in under this key?
    pub(crate) fn can_insert(&self, key: i64, row: &Row) -> Result<()> {
        self.check(row)?;
        if self.rows.contains_key(&key) { return Err(Error::KeyExists(key)); }
        Ok(())
    }

    /// Does this row fit the columns?
    pub(crate) fn fits_row(&self, row: &Row) -> Result<()> { self.check(row) }

    /// Would this change to one value work?
    pub(crate) fn can_update(&self, key: i64, column: usize, value: &Value) -> Result<()> {
        let col = self.columns.get(column).ok_or(Error::NoColumn(column))?;
        if *value == Value::Null {
            if !col.null_ok { return Err(Error::NotNull(col.name.clone())); }
        } else if !value.fits(col.kind) {
            return Err(Error::WrongKind {
                column: col.name.clone(),
                want: col.kind.name(),
                got: value.type_name(),
            });
        }
        if !self.rows.contains_key(&key) { return Err(Error::NoKey(key)); }
        Ok(())
    }

    fn check(&self, row: &Row) -> Result<()> {
        if row.len() != self.columns.len() {
            return Err(Error::WrongWidth { want: self.columns.len(), got: row.len() });
        }
        for (value, col) in row.iter().zip(&self.columns) {
            if *value == Value::Null {
                if !col.null_ok {
                    return Err(Error::NotNull(col.name.clone()));
                }
            } else if !value.fits(col.kind) {
                return Err(Error::WrongKind {
                    column: col.name.clone(),
                    want: col.kind.name(),
                    got: value.type_name(),
                });
            }
        }
        Ok(())
    }
}

/// Which index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexId(usize);

/// One column of one table, with the keys of every row that holds each
/// value. Several rows can share a value, so each entry holds a set.
#[derive(Debug, Clone)]
pub struct Index {
    name: String,
    table: TableId,
    column: usize,
    entries: BTreeMap<SortKey, BTreeSet<i64>>,
    dropped: bool,
}

impl Index {
    pub fn name(&self) -> &str { &self.name }
    pub fn table(&self) -> TableId { self.table }
    pub fn column(&self) -> usize { self.column }

    /// The keys of the rows whose value in this column is `value`.
    pub fn keys_for(&self, value: &Value) -> Option<&BTreeSet<i64>> {
        self.entries.get(&SortKey(value.clone()))
    }

    /// Every value in order, with the rows holding it.
    pub fn iter(&self) -> impl Iterator<Item = (&Value, &BTreeSet<i64>)> {
        self.entries.iter().map(|(k, v)| (&k.0, v))
    }

    fn add(&mut self, value: &Value, key: i64) {
        self.entries.entry(SortKey(value.clone())).or_default().insert(key);
    }

    fn remove(&mut self, value: &Value, key: i64) {
        let k = SortKey(value.clone());
        if let Some(set) = self.entries.get_mut(&k) {
            set.remove(&key);
            if set.is_empty() { self.entries.remove(&k); }
        }
    }
}

// ── The database ───────────────────────────────────────────────────────

/// Which table. Handing one of these back and taking it again means a
/// caller reaches its table by array index rather than by hashing a
/// name on every row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableId(usize);

/// Every table, in memory. Nothing here touches the disk: durability is
/// phase 3.
#[derive(Debug, Default)]
pub struct Db {
    tables: Vec<Table>,
    by_name: HashMap<String, usize>,
    /// True between BEGIN and COMMIT.
    in_txn: bool,
    /// What to put back if the transaction is rolled back, newest last.
    /// Nothing is recorded outside a transaction, so the usual path
    /// costs one boolean test.
    undo: Vec<Undo>,
    /// The files, when this database has any. Without them it is a
    /// database in memory that forgets everything when the program ends.
    store: Option<Store>,
    indexes: Vec<Index>,
    index_names: HashMap<String, usize>,
    /// The commit being built, as the bytes that will go to the log.
    /// Keeping bytes rather than a list of changes means a row never has
    /// to be copied on its way to the disk.
    journal: Vec<u8>,
    journal_count: u32,
}

/// One step backwards.
#[derive(Debug)]
enum Undo {
    /// The row was not there before: take it out again.
    Added(TableId, i64),
    /// The row was there before: put it back as it was.
    Was(TableId, i64, Option<Row>),
}

impl Db {
    pub fn new() -> Db { Db::default() }

    /// Make a table. Every column takes a kind and allows null; use
    /// [`Db::create_table_full`] to forbid null.
    pub fn create_table(&mut self, name: &str, columns: &[(&str, Kind)]) -> Result<TableId> {
        let columns: Vec<Column> = columns
            .iter()
            .map(|(n, k)| Column { name: (*n).to_string(), kind: *k, null_ok: true })
            .collect();
        self.create_table_full(name, "id", columns)
    }

    pub fn create_table_full(&mut self, name: &str, key: &str, columns: Vec<Column>) -> Result<TableId> {
        if self.by_name.contains_key(name) {
            return Err(Error::TableExists(name.to_string()));
        }
        let id = TableId(self.tables.len());
        self.tables.push(Table {
            name: name.to_string(),
            dropped: false,
            key: key.to_string(),
            columns,
            rows: BTreeMap::new(),
            watchers: Vec::new(),
        });
        self.by_name.insert(name.to_string(), id.0);
        let columns = self.tables[id.0].columns.clone();
        self.record(Change::NewTable {
            name: name.to_string(),
            key: key.to_string(),
            columns,
        })?;
        Ok(id)
    }

    /// Find a table by name. Do this once and keep the id.
    pub fn table_id(&self, name: &str) -> Result<TableId> {
        self.by_name.get(name).map(|i| TableId(*i)).ok_or_else(|| Error::NoTable(name.to_string()))
    }

    #[inline]
    pub fn table(&self, id: TableId) -> &Table { &self.tables[id.0] }

    /// A table to change directly. This does not reach the log, so it
    /// is only for the inside of the crate, where the caller has already
    /// arranged for the change to be recorded.
    #[inline]
    fn table_mut(&mut self, id: TableId) -> &mut Table { &mut self.tables[id.0] }

    pub fn table_names(&self) -> impl Iterator<Item = &str> {
        self.tables.iter().map(|t| t.name.as_str())
    }

    // ── SQL ────────────────────────────────────────────────────────────

    /// Read and plan a statement, ready to run many times.
    ///
    /// The plan is settled here, including which index to use. A
    /// statement prepared before an index was made still answers
    /// correctly, and still walks the table. Prepare it again to pick
    /// the index up.
    pub fn prepare(&self, sql: &str) -> Result<Statement> { plan::plan(self, sql) }

    /// Read, plan and run a statement that only reads. It takes the
    /// database without asking to change it, so a reader needs no
    /// mutable borrow.
    pub fn query(&self, sql: &str, params: &[Value]) -> Result<Outcome> {
        plan::plan(self, sql)?.query(self, params)
    }

    /// Read, plan and run a statement once. Fine for a one-off; for
    /// anything in a loop, prepare it and keep it.
    pub fn execute(&mut self, sql: &str, params: &[Value]) -> Result<Outcome> {
        let stmt = plan::plan(self, sql)?;
        if matches!(stmt.kind(), plan::Kind2::Reads) {
            stmt.query(self, params)
        } else {
            stmt.run(self, params)
        }
    }

    // ── Transactions ───────────────────────────────────────────────────

    pub fn begin(&mut self) {
        self.in_txn = true;
        self.undo.clear();
    }

    /// Finish a transaction. Everything it did goes to the log as one
    /// record, which is why a batch of writes costs one fsync and not
    /// one each.
    pub fn commit(&mut self) -> Result<()> {
        self.in_txn = false;
        self.undo.clear();
        self.write_journal()
    }

    /// Put everything back the way it was at BEGIN.
    pub fn rollback(&mut self) {
        self.in_txn = false;
        self.journal.clear();
        self.journal_count = 0;
        while let Some(step) = self.undo.pop() {
            match step {
                Undo::Added(id, key) => { self.tables[id.0].rows.remove(&key); }
                Undo::Was(id, key, Some(row)) => { self.tables[id.0].rows.insert(key, row); }
                Undo::Was(id, key, None) => { self.tables[id.0].rows.remove(&key); }
            }
        }
    }

    pub fn in_transaction(&self) -> bool { self.in_txn }

    /// Remember that a row has just been added.
    #[inline]
    pub(crate) fn note_insert(&mut self, table: TableId, key: i64) {
        if self.in_txn { self.undo.push(Undo::Added(table, key)); }
    }

    /// Remember what a row looked like before it is changed or removed.
    #[inline]
    pub(crate) fn note_change(&mut self, table: TableId, key: i64) {
        if self.in_txn {
            let was = self.tables[table.0].rows.get(&key).cloned();
            self.undo.push(Undo::Was(table, key, was));
        }
    }

    /// Throw a table away, with everything in it.
    pub fn drop_table(&mut self, name: &str) -> Result<()> {
        let i = *self.by_name.get(name).ok_or_else(|| Error::NoTable(name.to_string()))?;
        self.tables[i].rows.clear();
        self.tables[i].dropped = true;
        self.by_name.remove(name);
        self.record(Change::DropTable { name: name.to_string() })
    }

    // ── Indexes ────────────────────────────────────────────────────────

    /// Make an index on one column, and fill it from the rows already
    /// there.
    pub fn create_index(&mut self, name: &str, table: TableId, column: usize) -> Result<IndexId> {
        if self.index_names.contains_key(name) {
            return Err(Error::IndexExists(name.to_string()));
        }
        if column >= self.tables[table.0].columns.len() {
            return Err(Error::NoColumn(column));
        }
        let slot = self.indexes.len();
        self.indexes.push(Index {
            name: name.to_string(),
            table,
            column,
            entries: BTreeMap::new(),
            dropped: false,
        });
        self.index_names.insert(name.to_string(), slot);
        self.tables[table.0].watchers.push(slot);
        self.fill_index(slot);
        let column = self.tables[table.0].columns[column].name.clone();
        self.record(Change::NewIndex { name: name.to_string(), table: table.0 as u32, column })?;
        Ok(IndexId(slot))
    }

    pub fn index_id(&self, name: &str) -> Result<IndexId> {
        self.index_names.get(name).map(|i| IndexId(*i)).ok_or_else(|| Error::NoIndex(name.to_string()))
    }

    pub fn index(&self, id: IndexId) -> &Index { &self.indexes[id.0] }

    /// An index on this column of this table, if there is one.
    pub fn index_on(&self, table: TableId, column: usize) -> Option<IndexId> {
        self.tables[table.0]
            .watchers
            .iter()
            .find(|&&s| !self.indexes[s].dropped && self.indexes[s].column == column)
            .map(|&s| IndexId(s))
    }

    pub fn drop_index(&mut self, name: &str) -> Result<()> {
        let slot = *self.index_names.get(name).ok_or_else(|| Error::NoIndex(name.to_string()))?;
        self.indexes[slot].entries.clear();
        self.indexes[slot].dropped = true;
        let table = self.indexes[slot].table;
        self.tables[table.0].watchers.retain(|&s| s != slot);
        self.index_names.remove(name);
        self.record(Change::DropIndex { name: name.to_string() })
    }

    fn fill_index(&mut self, slot: usize) {
        let table = self.indexes[slot].table;
        let column = self.indexes[slot].column;
        let pairs: Vec<(i64, Value)> = self.tables[table.0]
            .rows
            .iter()
            .filter_map(|(k, row)| row.get(column).map(|v| (*k, v.clone())))
            .collect();
        for (key, value) in pairs {
            self.indexes[slot].add(&value, key);
        }
    }

    /// Put a row into every index watching its table.
    fn index_add(&mut self, table: TableId, key: i64, row: &Row) {
        if self.tables[table.0].watchers.is_empty() { return; }
        for slot in self.tables[table.0].watchers.clone() {
            if let Some(v) = row.get(self.indexes[slot].column) {
                let v = v.clone();
                self.indexes[slot].add(&v, key);
            }
        }
    }

    /// Take a row out of every index watching its table.
    fn index_take(&mut self, table: TableId, key: i64, row: &Row) {
        if self.tables[table.0].watchers.is_empty() { return; }
        for slot in self.tables[table.0].watchers.clone() {
            if let Some(v) = row.get(self.indexes[slot].column) {
                let v = v.clone();
                self.indexes[slot].remove(&v, key);
            }
        }
    }

    #[inline]
    fn watched(&self, table: TableId) -> bool { !self.tables[table.0].watchers.is_empty() }

    // ── Changing rows ──────────────────────────────────────────────────
    //
    // Every change goes through here, so that the log hears about all of
    // them. When there is no file and no transaction, none of this costs
    // more than one test of a boolean.

    /// True when a change has to be written down, either for the log or
    /// so that a rollback can undo it.
    #[inline]
    fn recording(&self) -> bool { self.store.is_some() || self.in_txn }

    /// Make room for another change in the commit being built.
    #[inline]
    fn opening(&mut self) -> &mut Vec<u8> {
        if self.journal.is_empty() { log::begin(&mut self.journal); }
        self.journal_count += 1;
        &mut self.journal
    }

    /// Outside a transaction, one change is one commit, so close it off
    /// and write it now.
    #[inline]
    fn close_if_alone(&mut self) -> Result<()> {
        if self.in_txn { return Ok(()); }
        self.write_journal()
    }

    fn write_journal(&mut self) -> Result<()> {
        if self.journal_count == 0 {
            self.journal.clear();
            return Ok(());
        }
        log::finish(&mut self.journal, self.journal_count);
        if let Some(s) = &mut self.store {
            s.commit_bytes(&self.journal, self.journal_count)?;
        }
        self.journal.clear();
        self.journal_count = 0;
        self.snapshot_if_grown()
    }

    /// Put a new row in. It is an error if the key is taken.
    pub fn insert(&mut self, table: TableId, key: i64, row: Row) -> Result<()> {
        if self.recording() {
            // Check before writing anything down, so a row the table
            // would refuse never reaches the log.
            self.tables[table.0].can_insert(key, &row)?;
            let t = table.0 as u32;
            log::put_row(self.opening(), t, key, &row);
            // Already checked a line ago; checking again is pure cost.
            self.tables[table.0].insert_known_good(key, row);
        } else {
            self.tables[table.0].insert(key, row)?;
        }
        self.note_insert(table, key);
        if self.watched(table) {
            let row = self.tables[table.0].rows[&key].clone();
            self.index_add(table, key, &row);
        }
        self.close_if_alone()
    }

    /// Put a row in over whatever was there. Gives back the old row.
    pub fn put(&mut self, table: TableId, key: i64, row: Row) -> Result<Option<Row>> {
        if self.recording() {
            self.tables[table.0].fits_row(&row)?;
            let t = table.0 as u32;
            log::put_row(self.opening(), t, key, &row);
        }
        self.note_change(table, key);
        let watched = self.watched(table);
        if watched {
            if let Some(old) = self.tables[table.0].get(key).cloned() {
                self.index_take(table, key, &old);
            }
        }
        let old = self.tables[table.0].put(key, row)?;
        if watched {
            let now = self.tables[table.0].rows[&key].clone();
            self.index_add(table, key, &now);
        }
        self.close_if_alone()?;
        Ok(old)
    }

    /// Change one value in an existing row.
    pub fn update(&mut self, table: TableId, key: i64, column: usize, value: Value) -> Result<()> {
        if self.recording() {
            self.tables[table.0].can_update(key, column, &value)?;
            let t = table.0 as u32;
            log::put_set(self.opening(), t, key, column as u32, &value);
        }
        self.note_change(table, key);
        let watched = self.watched(table);
        let was = if watched { self.tables[table.0].get_at(key, column).cloned() } else { None };
        self.tables[table.0].update(key, column, value)?;
        if watched {
            let slots = self.tables[table.0].watchers.clone();
            let now = self.tables[table.0].get_at(key, column).cloned();
            for slot in slots {
                if self.indexes[slot].column != column { continue; }
                if let Some(v) = &was { self.indexes[slot].remove(v, key); }
                if let Some(v) = &now { self.indexes[slot].add(v, key); }
            }
        }
        self.close_if_alone()
    }

    /// Take a row out. True when there was one.
    pub fn delete(&mut self, table: TableId, key: i64) -> Result<bool> {
        if self.tables[table.0].get(key).is_none() { return Ok(false); }
        if self.recording() {
            let t = table.0 as u32;
            log::put_delete(self.opening(), t, key);
        }
        self.note_change(table, key);
        if self.watched(table) {
            if let Some(old) = self.tables[table.0].get(key).cloned() {
                self.index_take(table, key, &old);
            }
        }
        self.tables[table.0].delete(key);
        self.close_if_alone()?;
        Ok(true)
    }

    // ── The files ──────────────────────────────────────────────────────

    /// Open a database in a directory, making it if it is not there.
    ///
    /// **This is NORMAL durability.** A commit is written straight away,
    /// so a program that crashes loses nothing. A power cut or a kernel
    /// panic can lose the last few commits. Use [`Db::open_with`] with
    /// [`Durability::Full`] when that is not good enough.
    pub fn open(dir: impl AsRef<Path>) -> Result<Db> {
        Db::open_with(dir, Durability::default())
    }

    /// Open a database and say how hard a commit should try to survive.
    pub fn open_with(dir: impl AsRef<Path>, durability: Durability) -> Result<Db> {
        let (store, commits) = Store::open(dir.as_ref(), durability)?;
        let mut db = Db::new();
        for commit in commits {
            db.replay(commit)?;
        }
        // The indexes were only definitions on the way in. Now that
        // every row is back, fill them.
        for slot in 0..db.indexes.len() {
            if !db.indexes[slot].dropped { db.fill_index(slot); }
        }
        db.store = Some(store);
        Ok(db)
    }

    /// How hard a commit tries to survive.
    pub fn durability(&self) -> Option<Durability> {
        self.store.as_ref().map(|s| s.durability)
    }

    /// Change how hard a commit tries to survive, from here on.
    pub fn set_durability(&mut self, d: Durability) {
        if let Some(s) = &mut self.store { s.durability = d; }
    }

    /// Force everything committed so far onto the disk. In FULL this has
    /// already happened; in NORMAL this is how you make sure.
    pub fn flush(&mut self) -> Result<()> {
        match &mut self.store {
            Some(s) => s.flush(),
            None => Ok(()),
        }
    }

    /// Write the whole database out fresh and start the log again.
    pub fn checkpoint(&mut self) -> Result<()> {
        let all = self.everything();
        match &mut self.store {
            Some(s) => s.snapshot(&all),
            None => Ok(()),
        }
    }

    /// Write a change down that happens rarely enough to build the
    /// slow way: making or dropping a table.
    fn record(&mut self, change: Change) -> Result<()> {
        if !self.recording() { return Ok(()); }
        log::put_one(self.opening(), &change);
        self.close_if_alone()
    }

    fn snapshot_if_grown(&mut self) -> Result<()> {
        let live: u64 = self.tables.iter().map(|t| t.rows.len() as u64).sum();
        if self.store.as_ref().is_some_and(|s| s.wants_snapshot(live)) {
            let all = self.everything();
            if let Some(s) = &mut self.store { s.snapshot(&all)?; }
        }
        Ok(())
    }

    /// The whole database as a list of changes, for a snapshot. The
    /// tables come in the order they were made, because the log names
    /// them by number.
    fn everything(&self) -> Vec<Change> {
        let mut out = Vec::new();
        for (i, t) in self.tables.iter().enumerate() {
            out.push(Change::NewTable {
                name: t.name.clone(),
                key: t.key.clone(),
                columns: t.columns.clone(),
            });
            if t.dropped {
                out.push(Change::DropTable { name: t.name.clone() });
                continue;
            }
            for (key, row) in &t.rows {
                out.push(Change::Put { table: i as u32, key: *key, row: row.clone() });
            }
        }
        for index in &self.indexes {
            if index.dropped { continue; }
            out.push(Change::NewIndex {
                name: index.name.clone(),
                table: index.table.0 as u32,
                column: self.tables[index.table.0].columns[index.column].name.clone(),
            });
        }
        out
    }

    /// Put a commit from the log back into the tables. Nothing here is
    /// written down again: this is the reading side.
    fn replay(&mut self, changes: Vec<Change>) -> Result<()> {
        for change in changes {
            match change {
                Change::NewTable { name, key, columns } => {
                    self.create_table_full(&name, &key, columns)?;
                }
                Change::DropTable { name } => {
                    if let Some(i) = self.by_name.remove(&name) {
                        self.tables[i].rows.clear();
                        self.tables[i].dropped = true;
                    }
                }
                Change::Put { table, key, row } => {
                    let t = self.tables.get_mut(table as usize)
                        .ok_or_else(|| Error::Disk(format!("the log names table {table}, which is not there")))?;
                    t.rows.insert(key, row);
                }
                Change::Set { table, key, col, value } => {
                    let t = self.tables.get_mut(table as usize)
                        .ok_or_else(|| Error::Disk(format!("the log names table {table}, which is not there")))?;
                    if let Some(row) = t.rows.get_mut(&key) {
                        if let Some(slot) = row.get_mut(col as usize) { *slot = value; }
                    }
                }
                Change::Delete { table, key } => {
                    let t = self.tables.get_mut(table as usize)
                        .ok_or_else(|| Error::Disk(format!("the log names table {table}, which is not there")))?;
                    t.rows.remove(&key);
                }
                Change::NewIndex { name, table, column } => {
                    let t = self.tables.get(table as usize)
                        .ok_or_else(|| Error::Disk(format!("the log names table {table}, which is not there")))?;
                    let col = t.column_of(&column)
                        .ok_or_else(|| Error::Disk(format!("the log names column {column}, which is not there")))?;
                    // Filled in once the replay is over, because the rows
                    // it covers may still be coming.
                    let slot = self.indexes.len();
                    self.indexes.push(Index {
                        name: name.clone(),
                        table: TableId(table as usize),
                        column: col,
                        entries: BTreeMap::new(),
                        dropped: false,
                    });
                    self.index_names.insert(name, slot);
                    self.tables[table as usize].watchers.push(slot);
                }
                Change::DropIndex { name } => {
                    if let Some(slot) = self.index_names.remove(&name) {
                        self.indexes[slot].dropped = true;
                        let table = self.indexes[slot].table;
                        self.tables[table.0].watchers.retain(|&s| s != slot);
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kv() -> (Db, TableId) {
        let mut db = Db::new();
        let id = db
            .create_table("kv", &[("a", Kind::Int), ("b", Kind::Real), ("c", Kind::Text)])
            .unwrap();
        (db, id)
    }

    fn row(i: i64) -> Row {
        vec![Value::Int(i), Value::Real(i as f64 * 1.5), Value::Text(format!("row {i}"))]
    }

    #[test]
    fn a_value_knows_what_it_is() {
        assert_eq!(Value::Int(1).type_name(), "integer");
        assert_eq!(Value::Text("x".into()).type_name(), "text");
        assert_eq!(Value::Null.type_name(), "null");
    }

    #[test]
    fn a_row_goes_in_and_comes_back() {
        let (mut db, id) = kv();
        db.insert(id, 7, row(7)).unwrap();
        assert_eq!(db.table(id).get(7), Some(&row(7)));
        assert_eq!(db.table(id).get_at(7, 0), Some(&Value::Int(7)));
        assert_eq!(db.table(id).len(), 1);
    }

    #[test]
    fn a_missing_row_is_none_not_a_panic() {
        let (db, id) = kv();
        assert_eq!(db.table(id).get(1), None);
        assert_eq!(db.table(id).get_at(1, 0), None);
    }

    #[test]
    fn insert_refuses_to_overwrite_and_put_agrees_to() {
        let (mut db, id) = kv();
        db.insert(id, 1, row(1)).unwrap();
        assert_eq!(db.insert(id, 1, row(2)), Err(Error::KeyExists(1)));
        let old = db.put(id, 1, row(2)).unwrap();
        assert_eq!(old, Some(row(1)));
        assert_eq!(db.table(id).get_at(1, 0), Some(&Value::Int(2)));
    }

    #[test]
    fn a_row_of_the_wrong_shape_is_refused() {
        let (mut db, id) = kv();
        let short = vec![Value::Int(1)];
        assert_eq!(
            db.insert(id, 1, short),
            Err(Error::WrongWidth { want: 3, got: 1 })
        );
        let wrong = vec![Value::Text("no".into()), Value::Real(1.0), Value::Text("x".into())];
        assert_eq!(
            db.insert(id, 1, wrong),
            Err(Error::WrongKind { column: "a".into(), want: "integer", got: "text" })
        );
    }

    #[test]
    fn a_column_can_forbid_null() {
        let mut db = Db::new();
        let id = db
            .create_table_full(
                "t",
                "id",
                vec![Column { name: "a".into(), kind: Kind::Int, null_ok: false }],
            )
            .unwrap();
        assert_eq!(
            db.insert(id, 1, vec![Value::Null]),
            Err(Error::NotNull("a".into()))
        );
        db.insert(id, 1, vec![Value::Int(1)]).unwrap();
        assert_eq!(
            db.update(id, 1, 0, Value::Null),
            Err(Error::NotNull("a".into()))
        );
    }

    #[test]
    fn update_changes_one_value_and_checks_it() {
        let (mut db, id) = kv();
        db.insert(id, 1, row(1)).unwrap();
        db.update(id, 1, 0, Value::Int(99)).unwrap();
        assert_eq!(db.table(id).get_at(1, 0), Some(&Value::Int(99)));
        assert_eq!(
            db.update(id, 1, 0, Value::Text("no".into())),
            Err(Error::WrongKind { column: "a".into(), want: "integer", got: "text" })
        );
        assert_eq!(db.update(id, 5, 0, Value::Int(1)), Err(Error::NoKey(5)));
        assert_eq!(db.update(id, 1, 9, Value::Int(1)), Err(Error::NoColumn(9)));
    }

    #[test]
    fn delete_takes_a_row_out_once() {
        let (mut db, id) = kv();
        db.insert(id, 1, row(1)).unwrap();
        assert!(db.delete(id, 1).unwrap());
        assert!(!db.delete(id, 1).unwrap());
        assert!(db.table(id).is_empty());
    }

    #[test]
    fn rows_come_back_in_key_order() {
        let (mut db, id) = kv();
        for k in [5, 1, 9, 3] {
            db.insert(id, k, row(k)).unwrap();
        }
        let keys: Vec<i64> = db.table(id).iter().map(|(k, _)| k).collect();
        assert_eq!(keys, vec![1, 3, 5, 9]);
        let in_range: Vec<i64> = db.table(id).range(2, 6).map(|(k, _)| k).collect();
        assert_eq!(in_range, vec![3, 5]);
    }

    #[test]
    fn two_tables_keep_their_own_rows() {
        let mut db = Db::new();
        let a = db.create_table("a", &[("x", Kind::Int)]).unwrap();
        let b = db.create_table("b", &[("x", Kind::Int)]).unwrap();
        db.insert(a, 1, vec![Value::Int(10)]).unwrap();
        db.insert(b, 1, vec![Value::Int(20)]).unwrap();
        assert_eq!(db.table(a).get_at(1, 0), Some(&Value::Int(10)));
        assert_eq!(db.table(b).get_at(1, 0), Some(&Value::Int(20)));
        assert_eq!(db.create_table("a", &[]), Err(Error::TableExists("a".into())));
    }

    #[test]
    fn a_table_is_found_by_name_once_and_then_by_id() {
        let (mut db, id) = kv();
        db.insert(id, 1, row(1)).unwrap();
        let again = db.table_id("kv").unwrap();
        assert_eq!(again, id);
        assert_eq!(db.table_id("nope"), Err(Error::NoTable("nope".into())));
        let names: Vec<&str> = db.table_names().collect();
        assert_eq!(names, vec!["kv"]);
    }

    #[test]
    fn a_dropped_table_is_gone_by_name() {
        let (mut db, _) = kv();
        db.drop_table("kv").unwrap();
        assert_eq!(db.table_id("kv"), Err(Error::NoTable("kv".into())));
        assert_eq!(db.drop_table("kv"), Err(Error::NoTable("kv".into())));
    }

    #[test]
    fn errors_read_as_plain_english() {
        assert_eq!(Error::NoKey(4).to_string(), "there is no row with key 4");
        assert_eq!(
            Error::WrongWidth { want: 3, got: 1 }.to_string(),
            "the table has 3 columns and the row has 1"
        );
    }
}
