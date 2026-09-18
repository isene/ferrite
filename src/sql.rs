//! Reading SQL: the words, then the shape.
//!
//! A hand-written scanner and a hand-written parser, because a generated
//! one would drag in a dependency and hide where the time goes. What
//! comes out is a [`Stmt`], which says what the query wants without
//! knowing anything about the tables. Turning that into something that
//! can run is [`crate::plan`].

use crate::{Error, Kind, Result, Value};

// ── Words ──────────────────────────────────────────────────────────────

/// How two things are compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmp { Eq, Ne, Lt, Le, Gt, Ge }

impl Cmp {
    /// True when `a <cmp> b`.
    pub fn holds(&self, ordering: std::cmp::Ordering) -> bool {
        use std::cmp::Ordering::*;
        match self {
            Cmp::Eq => ordering == Equal,
            Cmp::Ne => ordering != Equal,
            Cmp::Lt => ordering == Less,
            Cmp::Le => ordering != Greater,
            Cmp::Gt => ordering == Greater,
            Cmp::Ge => ordering != Less,
        }
    }

    /// The same comparison with the two sides swapped.
    pub fn flipped(&self) -> Cmp {
        match self {
            Cmp::Lt => Cmp::Gt,
            Cmp::Le => Cmp::Ge,
            Cmp::Gt => Cmp::Lt,
            Cmp::Ge => Cmp::Le,
            same => *same,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Word(String),
    Int(i64),
    Real(f64),
    Str(String),
    /// `?1` is 1, and a bare `?` takes the next number in order.
    Param(usize),
    Sym(char),
    Cmp(Cmp),
}

fn scan(sql: &str) -> Result<Vec<Tok>> {
    let b: Vec<char> = sql.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    let mut bare_params = 0usize;
    while i < b.len() {
        let c = b[i];
        if c.is_whitespace() { i += 1; continue; }
        // A comment runs to the end of the line.
        if c == '-' && b.get(i + 1) == Some(&'-') {
            while i < b.len() && b[i] != '\n' { i += 1; }
            continue;
        }
        if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == '_') { i += 1; }
            out.push(Tok::Word(b[start..i].iter().collect()));
            continue;
        }
        if c.is_ascii_digit() {
            let start = i;
            while i < b.len() && b[i].is_ascii_digit() { i += 1; }
            let real = i < b.len() && b[i] == '.';
            if real {
                i += 1;
                while i < b.len() && b[i].is_ascii_digit() { i += 1; }
            }
            let text: String = b[start..i].iter().collect();
            out.push(if real {
                Tok::Real(text.parse().map_err(|_| sql_err(&format!("{text} is not a number")))?)
            } else {
                Tok::Int(text.parse().map_err(|_| sql_err(&format!("{text} is too big")))?)
            });
            continue;
        }
        if c == '\'' {
            i += 1;
            let mut text = String::new();
            loop {
                match b.get(i) {
                    None => return Err(sql_err("a string was never closed")),
                    // Two quotes in a row mean one quote.
                    Some('\'') if b.get(i + 1) == Some(&'\'') => { text.push('\''); i += 2; }
                    Some('\'') => { i += 1; break; }
                    Some(ch) => { text.push(*ch); i += 1; }
                }
            }
            out.push(Tok::Str(text));
            continue;
        }
        if c == '?' {
            i += 1;
            let start = i;
            while i < b.len() && b[i].is_ascii_digit() { i += 1; }
            if i > start {
                let n: usize = b[start..i].iter().collect::<String>().parse().unwrap_or(0);
                if n == 0 { return Err(sql_err("parameters start at ?1")); }
                bare_params = bare_params.max(n);
                out.push(Tok::Param(n));
            } else {
                bare_params += 1;
                out.push(Tok::Param(bare_params));
            }
            continue;
        }
        let two: String = b[i..(i + 2).min(b.len())].iter().collect();
        let cmp = match two.as_str() {
            "<=" => Some(Cmp::Le),
            ">=" => Some(Cmp::Ge),
            "<>" => Some(Cmp::Ne),
            "!=" => Some(Cmp::Ne),
            _ => None,
        };
        if let Some(cmp) = cmp { out.push(Tok::Cmp(cmp)); i += 2; continue; }
        match c {
            '=' => out.push(Tok::Cmp(Cmp::Eq)),
            '<' => out.push(Tok::Cmp(Cmp::Lt)),
            '>' => out.push(Tok::Cmp(Cmp::Gt)),
            '(' | ')' | ',' | '*' | ';' | '.' | '-' | '+' | '/' => out.push(Tok::Sym(c)),
            _ => return Err(sql_err(&format!("I do not know what to do with {c}"))),
        }
        i += 1;
    }
    Ok(out)
}

fn sql_err(what: &str) -> Error { Error::Sql(what.to_string()) }

// ── Shape ──────────────────────────────────────────────────────────────

/// A column, and which table it came from when that has to be said.
#[derive(Debug, Clone, PartialEq)]
pub struct Name {
    pub table: Option<String>,
    pub column: String,
}

impl Name {
    pub fn bare(column: &str) -> Name { Name { table: None, column: column.to_string() } }
}

impl std::fmt::Display for Name {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.table {
            Some(t) => write!(f, "{t}.{}", self.column),
            None => write!(f, "{}", self.column),
        }
    }
}

/// The four things a query can work out over a lot of rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Func { Count, Sum, Min, Max }

/// What joins the two sides of an expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Cmp(Cmp),
    And,
    Or,
    Add,
    Sub,
    Mul,
    Div,
}

