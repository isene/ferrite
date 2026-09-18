//! ferrite: an embedded SQL database that keeps its tables in memory.
//!
//! A table holds its rows in a B-tree map under an integer key, and the
//! API is plain Rust calls. SQL compiles down to exactly those calls, so
//! whatever they cost is the floor for everything above.
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

use std::collections::{BTreeMap, HashMap};
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
    /// A row with the same value in a unique index is already here.
    Unique(String),
    /// A foreign key points at a row that is not there, or a row that
    /// others still point at was to be deleted.
    ForeignKey(String),
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
            Error::Unique(n) => write!(f, "a row with the same {n} is already there"),
            Error::ForeignKey(s) => write!(f, "{s}"),
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
    /// What an insert that does not name this column puts in it.
    pub default: Value,
}

/// A row is its values in column order. The key is not among them: it
/// is what the row is filed under.
pub type Row = Vec<Value>;

/// A column of this table that holds the key of a row in another. The
/// row here can only point at a row that is there, and when `cascade`
/// is set it goes when that row goes.
#[derive(Debug, Clone, PartialEq)]
pub struct ForeignKey {
    pub column: usize,
    pub parent: TableId,
    pub cascade: bool,
}

/// One table, with its rows under an integer key.
///
/// The rows sit in one vector, the slab, and the key tree maps a key to
/// a slot in it. An index does the same, so a row found through an index
/// is one array lookup away rather than a second walk down the tree. The
/// tree keeps the keys in order, which is what a range, an ORDER BY and
/// the next key all lean on.
#[derive(Debug, Clone)]
pub struct Table {
    name: String,
    /// True once the table has been thrown away. The slot stays, because
    /// the log names tables by their number.
    dropped: bool,
    /// What the key is called in SQL: the INTEGER PRIMARY KEY column
    /// when there is one, else `rowid`. The key is not stored in the
    /// row; it is what the row is filed under.
    key: String,
    columns: Vec<Column>,
    /// Key to slot.
    rows: BTreeMap<i64, u32>,
    /// The rows themselves. A slot that has been freed holds None until
    /// an insert takes it again.
    slab: Vec<Option<Row>>,
    free: Vec<u32>,
    /// Which indexes have to be kept up as rows change. Empty is the
    /// usual case, and one test of that keeps the cost off the hot path.
    watchers: Vec<usize>,
    /// The key an insert gets when it gives none: one past the biggest
    /// ever used while this table has been open.
    next: i64,
    foreign_keys: Vec<ForeignKey>,
    /// Tables with a foreign key pointing here, which a delete has to
    /// look at. Empty for most tables, and one test of that is the
    /// whole cost.
    children: Vec<usize>,
}

impl Table {
    pub fn name(&self) -> &str { &self.name }
    pub fn key_name(&self) -> &str { &self.key }
    pub fn columns(&self) -> &[Column] { &self.columns }
    pub fn foreign_keys(&self) -> &[ForeignKey] { &self.foreign_keys }
    /// True when another table's foreign key points here, so a delete
    /// can reach into it.
    pub fn is_pointed_at(&self) -> bool { !self.children.is_empty() }
    pub fn len(&self) -> usize { self.rows.len() }
    pub fn is_empty(&self) -> bool { self.rows.is_empty() }

    /// Which column that name is, if any.
    pub fn column_of(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }

    /// The row under that key.
    #[inline]
    pub fn get(&self, key: i64) -> Option<&Row> {
        let slot = *self.rows.get(&key)?;
        self.slab[slot as usize].as_ref()
    }

    /// One value out of the row under that key.
    #[inline]
    pub fn get_at(&self, key: i64, column: usize) -> Option<&Value> {
        self.get(key)?.get(column)
    }

    /// The row in a given slot. An index hands slots out, so a row it
    /// found costs an array lookup and nothing more.
    #[inline]
    pub fn at_slot(&self, slot: u32) -> Option<&Row> {
        self.slab.get(slot as usize)?.as_ref()
    }

    /// Which slot a key's row is in.
    #[inline]
    pub(crate) fn slot_of(&self, key: i64) -> Option<u32> { self.rows.get(&key).copied() }

    /// The key the next insert gets when it gives none.
    pub fn next_key(&self) -> i64 { self.next.max(1) }

    #[inline]
    fn bump(&mut self, key: i64) {
        if key >= self.next { self.next = key.saturating_add(1); }
    }

    fn alloc(&mut self, row: Row) -> u32 {
        match self.free.pop() {
            Some(slot) => {
                self.slab[slot as usize] = Some(row);
                slot
            }
            None => {
                self.slab.push(Some(row));
                (self.slab.len() - 1) as u32
            }
        }
    }

