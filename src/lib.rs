//! ferrite: an embedded SQL database that keeps its tables in memory.
//!
//! Phase 1 of `PLAN.md`: the core, with no SQL yet. A table holds its
//! rows in a B-tree map under an integer primary key, and the API is
//! plain Rust calls. SQL arrives in phase 2 and compiles down to exactly
//! these calls, so whatever they cost is the floor for everything above.
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
//! db.table_mut(kv).insert(1, vec![Value::Int(7), Value::Text("hello".into())]).unwrap();
//! assert_eq!(db.table(kv).get(1).unwrap()[0], Value::Int(7));
//! ```

use std::collections::{BTreeMap, HashMap};

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
    columns: Vec<Column>,
    rows: BTreeMap<i64, Row>,
}

impl Table {
    pub fn name(&self) -> &str { &self.name }
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

    /// The rows whose keys fall in a range, in order.
    pub fn range(&self, from: i64, to: i64) -> impl Iterator<Item = (i64, &Row)> {
        self.rows.range(from..to).map(|(k, r)| (*k, r))
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
        self.create_table_full(name, columns)
    }

    pub fn create_table_full(&mut self, name: &str, columns: Vec<Column>) -> Result<TableId> {
        if self.by_name.contains_key(name) {
            return Err(Error::TableExists(name.to_string()));
        }
        let id = TableId(self.tables.len());
        self.tables.push(Table { name: name.to_string(), columns, rows: BTreeMap::new() });
        self.by_name.insert(name.to_string(), id.0);
        Ok(id)
    }

    /// Find a table by name. Do this once and keep the id.
    pub fn table_id(&self, name: &str) -> Result<TableId> {
        self.by_name.get(name).map(|i| TableId(*i)).ok_or_else(|| Error::NoTable(name.to_string()))
    }

    #[inline]
    pub fn table(&self, id: TableId) -> &Table { &self.tables[id.0] }

    #[inline]
    pub fn table_mut(&mut self, id: TableId) -> &mut Table { &mut self.tables[id.0] }

    pub fn table_names(&self) -> impl Iterator<Item = &str> {
        self.tables.iter().map(|t| t.name.as_str())
    }

    /// Throw a table away, with everything in it.
    pub fn drop_table(&mut self, name: &str) -> Result<()> {
        let i = *self.by_name.get(name).ok_or_else(|| Error::NoTable(name.to_string()))?;
        self.tables[i].rows.clear();
        self.tables[i].columns.clear();
        self.by_name.remove(name);
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
        db.table_mut(id).insert(7, row(7)).unwrap();
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
        db.table_mut(id).insert(1, row(1)).unwrap();
        assert_eq!(db.table_mut(id).insert(1, row(2)), Err(Error::KeyExists(1)));
        let old = db.table_mut(id).put(1, row(2)).unwrap();
        assert_eq!(old, Some(row(1)));
        assert_eq!(db.table(id).get_at(1, 0), Some(&Value::Int(2)));
    }

    #[test]
    fn a_row_of_the_wrong_shape_is_refused() {
        let (mut db, id) = kv();
        let short = vec![Value::Int(1)];
        assert_eq!(
            db.table_mut(id).insert(1, short),
            Err(Error::WrongWidth { want: 3, got: 1 })
        );
        let wrong = vec![Value::Text("no".into()), Value::Real(1.0), Value::Text("x".into())];
        assert_eq!(
            db.table_mut(id).insert(1, wrong),
            Err(Error::WrongKind { column: "a".into(), want: "integer", got: "text" })
        );
    }

    #[test]
    fn a_column_can_forbid_null() {
        let mut db = Db::new();
        let id = db
            .create_table_full(
                "t",
                vec![Column { name: "a".into(), kind: Kind::Int, null_ok: false }],
            )
            .unwrap();
        assert_eq!(
            db.table_mut(id).insert(1, vec![Value::Null]),
            Err(Error::NotNull("a".into()))
        );
        db.table_mut(id).insert(1, vec![Value::Int(1)]).unwrap();
        assert_eq!(
            db.table_mut(id).update(1, 0, Value::Null),
            Err(Error::NotNull("a".into()))
        );
    }

    #[test]
    fn update_changes_one_value_and_checks_it() {
        let (mut db, id) = kv();
        db.table_mut(id).insert(1, row(1)).unwrap();
        db.table_mut(id).update(1, 0, Value::Int(99)).unwrap();
        assert_eq!(db.table(id).get_at(1, 0), Some(&Value::Int(99)));
        assert_eq!(
            db.table_mut(id).update(1, 0, Value::Text("no".into())),
            Err(Error::WrongKind { column: "a".into(), want: "integer", got: "text" })
        );
        assert_eq!(db.table_mut(id).update(5, 0, Value::Int(1)), Err(Error::NoKey(5)));
        assert_eq!(db.table_mut(id).update(1, 9, Value::Int(1)), Err(Error::NoColumn(9)));
    }

    #[test]
    fn delete_takes_a_row_out_once() {
        let (mut db, id) = kv();
        db.table_mut(id).insert(1, row(1)).unwrap();
        assert!(db.table_mut(id).delete(1));
        assert!(!db.table_mut(id).delete(1));
        assert!(db.table(id).is_empty());
    }

    #[test]
    fn rows_come_back_in_key_order() {
        let (mut db, id) = kv();
        for k in [5, 1, 9, 3] {
            db.table_mut(id).insert(k, row(k)).unwrap();
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
        db.table_mut(a).insert(1, vec![Value::Int(10)]).unwrap();
        db.table_mut(b).insert(1, vec![Value::Int(20)]).unwrap();
        assert_eq!(db.table(a).get_at(1, 0), Some(&Value::Int(10)));
        assert_eq!(db.table(b).get_at(1, 0), Some(&Value::Int(20)));
        assert_eq!(db.create_table("a", &[]), Err(Error::TableExists("a".into())));
    }

    #[test]
    fn a_table_is_found_by_name_once_and_then_by_id() {
        let (mut db, id) = kv();
        db.table_mut(id).insert(1, row(1)).unwrap();
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