/// Anything that comes out as a value: a literal, a slot, a column, or
/// something worked out from those.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Lit(Value),
    /// `?1` is slot 0.
    Param(usize),
    Column(Name),
    Not(Box<Expr>),
    /// `x IS NULL`, or `x IS NOT NULL` when the flag is set.
    IsNull(Box<Expr>, bool),
    /// `x BETWEEN a AND b`, or `NOT BETWEEN` when the flag is set.
    Between { what: Box<Expr>, low: Box<Expr>, high: Box<Expr>, not: bool },
    Bin(Box<Expr>, Op, Box<Expr>),
    /// `COALESCE(a, b, ...)`: the first that is not null.
    Coalesce(Vec<Expr>),
    /// `COUNT(*)` has no argument; the others have one.
    Agg(Func, Option<Box<Expr>>),
}

impl Expr {
    /// True when an aggregate sits anywhere inside.
    pub fn has_aggregate(&self) -> bool {
        match self {
            Expr::Agg(..) => true,
            Expr::Lit(_) | Expr::Param(_) | Expr::Column(_) => false,
            Expr::Not(e) | Expr::IsNull(e, _) => e.has_aggregate(),
            Expr::Between { what, low, high, .. } => {
                what.has_aggregate() || low.has_aggregate() || high.has_aggregate()
            }
            Expr::Bin(a, _, b) => a.has_aggregate() || b.has_aggregate(),
            Expr::Coalesce(v) => v.iter().any(Expr::has_aggregate),
        }
    }

    /// The highest `?` slot used, plus one.
    pub fn params(&self) -> usize {
        match self {
            Expr::Param(n) => n + 1,
            Expr::Lit(_) | Expr::Column(_) => 0,
            Expr::Not(e) | Expr::IsNull(e, _) => e.params(),
            Expr::Between { what, low, high, .. } => what.params().max(low.params()).max(high.params()),
            Expr::Bin(a, _, b) => a.params().max(b.params()),
            Expr::Coalesce(v) => v.iter().map(Expr::params).max().unwrap_or(0),
            Expr::Agg(_, e) => e.as_ref().map_or(0, |e| e.params()),
        }
    }

    /// The parts of an AND chain, so `a AND b AND c` is three things.
    pub fn conjuncts(self) -> Vec<Expr> {
        match self {
            Expr::Bin(a, Op::And, b) => {
                let mut out = a.conjuncts();
                out.extend(b.conjuncts());
                out
            }
            other => vec![other],
        }
    }
}

/// Something to sort by, and which way.
#[derive(Debug, Clone, PartialEq)]
pub struct Order {
    pub expr: Expr,
    pub desc: bool,
}