    /// Put a row under a key with no checking at all: for undoing and
    /// for replaying the log, where the row has been checked before.
    pub(crate) fn place(&mut self, key: i64, row: Row) -> Option<Row> {
        self.bump(key);
        if let Some(&slot) = self.rows.get(&key) {
            return self.slab[slot as usize].replace(row);
        }
        let slot = self.alloc(row);
        self.rows.insert(key, slot);
        None
    }

    /// Take a row out and hand it back.
    pub(crate) fn take(&mut self, key: i64) -> Option<Row> {
        let slot = self.rows.remove(&key)?;
        self.free.push(slot);
        self.slab[slot as usize].take()
    }

    pub(crate) fn get_mut(&mut self, key: i64) -> Option<&mut Row> {
        let slot = *self.rows.get(&key)?;
        self.slab[slot as usize].as_mut()
    }

    pub(crate) fn clear(&mut self) {
        self.rows.clear();
        self.slab.clear();
        self.free.clear();
    }

    /// Put a new row in. It is an error if the key is taken, so nothing
    /// is overwritten by accident.
    pub fn insert(&mut self, key: i64, row: Row) -> Result<()> {
        self.check(&row)?;
        if self.rows.contains_key(&key) {
            return Err(Error::KeyExists(key));
        }
        self.insert_known_good(key, row);
        Ok(())
    }

    /// Put a row in, over whatever was there. Gives back the old row.
    pub fn put(&mut self, key: i64, row: Row) -> Result<Option<Row>> {
        self.check(&row)?;
        Ok(self.place(key, row))
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
        let row = self.get_mut(key).ok_or(Error::NoKey(key))?;
        row[column] = value;
        Ok(())
    }

    /// Take a row out. True when there was one.
    pub fn delete(&mut self, key: i64) -> bool { self.take(key).is_some() }

    /// Every row in key order.
    pub fn iter(&self) -> impl Iterator<Item = (i64, &Row)> {
        self.rows.iter().map(|(k, s)| (*k, self.slab[*s as usize].as_ref().expect("a slot the key tree points at is empty")))
    }

    /// Every row in key order, with its slot.
    fn iter_slots(&self) -> impl Iterator<Item = (i64, u32, &Row)> {
        self.rows.iter().map(|(k, s)| (*k, *s, self.slab[*s as usize].as_ref().expect("a slot the key tree points at is empty")))
    }

    /// The rows whose keys fall in a range, in order. A range that ends
    /// before it starts holds nothing, rather than being a mistake:
    /// `WHERE id > 6 AND id < 3` is a fair question with no answer.
    pub fn range(&self, from: i64, to: i64) -> impl Iterator<Item = (i64, &Row)> {
        self.rows.range(from..to.max(from)).map(|(k, s)| (*k, self.slab[*s as usize].as_ref().expect("a slot the key tree points at is empty")))
    }

    /// Put a row in that has already been looked over.
    pub(crate) fn insert_known_good(&mut self, key: i64, row: Row) {
        self.bump(key);
        let slot = self.alloc(row);
        self.rows.insert(key, slot);
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

/// What an index files a row under: the value of one column, or the
/// values of several. One index uses one shape throughout, so the two
/// never meet in a comparison.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum IndexKey {
    One(SortKey),
    Many(Vec<SortKey>),
}

impl IndexKey {
    /// True when any part is null. Null is equal to nothing, so a row
    /// with one cannot clash with another in a unique index.
    fn has_null(&self) -> bool {
        match self {
            IndexKey::One(k) => k.0 == Value::Null,
            IndexKey::Many(ks) => ks.iter().any(|k| k.0 == Value::Null),
        }
    }
}

/// One or more columns of one table, with every row that holds each
/// value.
///
/// A single-column index keeps the row's own value beside its key, not
/// only the value the entry is filed under. Those two can differ: 5 and
/// 5.0 are equal in SQL, so they share an entry, and only the row knows
/// which of them it holds. Keeping it here is what lets a query asking
/// for this column be answered without fetching the row at all.
#[derive(Debug, Clone)]
pub struct Index {
    name: String,
    table: TableId,
    columns: Vec<usize>,
    unique: bool,
    entries: BTreeMap<IndexKey, BTreeMap<i64, (u32, Value)>>,
    dropped: bool,
}

impl Index {
    pub fn name(&self) -> &str { &self.name }
    pub fn table(&self) -> TableId { self.table }
    pub fn columns(&self) -> &[usize] { &self.columns }
    /// The column, for an index on one.
    pub fn column(&self) -> usize { self.columns[0] }
    pub fn is_unique(&self) -> bool { self.unique }

    /// The rows whose value in this one column equals `value`, in key
    /// order, each with its slot and the value it actually holds.
    pub fn rows_for(&self, value: &Value) -> Option<&BTreeMap<i64, (u32, Value)>> {
        self.entries.get(&IndexKey::One(SortKey(value.clone())))
    }

    /// The rows whose values in the index's columns equal these, in
    /// that order.
    pub fn rows_for_all(&self, values: &[Value]) -> Option<&BTreeMap<i64, (u32, Value)>> {
        self.entries.get(&Self::key_from(values))
    }

    /// Just the keys of those rows.
    pub fn keys_for(&self, value: &Value) -> Option<impl Iterator<Item = &i64>> {
        self.rows_for(value).map(|m| m.keys())
    }

    /// Every value in order, with the rows holding it.
    pub fn iter(&self) -> impl Iterator<Item = (&IndexKey, &BTreeMap<i64, (u32, Value)>)> {
        self.entries.iter()
    }

    fn key_from(values: &[Value]) -> IndexKey {
        match values {
            [one] => IndexKey::One(SortKey(one.clone())),
            many => IndexKey::Many(many.iter().map(|v| SortKey(v.clone())).collect()),
        }
    }

    /// What this row files under.
    fn key_of(&self, row: &Row) -> IndexKey {
        match self.columns.as_slice() {
            [c] => IndexKey::One(SortKey(row[*c].clone())),
            cs => IndexKey::Many(cs.iter().map(|c| SortKey(row[*c].clone())).collect()),
        }
    }

    /// The value kept beside the key: the row's own, for one column.
    fn own(&self, row: &Row) -> Value {
        match self.columns.as_slice() {
            [c] => row[*c].clone(),
            _ => Value::Null,
        }
    }

    fn add(&mut self, key: IndexKey, own: Value, row_key: i64, slot: u32) {
        self.entries.entry(key).or_default().insert(row_key, (slot, own));
    }

    fn remove(&mut self, key: &IndexKey, row_key: i64) {
        if let Some(rows) = self.entries.get_mut(key) {
            rows.remove(&row_key);
            if rows.is_empty() { self.entries.remove(key); }
        }
    }

    /// The key of a row, other than `except`, filed under this key.
    fn clash(&self, key: &IndexKey, except: i64) -> Option<i64> {
        if !self.unique || key.has_null() { return None; }
        self.entries.get(key)?.keys().find(|k| **k != except).copied()
    }
}

// ── The database ───────────────────────────────────────────────────────

/// Which table. Handing one of these back and taking it again means a
/// caller reaches its table by array index rather than by hashing a
/// name on every row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableId(usize);

/// Every table, in memory, and the files they are kept in when there
/// are any.
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
    /// The key of the last row put in.
    last_key: i64,
}