/// A second table, joined on a condition.
#[derive(Debug, Clone, PartialEq)]
pub struct Join {
    pub table: String,
    pub alias: Option<String>,
    pub on: Expr,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Project {
    All,
    Exprs(Vec<Expr>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ColDef {
    pub name: String,
    pub kind: Kind,
    pub primary: bool,
    pub null_ok: bool,
    pub unique: bool,
    pub default: Option<Value>,
}

/// A foreign key: this table's column points at another table's key,
/// and the row here goes with the row there when the flag is set.
#[derive(Debug, Clone, PartialEq)]
pub struct ForeignKey {
    pub column: String,
    pub table: String,
    pub cascade: bool,
}

/// What to do when an INSERT would land on a row that is already there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnConflict { Fail, Ignore, Replace }

/// One statement, as written. Nothing here has been checked against a
/// real table yet.
#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    CreateTable {
        name: String,
        columns: Vec<ColDef>,
        /// A key spelt out at the end: `PRIMARY KEY(a, b)`.
        key_columns: Vec<String>,
        /// Uniqueness spelt out at the end: `UNIQUE(a, b)`.
        unique_sets: Vec<Vec<String>>,
        foreign_keys: Vec<ForeignKey>,
        if_missing: bool,
    },
    DropTable { name: String, if_there: bool },
    CreateIndex { name: String, table: String, columns: Vec<String>, unique: bool, if_missing: bool },
    DropIndex { name: String, if_there: bool },
    Insert { table: String, columns: Vec<String>, values: Vec<Expr>, on_conflict: OnConflict },
    Select {
        table: String,
        alias: Option<String>,
        join: Option<Join>,
        project: Project,
        filter: Option<Expr>,
        order: Vec<Order>,
        limit: Option<usize>,
    },
    Update { table: String, sets: Vec<(String, Expr)>, filter: Option<Expr> },
    Delete { table: String, filter: Option<Expr> },
    Begin,
    Commit,
    Rollback,
    /// A PRAGMA is read and let go. There is nothing here it could set.
    Nothing,
}

impl Stmt {
    /// How many `?` slots the statement wants.
    pub fn params(&self) -> usize {
        let of = |es: &[&Expr]| es.iter().map(|e| e.params()).max().unwrap_or(0);
        match self {
            Stmt::Insert { values, .. } => of(&values.iter().collect::<Vec<_>>()),
            Stmt::Select { join, project, filter, order, .. } => {
                let mut all: Vec<&Expr> = Vec::new();
                if let Some(j) = join { all.push(&j.on); }
                if let Project::Exprs(items) = project { all.extend(items.iter()); }
                if let Some(f) = filter { all.push(f); }
                all.extend(order.iter().map(|o| &o.expr));
                of(&all)
            }
            Stmt::Update { sets, filter, .. } => {
                let mut all: Vec<&Expr> = sets.iter().map(|(_, e)| e).collect();
                if let Some(f) = filter { all.push(f); }
                of(&all)
            }
            Stmt::Delete { filter, .. } => filter.as_ref().map_or(0, Expr::params),
            _ => 0,
        }
    }
}

// ── Parsing ────────────────────────────────────────────────────────────

struct Parser {
    toks: Vec<Tok>,
    at: usize,
}

/// Read one statement out of `sql`.
pub fn parse(sql: &str) -> Result<Stmt> {
    let mut p = Parser { toks: scan(sql)?, at: 0 };
    let stmt = p.statement()?;
    p.eat_sym(';');
    if p.at < p.toks.len() {
        return Err(sql_err("there is more after the end of the statement"));
    }
    Ok(stmt)
}

/// Cut a run of statements apart at the semicolons, minding the ones
/// inside strings and comments.
pub fn split(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_str = false;
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => { in_str = !in_str; cur.push(c); }
            ';' if !in_str => {
                if !cur.trim().is_empty() { out.push(cur.trim().to_string()); }
                cur.clear();
            }
            '-' if !in_str && chars.peek() == Some(&'-') => {
                for d in chars.by_ref() { if d == '\n' { break; } }
            }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() { out.push(cur.trim().to_string()); }
    out
}

/// Words that cannot be a table's short name, because they start the
/// next part of the statement.
const NOT_AN_ALIAS: &[&str] = &[
    "join", "inner", "left", "cross", "on", "where", "order", "limit", "group", "set", "values",
];

impl Parser {
    fn peek(&self) -> Option<&Tok> { self.toks.get(self.at) }

    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.at).cloned();
        if t.is_some() { self.at += 1; }
        t
    }

    fn peek_word(&self, want: &str) -> bool {
        matches!(self.peek(), Some(Tok::Word(w)) if w.eq_ignore_ascii_case(want))
    }

    /// True (and step over it) when the next word is this one, whatever
    /// its case.
    fn eat_word(&mut self, want: &str) -> bool {
        if self.peek_word(want) { self.at += 1; return true; }
        false
    }

    fn eat_sym(&mut self, want: char) -> bool {
        if self.peek() == Some(&Tok::Sym(want)) { self.at += 1; return true; }
        false
    }

    fn want_word(&mut self, want: &str) -> Result<()> {
        if self.eat_word(want) { return Ok(()); }
        Err(sql_err(&format!("I expected {want} here")))
    }

    fn want_sym(&mut self, want: char) -> Result<()> {
        if self.eat_sym(want) { return Ok(()); }
        Err(sql_err(&format!("I expected {want} here")))
    }

    /// A table or column name.
    fn name(&mut self) -> Result<String> {
        match self.next() {
            Some(Tok::Word(w)) => Ok(w),
            _ => Err(sql_err("I expected a name here")),
        }
    }

    /// A table name, and the short name it goes by in this query if one
    /// was given: `events e` or `events AS e`.
    fn table_and_alias(&mut self) -> Result<(String, Option<String>)> {
        let table = self.name()?;
        if self.eat_word("as") { return Ok((table, Some(self.name()?))); }
        match self.peek() {
            Some(Tok::Word(w)) if !NOT_AN_ALIAS.iter().any(|k| w.eq_ignore_ascii_case(k)) => {
                Ok((table, Some(self.name()?)))
            }
            _ => Ok((table, None)),
        }
    }

    fn names_in_parens(&mut self) -> Result<Vec<String>> {
        self.want_sym('(')?;
        let mut out = Vec::new();
        loop {
            out.push(self.name()?);
            self.eat_word("asc");
            self.eat_word("desc");
            if !self.eat_sym(',') { break; }
        }
        self.want_sym(')')?;
        Ok(out)
    }

    fn if_not_exists(&mut self) -> Result<bool> {
        if self.eat_word("if") {
            self.want_word("not")?;
            self.want_word("exists")?;
            return Ok(true);
        }
        Ok(false)
    }

    fn if_exists(&mut self) -> Result<bool> {
        if self.eat_word("if") {
            self.want_word("exists")?;
            return Ok(true);
        }
        Ok(false)
    }

    fn statement(&mut self) -> Result<Stmt> {
        if self.eat_word("create") { return self.create(); }
        if self.eat_word("drop") { return self.drop(); }
        if self.eat_word("insert") { return self.insert(); }
        if self.eat_word("select") { return self.select(); }
        if self.eat_word("update") { return self.update(); }
        if self.eat_word("delete") { return self.delete(); }
        if self.eat_word("pragma") {
            while let Some(t) = self.peek() {
                if *t == Tok::Sym(';') { break; }
                self.at += 1;
            }
            return Ok(Stmt::Nothing);
        }
        if self.eat_word("begin") {
            self.eat_word("transaction");
            return Ok(Stmt::Begin);
        }
        if self.eat_word("commit") || self.eat_word("end") {
            self.eat_word("transaction");
            return Ok(Stmt::Commit);
        }
        if self.eat_word("rollback") {
            self.eat_word("transaction");
            return Ok(Stmt::Rollback);
        }
        Err(sql_err("I do not know that kind of statement"))
    }

    fn create(&mut self) -> Result<Stmt> {
        let unique = self.eat_word("unique");
        if self.eat_word("index") { return self.create_index(unique); }
        if unique { return Err(sql_err("UNIQUE goes with INDEX")); }
        self.want_word("table")?;
        let if_missing = self.if_not_exists()?;
        let name = self.name()?;
        self.want_sym('(')?;
        let mut columns = Vec::new();
        let mut key_columns = Vec::new();
        let mut unique_sets = Vec::new();
        let mut foreign_keys = Vec::new();
        loop {
            if self.eat_word("primary") {
                self.want_word("key")?;
                key_columns = self.names_in_parens()?;
            } else if self.eat_word("unique") {
                unique_sets.push(self.names_in_parens()?);
            } else if self.eat_word("foreign") {
                self.want_word("key")?;
                let mut cols = self.names_in_parens()?;
                if cols.len() != 1 {
                    return Err(sql_err("a foreign key over several columns is not something I do"));
                }
                foreign_keys.push(self.references(cols.remove(0))?);
            } else {
                let (col, fk) = self.column_def()?;
                columns.push(col);
                foreign_keys.extend(fk);
            }
            if !self.eat_sym(',') { break; }
        }
        self.want_sym(')')?;
        Ok(Stmt::CreateTable { name, columns, key_columns, unique_sets, foreign_keys, if_missing })
    }

    /// `REFERENCES table(column) ON DELETE CASCADE`, after the word
    /// REFERENCES has been read or is next. The column it points at is
    /// that table's key whatever it is called, so the name is read and
    /// let go.
    fn references(&mut self, column: String) -> Result<ForeignKey> {
        self.want_word("references")?;
        let table = self.name()?;
        if self.eat_sym('(') {
            self.name()?;
            self.want_sym(')')?;
        }
        let mut cascade = false;
        while self.eat_word("on") {
            let which = self.name()?;
            let action = self.name()?;
            if action.eq_ignore_ascii_case("set") || action.eq_ignore_ascii_case("no") {
                self.name()?;
            }
            if which.eq_ignore_ascii_case("delete") && action.eq_ignore_ascii_case("cascade") {
                cascade = true;
            }
        }
        Ok(ForeignKey { column, table, cascade })
    }

    fn column_def(&mut self) -> Result<(ColDef, Option<ForeignKey>)> {
        let name = self.name()?;
        let kind = self.kind()?;
        let mut def = ColDef { name, kind, primary: false, null_ok: true, unique: false, default: None };
        let mut fk = None;
        loop {
            if self.eat_word("primary") {
                self.want_word("key")?;
                self.eat_word("asc");
                self.eat_word("desc");
                self.eat_word("autoincrement");
                def.primary = true;
            } else if self.eat_word("not") {
                self.want_word("null")?;
                def.null_ok = false;
            } else if self.eat_word("null") {
                def.null_ok = true;
            } else if self.eat_word("unique") {
                def.unique = true;
            } else if self.eat_word("default") {
                def.default = Some(match self.unary()? {
                    Expr::Lit(v) => v,
                    _ => return Err(sql_err("a DEFAULT has to be a plain value")),
                });
            } else if self.peek_word("references") {
                fk = Some(self.references(def.name.clone())?);
            } else {
                break;
            }
        }
        Ok((def, fk))
    }

    fn kind(&mut self) -> Result<Kind> {
        let word = self.name()?;
        let kind = match word.to_ascii_uppercase().as_str() {
            "INTEGER" | "INT" | "BIGINT" | "SMALLINT" | "BOOLEAN" | "BOOL" => Kind::Int,
            "REAL" | "FLOAT" | "DOUBLE" | "NUMERIC" => Kind::Real,
            "TEXT" | "VARCHAR" | "CHAR" | "STRING" | "CLOB" => Kind::Text,
            "BLOB" => Kind::Blob,
            _ => return Err(sql_err(&format!("I do not know the type {word}"))),
        };
        // A size in brackets, like VARCHAR(64), is read and let go.
        if self.eat_sym('(') {
            while let Some(t) = self.next() {
                if t == Tok::Sym(')') { break; }
            }
        }
        Ok(kind)
    }

    fn create_index(&mut self, unique: bool) -> Result<Stmt> {
        let if_missing = self.if_not_exists()?;
        let name = self.name()?;
        self.want_word("on")?;
        let table = self.name()?;
        let columns = self.names_in_parens()?;
        Ok(Stmt::CreateIndex { name, table, columns, unique, if_missing })
    }

    fn drop(&mut self) -> Result<Stmt> {
        if self.eat_word("index") {
            let if_there = self.if_exists()?;
            return Ok(Stmt::DropIndex { name: self.name()?, if_there });
        }
        self.want_word("table")?;
        let if_there = self.if_exists()?;
        Ok(Stmt::DropTable { name: self.name()?, if_there })
    }

    fn insert(&mut self) -> Result<Stmt> {
        let on_conflict = if self.eat_word("or") {
            if self.eat_word("ignore") {
                OnConflict::Ignore
            } else if self.eat_word("replace") {
                OnConflict::Replace
            } else {
                return Err(sql_err("after INSERT OR I know IGNORE and REPLACE"));
            }
        } else {
            OnConflict::Fail
        };
        self.want_word("into")?;
        let table = self.name()?;
        let mut columns = Vec::new();
        if self.eat_sym('(') {
            loop {
                columns.push(self.name()?);
                if !self.eat_sym(',') { break; }
            }
            self.want_sym(')')?;
        }
        self.want_word("values")?;
        self.want_sym('(')?;
        let mut values = Vec::new();
        loop {
            values.push(self.expr()?);
            if !self.eat_sym(',') { break; }
        }
        self.want_sym(')')?;
        Ok(Stmt::Insert { table, columns, values, on_conflict })
    }

    fn select(&mut self) -> Result<Stmt> {
        let project = if self.eat_sym('*') {
            Project::All
        } else {
            let mut items = Vec::new();
            loop {
                items.push(self.expr()?);
                // A name for the column, which nothing here uses.
                if self.eat_word("as") { self.name()?; }
                if !self.eat_sym(',') { break; }
            }
            Project::Exprs(items)
        };
        self.want_word("from")?;
        let (table, alias) = self.table_and_alias()?;
        let join = self.join_clause()?;
        let filter = self.where_clause()?;
        let order = self.order_clause()?;
        let limit = if self.eat_word("limit") {
            match self.next() {
                Some(Tok::Int(n)) if n >= 0 => Some(n as usize),
                _ => return Err(sql_err("LIMIT wants a whole number")),
            }
        } else {
            None
        };
        Ok(Stmt::Select { table, alias, join, project, filter, order, limit })
    }

    fn join_clause(&mut self) -> Result<Option<Join>> {
        self.eat_word("inner");
        if !self.eat_word("join") { return Ok(None); }
        let (table, alias) = self.table_and_alias()?;
        self.want_word("on")?;
        let on = self.expr()?;
        Ok(Some(Join { table, alias, on }))
    }

    fn order_clause(&mut self) -> Result<Vec<Order>> {
        if !self.eat_word("order") { return Ok(Vec::new()); }
        self.want_word("by")?;
        let mut out = Vec::new();
        loop {
            let expr = self.expr()?;
            let desc = if self.eat_word("desc") { true } else { self.eat_word("asc"); false };
            out.push(Order { expr, desc });
            if !self.eat_sym(',') { break; }
        }
        Ok(out)
    }

    fn update(&mut self) -> Result<Stmt> {
        let table = self.name()?;
        self.want_word("set")?;
        let mut sets = Vec::new();
        loop {
            let col = self.name()?;
            if self.peek() != Some(&Tok::Cmp(Cmp::Eq)) {
                return Err(sql_err("I expected = here"));
            }
            self.at += 1;
            sets.push((col, self.expr()?));
            if !self.eat_sym(',') { break; }
        }
        let filter = self.where_clause()?;
        Ok(Stmt::Update { table, sets, filter })
    }

    fn delete(&mut self) -> Result<Stmt> {
        self.want_word("from")?;
        let table = self.name()?;
        let filter = self.where_clause()?;
        Ok(Stmt::Delete { table, filter })
    }

    fn where_clause(&mut self) -> Result<Option<Expr>> {
        if !self.eat_word("where") { return Ok(None); }
        Ok(Some(self.expr()?))
    }

    // ── Expressions, loosest binding first ─────────────────────────────

    fn expr(&mut self) -> Result<Expr> { self.or() }

    fn or(&mut self) -> Result<Expr> {
        let mut left = self.and()?;
        while self.eat_word("or") {
            let right = self.and()?;
            left = Expr::Bin(Box::new(left), Op::Or, Box::new(right));
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Expr> {
        let mut left = self.not()?;
        while self.eat_word("and") {
            let right = self.not()?;
            left = Expr::Bin(Box::new(left), Op::And, Box::new(right));
        }
        Ok(left)
    }

    fn not(&mut self) -> Result<Expr> {
        if self.eat_word("not") {
            return Ok(Expr::Not(Box::new(self.not()?)));
        }
        self.comparison()
    }

    fn comparison(&mut self) -> Result<Expr> {
        let left = self.additive()?;
        if self.eat_word("is") {
            let not = self.eat_word("not");
            self.want_word("null")?;
            return Ok(Expr::IsNull(Box::new(left), not));
        }
        let save = self.at;
        let not = self.eat_word("not");
        if self.eat_word("between") {
            let low = self.additive()?;
            self.want_word("and")?;
            let high = self.additive()?;
            return Ok(Expr::Between { what: Box::new(left), low: Box::new(low), high: Box::new(high), not });
        }
        self.at = save;
        if let Some(Tok::Cmp(c)) = self.peek().cloned() {
            self.at += 1;
            let right = self.additive()?;
            return Ok(Expr::Bin(Box::new(left), Op::Cmp(c), Box::new(right)));
        }
        Ok(left)
    }

    fn additive(&mut self) -> Result<Expr> {
        let mut left = self.multiplicative()?;
        loop {
            let op = if self.eat_sym('+') {
                Op::Add
            } else if self.eat_sym('-') {
                Op::Sub
            } else {
                break;
            };
            let right = self.multiplicative()?;
            left = Expr::Bin(Box::new(left), op, Box::new(right));
        }
        Ok(left)
    }

    fn multiplicative(&mut self) -> Result<Expr> {
        let mut left = self.unary()?;
        loop {
            let op = if self.eat_sym('*') {
                Op::Mul
            } else if self.eat_sym('/') {
                Op::Div
            } else {
                break;
            };
            let right = self.unary()?;
            left = Expr::Bin(Box::new(left), op, Box::new(right));
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<Expr> {
        if self.eat_sym('-') {
            // A minus in front of a number belongs to the number.
            return Ok(match self.unary()? {
                Expr::Lit(Value::Int(i)) => Expr::Lit(Value::Int(-i)),
                Expr::Lit(Value::Real(r)) => Expr::Lit(Value::Real(-r)),
                other => Expr::Bin(Box::new(Expr::Lit(Value::Int(0))), Op::Sub, Box::new(other)),
            });
        }
        if self.eat_sym('+') {
            return self.unary();
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Expr> {
        if self.eat_sym('(') {
            let inner = self.expr()?;
            self.want_sym(')')?;
            return Ok(inner);
        }
        match self.next() {
            Some(Tok::Int(i)) => Ok(Expr::Lit(Value::Int(i))),
            Some(Tok::Real(r)) => Ok(Expr::Lit(Value::Real(r))),
            Some(Tok::Str(s)) => Ok(Expr::Lit(Value::Text(s))),
            Some(Tok::Param(n)) => Ok(Expr::Param(n - 1)),
            Some(Tok::Word(w)) => {
                let up = w.to_ascii_uppercase();
                if up == "NULL" { return Ok(Expr::Lit(Value::Null)); }
                if up == "TRUE" { return Ok(Expr::Lit(Value::Int(1))); }
                if up == "FALSE" { return Ok(Expr::Lit(Value::Int(0))); }
                if self.eat_sym('(') { return self.call(&up); }
                if self.eat_sym('.') {
                    let column = self.name()?;
                    return Ok(Expr::Column(Name { table: Some(w), column }));
                }
                Ok(Expr::Column(Name { table: None, column: w }))
            }
            _ => Err(sql_err("I expected a value here")),
        }
    }

    /// The name has been read and the bracket is open.
    fn call(&mut self, name: &str) -> Result<Expr> {
        let func = match name {
            "COUNT" => {
                if self.eat_sym('*') {
                    self.want_sym(')')?;
                    return Ok(Expr::Agg(Func::Count, None));
                }
                Some(Func::Count)
            }
            "SUM" => Some(Func::Sum),
            "MIN" => Some(Func::Min),
            "MAX" => Some(Func::Max),
            _ => None,
        };
        if let Some(func) = func {
            let arg = self.expr()?;
            self.want_sym(')')?;
            return Ok(Expr::Agg(func, Some(Box::new(arg))));
        }
        if name == "COALESCE" || name == "IFNULL" {
            let mut args = Vec::new();
            loop {
                args.push(self.expr()?);
                if !self.eat_sym(',') { break; }
            }
            self.want_sym(')')?;
            return Ok(Expr::Coalesce(args));
        }
        Err(sql_err(&format!("I do not know a function called {name}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(n: &str) -> Expr { Expr::Column(Name::bare(n)) }
    fn int(i: i64) -> Expr { Expr::Lit(Value::Int(i)) }
    fn bin(a: Expr, op: Op, b: Expr) -> Expr { Expr::Bin(Box::new(a), op, Box::new(b)) }

    fn filter_of(sql: &str) -> Expr {
        match parse(sql).unwrap() {
            Stmt::Select { filter, .. } => filter.unwrap(),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_create_says_which_column_is_the_key() {
        let s = parse("CREATE TABLE kv (id INTEGER PRIMARY KEY, a INT NOT NULL, c TEXT)").unwrap();
        let Stmt::CreateTable { name, columns, if_missing, .. } = s else { panic!() };
        assert_eq!(name, "kv");
        assert!(!if_missing);
        assert_eq!(columns.len(), 3);
        assert!(columns[0].primary);
        assert_eq!(columns[1].kind, Kind::Int);
        assert!(!columns[1].null_ok);
        assert!(columns[2].null_ok);
    }

    #[test]
    fn a_create_can_spell_out_its_key_defaults_and_foreign_keys() {
        let s = parse(
            "CREATE TABLE ev (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                cal INTEGER NOT NULL,
                color INTEGER DEFAULT 39,
                status TEXT DEFAULT 'confirmed',
                note VARCHAR(64) UNIQUE,
                who INTEGER REFERENCES people(id),
                FOREIGN KEY(cal) REFERENCES calendars(id) ON DELETE CASCADE
            )",
        )
        .unwrap();
        let Stmt::CreateTable { columns, foreign_keys, .. } = s else { panic!() };
        assert_eq!(columns[2].default, Some(Value::Int(39)));
        assert_eq!(columns[3].default, Some(Value::Text("confirmed".into())));
        assert!(columns[4].unique);
        assert_eq!(
            foreign_keys,
            vec![
                ForeignKey { column: "who".into(), table: "people".into(), cascade: false },
                ForeignKey { column: "cal".into(), table: "calendars".into(), cascade: true },
            ]
        );

        let s = parse("CREATE TABLE w (date TEXT NOT NULL, hour INTEGER, PRIMARY KEY(date, hour), UNIQUE(hour))").unwrap();
        let Stmt::CreateTable { key_columns, unique_sets, .. } = s else { panic!() };
        assert_eq!(key_columns, vec!["date", "hour"]);
        assert_eq!(unique_sets, vec![vec!["hour".to_string()]]);
    }

    #[test]
    fn keywords_do_not_care_about_case() {
        let a = parse("select * from kv").unwrap();
        let b = parse("SELECT * FROM kv").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn an_insert_keeps_its_values_in_order_and_knows_what_to_do_on_a_clash() {
        let s = parse("INSERT INTO kv (id, a, c) VALUES (?1, 7, 'hi')").unwrap();
        let Stmt::Insert { table, columns, values, on_conflict } = s else { panic!() };
        assert_eq!(table, "kv");
        assert_eq!(columns, vec!["id", "a", "c"]);
        assert_eq!(values, vec![Expr::Param(0), int(7), Expr::Lit(Value::Text("hi".into()))]);
        assert_eq!(on_conflict, OnConflict::Fail);
        let Stmt::Insert { on_conflict, .. } = parse("INSERT OR IGNORE INTO kv (id) VALUES (1)").unwrap() else { panic!() };
        assert_eq!(on_conflict, OnConflict::Ignore);
        let Stmt::Insert { on_conflict, .. } = parse("INSERT OR REPLACE INTO kv (id) VALUES (1)").unwrap() else { panic!() };
        assert_eq!(on_conflict, OnConflict::Replace);
    }

    #[test]
    fn a_bare_question_mark_counts_itself() {
        let s = parse("SELECT a FROM kv WHERE id = ? AND a = ?").unwrap();
        let Stmt::Select { filter, .. } = &s else { panic!() };
        assert_eq!(
            filter.clone().unwrap(),
            bin(bin(col("id"), Op::Cmp(Cmp::Eq), Expr::Param(0)), Op::And, bin(col("a"), Op::Cmp(Cmp::Eq), Expr::Param(1)))
        );
        assert_eq!(s.params(), 2);
    }

    #[test]
    fn every_comparison_parses() {
        for (text, want) in [
            ("=", Cmp::Eq), ("<>", Cmp::Ne), ("!=", Cmp::Ne),
            ("<", Cmp::Lt), ("<=", Cmp::Le), (">", Cmp::Gt), (">=", Cmp::Ge),
        ] {
            let f = filter_of(&format!("SELECT a FROM kv WHERE id {text} 1"));
            assert_eq!(f, bin(col("id"), Op::Cmp(want), int(1)), "{text}");
        }
    }

    #[test]
    fn and_binds_tighter_than_or_and_brackets_win() {
        let f = filter_of("SELECT a FROM kv WHERE a = 1 OR b = 2 AND c = 3");
        let Expr::Bin(_, Op::Or, right) = f else { panic!("{f:?}") };
        assert!(matches!(*right, Expr::Bin(_, Op::And, _)));
        let f = filter_of("SELECT a FROM kv WHERE (a = 1 OR b = 2) AND c = 3");
        let Expr::Bin(left, Op::And, _) = f else { panic!("{f:?}") };
        assert!(matches!(*left, Expr::Bin(_, Op::Or, _)));
        let parts = filter_of("SELECT a FROM kv WHERE a = 1 AND b = 2 AND c = 3").conjuncts();
        assert_eq!(parts.len(), 3);
    }

    #[test]
    fn is_null_between_and_not_all_parse() {
        assert_eq!(filter_of("SELECT a FROM kv WHERE a IS NULL"), Expr::IsNull(Box::new(col("a")), false));
        assert_eq!(filter_of("SELECT a FROM kv WHERE a IS NOT NULL"), Expr::IsNull(Box::new(col("a")), true));
        assert_eq!(
            filter_of("SELECT a FROM kv WHERE a BETWEEN 1 AND 5"),
            Expr::Between { what: Box::new(col("a")), low: Box::new(int(1)), high: Box::new(int(5)), not: false }
        );
        assert_eq!(
            filter_of("SELECT a FROM kv WHERE a NOT BETWEEN ?1 AND ?2"),
            Expr::Between { what: Box::new(col("a")), low: Box::new(Expr::Param(0)), high: Box::new(Expr::Param(1)), not: true }
        );
        assert_eq!(filter_of("SELECT a FROM kv WHERE NOT a = 1"), Expr::Not(Box::new(bin(col("a"), Op::Cmp(Cmp::Eq), int(1)))));
    }

    #[test]
    fn arithmetic_has_the_usual_order() {
        let Stmt::Update { sets, .. } = parse("UPDATE kv SET a = 1 - a, b = 2 + 3 * 4").unwrap() else { panic!() };
        assert_eq!(sets[0].1, bin(int(1), Op::Sub, col("a")));
        assert_eq!(sets[1].1, bin(int(2), Op::Add, bin(int(3), Op::Mul, int(4))));
    }

    #[test]
    fn a_number_can_be_negative_and_a_minus_still_starts_a_comment() {
        assert_eq!(filter_of("SELECT a FROM kv WHERE a < -7"), bin(col("a"), Op::Cmp(Cmp::Lt), int(-7)));
        assert_eq!(filter_of("SELECT a FROM kv WHERE b >= -1.5"), bin(col("b"), Op::Cmp(Cmp::Ge), Expr::Lit(Value::Real(-1.5))));
        assert_eq!(filter_of("SELECT a FROM kv WHERE a = +3"), bin(col("a"), Op::Cmp(Cmp::Eq), int(3)));
        assert_eq!(filter_of("SELECT a FROM kv -- a < -7\n WHERE id = 1"), bin(col("id"), Op::Cmp(Cmp::Eq), int(1)));
    }

    #[test]
    fn a_string_can_hold_a_quote() {
        assert_eq!(filter_of("SELECT a FROM kv WHERE c = 'it''s'"), bin(col("c"), Op::Cmp(Cmp::Eq), Expr::Lit(Value::Text("it's".into()))));
    }

    #[test]
    fn a_select_list_takes_a_star_columns_expressions_and_aggregates() {
        let Stmt::Select { project, .. } = parse("SELECT * FROM kv").unwrap() else { panic!() };
        assert_eq!(project, Project::All);
        let Stmt::Select { project, .. } = parse("SELECT a, c FROM kv").unwrap() else { panic!() };
        assert_eq!(project, Project::Exprs(vec![col("a"), col("c")]));
        let Stmt::Select { project, .. } = parse("SELECT COUNT(*) > 0, COALESCE(a, id) AS x FROM kv").unwrap() else { panic!() };
        let Project::Exprs(items) = project else { panic!() };
        assert_eq!(items[0], bin(Expr::Agg(Func::Count, None), Op::Cmp(Cmp::Gt), int(0)));
        assert_eq!(items[1], Expr::Coalesce(vec![col("a"), col("id")]));
        assert!(items[0].has_aggregate());
        assert!(!items[1].has_aggregate());
        for (text, func) in [("SUM", Func::Sum), ("MIN", Func::Min), ("MAX", Func::Max), ("COUNT", Func::Count)] {
            let Stmt::Select { project, .. } = parse(&format!("SELECT {text}(a) FROM kv")).unwrap() else { panic!() };
            assert_eq!(project, Project::Exprs(vec![Expr::Agg(func, Some(Box::new(col("a"))))]));
        }
        assert!(parse("SELECT SUM(*) FROM kv").is_err());
        // A column called count is still a column.
        let Stmt::Select { project, .. } = parse("SELECT count FROM kv").unwrap() else { panic!() };
        assert_eq!(project, Project::Exprs(vec![col("count")]));
    }

    #[test]
    fn a_name_can_say_its_table_and_a_table_can_have_a_short_name() {
        let s = parse("SELECT e.title, c.name FROM events e JOIN calendars AS c ON c.id = e.calendar_id WHERE e.id = 1").unwrap();
        let Stmt::Select { alias, join, project, .. } = s else { panic!() };
        assert_eq!(alias.as_deref(), Some("e"));
        let join = join.unwrap();
        assert_eq!(join.table, "calendars");
        assert_eq!(join.alias.as_deref(), Some("c"));
        assert_eq!(
            project,
            Project::Exprs(vec![
                Expr::Column(Name { table: Some("e".into()), column: "title".into() }),
                Expr::Column(Name { table: Some("c".into()), column: "name".into() }),
            ])
        );
        let Stmt::Select { alias, .. } = parse("SELECT a FROM kv WHERE a = 1").unwrap() else { panic!() };
        assert_eq!(alias, None);
        assert!(parse("SELECT x FROM a INNER JOIN b ON a.k = b.id").is_ok());
    }

    #[test]
    fn order_by_takes_a_direction_and_a_list() {
        let s = parse("SELECT a FROM kv ORDER BY a DESC, b, c ASC LIMIT 5").unwrap();
        let Stmt::Select { order, limit, .. } = s else { panic!() };
        assert_eq!(order.len(), 3);
        assert!(order[0].desc);
        assert!(!order[1].desc);
        assert!(!order[2].desc);
        assert_eq!(limit, Some(5));
    }

    #[test]
    fn an_index_can_cover_several_columns_and_be_unique() {
        let s = parse("CREATE INDEX kv_a ON kv (a)").unwrap();
        assert_eq!(
            s,
            Stmt::CreateIndex { name: "kv_a".into(), table: "kv".into(), columns: vec!["a".into()], unique: false, if_missing: false }
        );
        let s = parse("CREATE UNIQUE INDEX IF NOT EXISTS kv_ab ON kv (a DESC, b)").unwrap();
        let Stmt::CreateIndex { columns, unique, if_missing, .. } = s else { panic!() };
        assert_eq!(columns, vec!["a", "b"]);
        assert!(unique && if_missing);
        assert_eq!(parse("DROP INDEX kv_a").unwrap(), Stmt::DropIndex { name: "kv_a".into(), if_there: false });
        assert_eq!(parse("DROP INDEX IF EXISTS kv_a").unwrap(), Stmt::DropIndex { name: "kv_a".into(), if_there: true });
        assert_eq!(parse("DROP TABLE IF EXISTS kv").unwrap(), Stmt::DropTable { name: "kv".into(), if_there: true });
    }

    #[test]
    fn an_update_takes_several_columns() {
        let s = parse("UPDATE kv SET a = ?1, c = 'x' WHERE id = ?2").unwrap();
        assert_eq!(s.params(), 2);
        let Stmt::Update { sets, filter, .. } = s else { panic!() };
        assert_eq!(sets.len(), 2);
        assert_eq!(sets[0].1, Expr::Param(0));
        assert_eq!(filter.unwrap(), bin(col("id"), Op::Cmp(Cmp::Eq), Expr::Param(1)));
    }

    #[test]
    fn transactions_and_pragmas_are_words_of_their_own() {
        assert_eq!(parse("BEGIN").unwrap(), Stmt::Begin);
        assert_eq!(parse("BEGIN TRANSACTION").unwrap(), Stmt::Begin);
        assert_eq!(parse("COMMIT").unwrap(), Stmt::Commit);
        assert_eq!(parse("END").unwrap(), Stmt::Commit);
        assert_eq!(parse("ROLLBACK").unwrap(), Stmt::Rollback);
        assert_eq!(parse("PRAGMA foreign_keys = ON").unwrap(), Stmt::Nothing);
        assert_eq!(parse("PRAGMA journal_mode(WAL);").unwrap(), Stmt::Nothing);
    }

    #[test]
    fn a_batch_is_cut_at_semicolons_but_not_inside_strings() {
        let parts = split("CREATE TABLE a (x TEXT); INSERT INTO a VALUES ('one; two'); -- c; d\n SELECT * FROM a;");
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[1], "INSERT INTO a VALUES ('one; two')");
    }

    #[test]
    fn nonsense_comes_back_as_an_error_not_a_panic() {
        for bad in [
            "SELECT", "SELECT a FROM", "INSERT INTO kv VALUES", "WOBBLE kv",
            "SELECT a FROM kv WHERE", "SELECT a FROM kv WHERE id =", "CREATE TABLE kv (a WOBBLE)",
            "SELECT a FROM kv WHERE c = 'never closed", "SELECT a FROM kv; SELECT a FROM kv",
            "SELECT a FROM kv LIMIT x", "SELECT a FROM kv WHERE id = ?0", "SELECT a FROM kv WHERE (a = 1",
            "SELECT NOPE(a) FROM kv", "INSERT OR WOBBLE INTO kv (a) VALUES (1)",
            "CREATE TABLE kv (a INTEGER DEFAULT b)",
        ] {
            assert!(parse(bad).is_err(), "{bad} should not parse");
        }
    }

    #[test]
    fn a_trailing_semicolon_is_fine() {
        assert!(parse("SELECT a FROM kv;").is_ok());
    }
}