/// Where a statement began, for taking it back on its own.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Mark {
    undo: usize,
    journal: usize,
    count: u32,
    was_in_txn: bool,
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
    /// [`Db::create_table_full`] to forbid null or set a default.
    pub fn create_table(&mut self, name: &str, columns: &[(&str, Kind)]) -> Result<TableId> {
        let columns: Vec<Column> = columns
            .iter()
            .map(|(n, k)| Column { name: (*n).to_string(), kind: *k, null_ok: true, default: Value::Null })
            .collect();
        self.create_table_full(name, "id", columns, Vec::new())
    }

    /// Make a table, saying what its key is called, what its columns
    /// are and which of them point at rows of other tables.
    pub fn create_table_full(
        &mut self,
        name: &str,
        key: &str,
        columns: Vec<Column>,
        foreign_keys: Vec<ForeignKey>,
    ) -> Result<TableId> {
        if self.by_name.contains_key(name) {
            return Err(Error::TableExists(name.to_string()));
        }
        for fk in &foreign_keys {
            if fk.column >= columns.len() { return Err(Error::NoColumn(fk.column)); }
            if fk.parent.0 >= self.tables.len() {
                return Err(Error::NoTable(format!("table {}", fk.parent.0)));
            }
        }
        let id = TableId(self.tables.len());
        for fk in &foreign_keys {
            self.tables[fk.parent.0].children.push(id.0);
        }
        self.tables.push(Table {
            name: name.to_string(),
            dropped: false,
            key: key.to_string(),
            columns,
            rows: BTreeMap::new(),
            slab: Vec::new(),
            free: Vec::new(),
            watchers: Vec::new(),
            next: 1,
            foreign_keys: foreign_keys.clone(),
            children: Vec::new(),
        });
        self.by_name.insert(name.to_string(), id.0);
        let columns = self.tables[id.0].columns.clone();
        self.record(Change::NewTable {
            name: name.to_string(),
            key: key.to_string(),
            columns,
            foreign_keys,
        })?;
        Ok(id)
    }

    /// Find a table by name. Do this once and keep the id.
    pub fn table_id(&self, name: &str) -> Result<TableId> {
        self.by_name.get(name).map(|i| TableId(*i)).ok_or_else(|| Error::NoTable(name.to_string()))
    }

    #[inline]
    pub fn table(&self, id: TableId) -> &Table { &self.tables[id.0] }

    pub fn table_names(&self) -> impl Iterator<Item = &str> {
        self.tables.iter().filter(|t| !t.dropped).map(|t| t.name.as_str())
    }

    /// The key the next insert into this table gets when it gives none.
    pub fn next_key(&self, table: TableId) -> i64 { self.tables[table.0].next_key() }

    /// The key of the last row put in, by any table.
    pub fn last_insert_key(&self) -> i64 { self.last_key }

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

    /// Run several statements, cut apart at the semicolons, one after
    /// the other. The first that fails stops the rest.
    pub fn execute_batch(&mut self, sql: &str) -> Result<()> {
        for one in sql::split(sql) {
            self.execute(&one, &[])?;
        }
        Ok(())
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
            self.step_back(step);
        }
    }

    /// One step back. It has to reach the indexes too, or a rolled back
    /// insert would stay findable through one.
    fn step_back(&mut self, step: Undo) {
        match step {
            Undo::Added(id, key) | Undo::Was(id, key, None) => {
                if let Some(row) = self.tables[id.0].take(key) {
                    self.index_take(id, key, &row);
                }
            }
            Undo::Was(id, key, Some(row)) => {
                if let Some(now) = self.tables[id.0].place(key, row.clone()) {
                    self.index_take(id, key, &now);
                }
                self.index_add(id, key, &row);
            }
        }
    }

    pub fn in_transaction(&self) -> bool { self.in_txn }

    /// Where a statement starts, so that one that fails halfway can be
    /// taken back on its own, inside a transaction or not. SQL promises
    /// that a statement changes everything it matched or nothing.
    pub(crate) fn mark(&mut self) -> Mark {
        let was_in_txn = self.in_txn;
        if !was_in_txn {
            self.in_txn = true;
            self.undo.clear();
        }
        Mark { undo: self.undo.len(), journal: self.journal.len(), count: self.journal_count, was_in_txn }
    }

    /// The statement went through: keep it, and commit it if it was on
    /// its own.
    pub(crate) fn release(&mut self, m: Mark) -> Result<()> {
        if m.was_in_txn { Ok(()) } else { self.commit() }
    }

    /// The statement failed: take back everything since the mark.
    pub(crate) fn undo_to(&mut self, m: Mark) {
        while self.undo.len() > m.undo {
            let step = self.undo.pop().expect("more undo steps than the mark");
            self.step_back(step);
        }
        self.journal.truncate(m.journal);
        self.journal_count = m.count;
        if !m.was_in_txn {
            self.in_txn = false;
            self.journal.clear();
            self.journal_count = 0;
        }
    }

    /// Remember that a row has just been added.
    #[inline]
    pub(crate) fn note_insert(&mut self, table: TableId, key: i64) {
        if self.in_txn { self.undo.push(Undo::Added(table, key)); }
    }

    /// Remember what a row looked like before it is changed or removed.
    #[inline]
    pub(crate) fn note_change(&mut self, table: TableId, key: i64) {
        if self.in_txn {
            let was = self.tables[table.0].get(key).cloned();
            self.undo.push(Undo::Was(table, key, was));
        }
    }

    /// Throw a table away, with everything in it. A table that other
    /// tables' foreign keys point at stays.
    pub fn drop_table(&mut self, name: &str) -> Result<()> {
        let i = *self.by_name.get(name).ok_or_else(|| Error::NoTable(name.to_string()))?;
        if self.tables[i].children.iter().any(|&c| !self.tables[c].dropped) {
            return Err(Error::ForeignKey(format!("other tables still point at {name}")));
        }
        self.forget_table(i);
        self.record(Change::DropTable { name: name.to_string() })
    }

    /// Empty a table and mark it gone, and take its indexes with it. The
    /// log records only the table, and replay does the same, so the two
    /// sides agree without an extra record.
    fn forget_table(&mut self, i: usize) {
        self.tables[i].clear();
        self.tables[i].dropped = true;
        self.by_name.remove(&self.tables[i].name.clone());
        for slot in std::mem::take(&mut self.tables[i].watchers) {
            self.indexes[slot].entries.clear();
            self.indexes[slot].dropped = true;
            self.index_names.remove(&self.indexes[slot].name.clone());
        }
        for fk in std::mem::take(&mut self.tables[i].foreign_keys) {
            self.tables[fk.parent.0].children.retain(|&c| c != i);
        }
    }

    // ── Indexes ────────────────────────────────────────────────────────

    /// Make an index on one column, and fill it from the rows already
    /// there.
    pub fn create_index(&mut self, name: &str, table: TableId, column: usize) -> Result<IndexId> {
        self.create_index_full(name, table, vec![column], false)
    }

    /// Make an index on one or more columns. A unique one refuses two
    /// rows with the same values, and is refused itself if the rows
    /// already there have any.
    pub fn create_index_full(
        &mut self,
        name: &str,
        table: TableId,
        columns: Vec<usize>,
        unique: bool,
    ) -> Result<IndexId> {
        if self.index_names.contains_key(name) {
            return Err(Error::IndexExists(name.to_string()));
        }
        if columns.is_empty() { return Err(Error::Sql("an index needs a column".into())); }
        for &c in &columns {
            if c >= self.tables[table.0].columns.len() { return Err(Error::NoColumn(c)); }
        }
        let mut index = Index {
            name: name.to_string(),
            table,
            columns,
            unique,
            entries: BTreeMap::new(),
            dropped: false,
        };
        for (key, slot, row) in self.tables[table.0].iter_slots() {
            let k = index.key_of(row);
            if let Some(other) = index.clash(&k, key) {
                return Err(Error::Unique(format!("{} (rows {other} and {key})", index.what())));
            }
            index.add(k, index.own(row), key, slot);
        }
        let column_names: Vec<String> =
            index.columns.iter().map(|&c| self.tables[table.0].columns[c].name.clone()).collect();
        let slot = self.indexes.len();
        self.indexes.push(index);
        self.index_names.insert(name.to_string(), slot);
        self.tables[table.0].watchers.push(slot);
        self.record(Change::NewIndex {
            name: name.to_string(),
            table: table.0 as u32,
            columns: column_names,
            unique,
        })?;
        Ok(IndexId(slot))
    }

    pub fn index_id(&self, name: &str) -> Result<IndexId> {
        self.index_names.get(name).map(|i| IndexId(*i)).ok_or_else(|| Error::NoIndex(name.to_string()))
    }

    pub fn index(&self, id: IndexId) -> &Index { &self.indexes[id.0] }

    /// An index on this one column of this table, if there is one.
    pub fn index_on(&self, table: TableId, column: usize) -> Option<IndexId> {
        self.tables[table.0]
            .watchers
            .iter()
            .find(|&&s| !self.indexes[s].dropped && self.indexes[s].columns == [column])
            .map(|&s| IndexId(s))
    }

    /// An index whose columns are all among these, if there is one. The
    /// widest wins, since it narrows the rows down the most.
    pub fn index_within(&self, table: TableId, columns: &[usize]) -> Option<IndexId> {
        self.tables[table.0]
            .watchers
            .iter()
            .filter(|&&s| {
                let ix = &self.indexes[s];
                !ix.dropped && ix.columns.iter().all(|c| columns.contains(c))
            })
            .max_by_key(|&&s| self.indexes[s].columns.len())
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
        let triples: Vec<(i64, u32, IndexKey, Value)> = self.tables[table.0]
            .iter_slots()
            .map(|(k, s, row)| (k, s, self.indexes[slot].key_of(row), self.indexes[slot].own(row)))
            .collect();
        for (key, at, k, own) in triples {
            self.indexes[slot].add(k, own, key, at);
        }
    }

    /// Put a row into every index watching its table.
    fn index_add(&mut self, table: TableId, key: i64, row: &Row) {
        if self.tables[table.0].watchers.is_empty() { return; }
        let Some(at) = self.tables[table.0].slot_of(key) else { return };
        for slot in self.tables[table.0].watchers.clone() {
            let k = self.indexes[slot].key_of(row);
            let own = self.indexes[slot].own(row);
            self.indexes[slot].add(k, own, key, at);
        }
    }

    /// Take a row out of every index watching its table.
    fn index_take(&mut self, table: TableId, key: i64, row: &Row) {
        if self.tables[table.0].watchers.is_empty() { return; }
        for slot in self.tables[table.0].watchers.clone() {
            let k = self.indexes[slot].key_of(row);
            self.indexes[slot].remove(&k, key);
        }
    }

    #[inline]
    fn watched(&self, table: TableId) -> bool { !self.tables[table.0].watchers.is_empty() }

    /// The rows that an insert of this row under this key would clash
    /// with: the one under the key, and any that a unique index already
    /// holds with the same values.
    pub fn clashes(&self, table: TableId, key: i64, row: &Row) -> Vec<i64> {
        let t = &self.tables[table.0];
        let mut out = Vec::new();
        if t.rows.contains_key(&key) { out.push(key); }
        for &slot in &t.watchers {
            let ix = &self.indexes[slot];
            if let Some(other) = ix.clash(&ix.key_of(row), key) {
                if !out.contains(&other) { out.push(other); }
            }
        }
        out
    }

    /// Refuse a row that a unique index already holds the like of.
    fn unique_check(&self, table: TableId, key: i64, row: &Row) -> Result<()> {
        for &slot in &self.tables[table.0].watchers {
            let ix = &self.indexes[slot];
            if let Some(other) = ix.clash(&ix.key_of(row), key) {
                return Err(Error::Unique(format!("{} (row {other})", ix.what())));
            }
        }
        Ok(())
    }

    /// Refuse a row whose foreign keys point at rows that are not there.
    fn foreign_check(&self, table: TableId, row: &Row) -> Result<()> {
        for fk in &self.tables[table.0].foreign_keys {
            match &row[fk.column] {
                Value::Null => {}
                Value::Int(k) if self.tables[fk.parent.0].rows.contains_key(k) => {}
                other => {
                    return Err(Error::ForeignKey(format!(
                        "{} points at row {other:?} of {}, which is not there",
                        self.tables[table.0].columns[fk.column].name,
                        self.tables[fk.parent.0].name
                    )))
                }
            }
        }
        Ok(())
    }

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

    /// Put a new row in. It is an error if the key is taken, if a
    /// unique index already holds the like of it, or if a foreign key
    /// points at nothing.
    pub fn insert(&mut self, table: TableId, key: i64, row: Row) -> Result<()> {
        let watched = self.watched(table);
        if watched { self.unique_check(table, key, &row)?; }
        if !self.tables[table.0].foreign_keys.is_empty() { self.foreign_check(table, &row)?; }
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
        self.last_key = key;
        self.note_insert(table, key);
        if watched {
            let row = self.tables[table.0].get(key).cloned().expect("the row that was just put in");
            self.index_add(table, key, &row);
        }
        self.close_if_alone()
    }

    /// Put a row in over whatever was there. Gives back the old row.
    pub fn put(&mut self, table: TableId, key: i64, row: Row) -> Result<Option<Row>> {
        let watched = self.watched(table);
        if watched { self.unique_check(table, key, &row)?; }
        if !self.tables[table.0].foreign_keys.is_empty() { self.foreign_check(table, &row)?; }
        if self.recording() {
            self.tables[table.0].fits_row(&row)?;
            let t = table.0 as u32;
            log::put_row(self.opening(), t, key, &row);
        }
        self.note_change(table, key);
        if watched {
            if let Some(old) = self.tables[table.0].get(key).cloned() {
                self.index_take(table, key, &old);
            }
        }
        let old = self.tables[table.0].put(key, row)?;
        self.last_key = key;
        if watched {
            let now = self.tables[table.0].get(key).cloned().expect("the row that was just put in");
            self.index_add(table, key, &now);
        }
        self.close_if_alone()?;
        Ok(old)
    }

    /// Change one value in an existing row.
    pub fn update(&mut self, table: TableId, key: i64, column: usize, value: Value) -> Result<()> {
        let watched = self.watched(table);
        // The indexes this column is part of, with what the row files
        // under now, before anything moves.
        let mut touched: Vec<(usize, IndexKey)> = Vec::new();
        if watched {
            let row = self.tables[table.0].get(key).ok_or(Error::NoKey(key))?;
            for &slot in &self.tables[table.0].watchers {
                let ix = &self.indexes[slot];
                if !ix.columns.contains(&column) { continue; }
                if ix.unique {
                    let mut after = row.clone();
                    after[column] = value.clone();
                    if let Some(other) = ix.clash(&ix.key_of(&after), key) {
                        return Err(Error::Unique(format!("{} (row {other})", ix.what())));
                    }
                }
                touched.push((slot, ix.key_of(row)));
            }
        }
        if self.tables[table.0].foreign_keys.iter().any(|fk| fk.column == column) {
            let mut after = self.tables[table.0].get(key).ok_or(Error::NoKey(key))?.clone();
            after[column] = value.clone();
            self.foreign_check(table, &after)?;
        }
        if self.recording() {
            self.tables[table.0].can_update(key, column, &value)?;
            let t = table.0 as u32;
            log::put_set(self.opening(), t, key, column as u32, &value);
        }
        self.note_change(table, key);
        self.tables[table.0].update(key, column, value)?;
        if !touched.is_empty() {
            let at = self.tables[table.0].slot_of(key).expect("the row that was just changed");
            let row = self.tables[table.0].get(key).expect("the row that was just changed").clone();
            for (slot, was) in touched {
                self.indexes[slot].remove(&was, key);
                let k = self.indexes[slot].key_of(&row);
                let own = self.indexes[slot].own(&row);
                self.indexes[slot].add(k, own, key, at);
            }
        }
        self.close_if_alone()
    }

    /// Take a row out. True when there was one. Rows in other tables
    /// that point at it go with it when their foreign key says so, and
    /// stop the delete when it does not.
    pub fn delete(&mut self, table: TableId, key: i64) -> Result<bool> {
        if self.tables[table.0].get(key).is_none() { return Ok(false); }
        if !self.tables[table.0].children.is_empty() {
            self.delete_children(table, key)?;
        }
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

    /// The rows in other tables pointing at this one: gone if their
    /// foreign key cascades, and an error otherwise.
    fn delete_children(&mut self, table: TableId, key: i64) -> Result<()> {
        let children = self.tables[table.0].children.clone();
        for c in children {
            if self.tables[c].dropped { continue; }
            let fks: Vec<ForeignKey> = self.tables[c]
                .foreign_keys
                .iter()
                .filter(|fk| fk.parent == table)
                .cloned()
                .collect();
            for fk in fks {
                let want = Value::Int(key);
                let keys: Vec<i64> = match self.index_on(TableId(c), fk.column) {
                    Some(ix) => self.indexes[ix.0].keys_for(&want).map(|k| k.copied().collect()).unwrap_or_default(),
                    None => self.tables[c].iter().filter(|(_, r)| r[fk.column] == want).map(|(k, _)| k).collect(),
                };
                if keys.is_empty() { continue; }
                if !fk.cascade {
                    return Err(Error::ForeignKey(format!(
                        "rows of {} still point at row {key} of {}",
                        self.tables[c].name, self.tables[table.0].name
                    )));
                }
                for k in keys {
                    self.delete(TableId(c), k)?;
                }
            }
        }
        Ok(())
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
        let live: u64 = self.tables.iter().map(|t| t.len() as u64).sum();
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
                foreign_keys: t.foreign_keys.clone(),
            });
            if t.dropped {
                out.push(Change::DropTable { name: t.name.clone() });
                continue;
            }
            for (key, row) in t.iter() {
                out.push(Change::Put { table: i as u32, key, row: row.clone() });
            }
            // The rows that were deleted are not written out, so their
            // keys would come round again without this.
            out.push(Change::Next { table: i as u32, key: t.next });
        }
        for index in &self.indexes {
            if index.dropped { continue; }
            let t = &self.tables[index.table.0];
            out.push(Change::NewIndex {
                name: index.name.clone(),
                table: index.table.0 as u32,
                columns: index.columns.iter().map(|&c| t.columns[c].name.clone()).collect(),
                unique: index.unique,
            });
        }
        out
    }

    /// Put a commit from the log back into the tables. Nothing here is
    /// written down again: this is the reading side.
    fn replay(&mut self, changes: Vec<Change>) -> Result<()> {
        for change in changes {
            match change {
                Change::NewTable { name, key, columns, foreign_keys } => {
                    self.create_table_full(&name, &key, columns, foreign_keys)?;
                }
                Change::DropTable { name } => {
                    if let Some(&i) = self.by_name.get(&name) {
                        self.forget_table(i);
                    }
                }
                Change::Put { table, key, row } => {
                    let t = self.tables.get_mut(table as usize)
                        .ok_or_else(|| Error::Disk(format!("the log names table {table}, which is not there")))?;
                    t.place(key, row);
                }
                Change::Set { table, key, col, value } => {
                    let t = self.tables.get_mut(table as usize)
                        .ok_or_else(|| Error::Disk(format!("the log names table {table}, which is not there")))?;
                    if let Some(row) = t.get_mut(key) {
                        if let Some(cell) = row.get_mut(col as usize) { *cell = value; }
                    }
                }
                Change::Delete { table, key } => {
                    let t = self.tables.get_mut(table as usize)
                        .ok_or_else(|| Error::Disk(format!("the log names table {table}, which is not there")))?;
                    t.take(key);
                }
                Change::Next { table, key } => {
                    if let Some(t) = self.tables.get_mut(table as usize) {
                        t.next = t.next.max(key);
                    }
                }
                Change::NewIndex { name, table, columns, unique } => {
                    let t = self.tables.get(table as usize)
                        .ok_or_else(|| Error::Disk(format!("the log names table {table}, which is not there")))?;
                    let mut cols = Vec::with_capacity(columns.len());
                    for column in &columns {
                        cols.push(t.column_of(column).ok_or_else(|| {
                            Error::Disk(format!("the log names column {column}, which is not there"))
                        })?);
                    }
                    // Filled in once the replay is over, because the rows
                    // it covers may still be coming.
                    let slot = self.indexes.len();
                    self.indexes.push(Index {
                        name: name.clone(),
                        table: TableId(table as usize),
                        columns: cols,
                        unique,
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

impl Index {
    /// The index's columns, for an error message.
    fn what(&self) -> String {
        format!("{} ({} columns)", self.name, self.columns.len())
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
        assert_eq!(db.last_insert_key(), 7);
        assert_eq!(db.next_key(id), 8);
    }

    #[test]
    fn a_missing_row_is_none_not_a_panic() {
        let (db, id) = kv();
        assert_eq!(db.table(id).get(1), None);
        assert_eq!(db.table(id).get_at(1, 0), None);
        assert_eq!(db.next_key(id), 1);
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
                vec![Column { name: "a".into(), kind: Kind::Int, null_ok: false, default: Value::Null }],
                Vec::new(),
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
        assert_eq!(db.next_key(id), 10);
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
    fn a_unique_index_refuses_a_second_of_the_same() {
        let (mut db, id) = kv();
        db.insert(id, 1, row(1)).unwrap();
        db.insert(id, 2, row(2)).unwrap();
        db.create_index_full("kv_a", id, vec![0], true).unwrap();
        // The same a as row 2.
        assert!(matches!(db.insert(id, 3, row(2)), Err(Error::Unique(_))));
        assert!(matches!(db.update(id, 1, 0, Value::Int(2)), Err(Error::Unique(_))));
        // Nulls never clash.
        db.insert(id, 3, vec![Value::Null, Value::Real(0.0), Value::Null]).unwrap();
        db.insert(id, 4, vec![Value::Null, Value::Real(0.0), Value::Null]).unwrap();
        // A row may keep its own value.
        db.update(id, 1, 0, Value::Int(1)).unwrap();
        assert_eq!(db.clashes(id, 9, &row(2)).as_slice(), &[2]);
        assert_eq!(db.clashes(id, 2, &row(7)).as_slice(), &[2]);
        assert!(db.clashes(id, 9, &row(7)).is_empty());
        // And one over two columns, made after the fact, checks what is
        // there first.
        db.insert(id, 5, vec![Value::Int(5), Value::Real(1.0), Value::Text("x".into())]).unwrap();
        db.insert(id, 6, vec![Value::Int(6), Value::Real(1.0), Value::Text("x".into())]).unwrap();
        assert!(matches!(db.create_index_full("kv_bc", id, vec![1, 2], true), Err(Error::Unique(_))));
        assert!(db.index_id("kv_bc").is_err());
        db.create_index_full("kv_bc", id, vec![1, 2], false).unwrap();
        let ix = db.index_id("kv_bc").unwrap();
        let both: Vec<i64> = db.index(ix).rows_for_all(&[Value::Real(1.0), Value::Text("x".into())]).unwrap().keys().copied().collect();
        assert_eq!(both, vec![5, 6]);
        assert_eq!(db.index_within(id, &[2, 1, 0]), Some(ix));
        assert_eq!(db.index_within(id, &[2]), None);
    }

    #[test]
    fn a_foreign_key_holds_both_ways() {
        let mut db = Db::new();
        let parent = db.create_table("p", &[("x", Kind::Int)]).unwrap();
        let kid = db
            .create_table_full(
                "k",
                "id",
                vec![Column { name: "p".into(), kind: Kind::Int, null_ok: true, default: Value::Null }],
                vec![ForeignKey { column: 0, parent, cascade: true }],
            )
            .unwrap();
        let held = db
            .create_table_full(
                "h",
                "id",
                vec![Column { name: "p".into(), kind: Kind::Int, null_ok: true, default: Value::Null }],
                vec![ForeignKey { column: 0, parent, cascade: false }],
            )
            .unwrap();
        db.insert(parent, 1, vec![Value::Int(0)]).unwrap();
        db.insert(parent, 2, vec![Value::Int(0)]).unwrap();
        // Pointing at nothing is refused; null points at nothing and is fine.
        assert!(matches!(db.insert(kid, 1, vec![Value::Int(9)]), Err(Error::ForeignKey(_))));
        db.insert(kid, 1, vec![Value::Null]).unwrap();
        db.insert(kid, 2, vec![Value::Int(1)]).unwrap();
        db.insert(kid, 3, vec![Value::Int(1)]).unwrap();
        assert!(matches!(db.update(kid, 2, 0, Value::Int(9)), Err(Error::ForeignKey(_))));
        db.insert(held, 1, vec![Value::Int(2)]).unwrap();
        // The kids go with the parent; the held one holds it.
        assert!(db.delete(parent, 1).unwrap());
        assert_eq!(db.table(kid).len(), 1);
        assert!(matches!(db.delete(parent, 2), Err(Error::ForeignKey(_))));
        assert!(matches!(db.drop_table("p"), Err(Error::ForeignKey(_))));
        db.drop_table("h").unwrap();
        db.drop_table("k").unwrap();
        db.drop_table("p").unwrap();
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
