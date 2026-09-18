//! Reading SQL: the words, then the shape.
//!
//! A hand-written scanner and a hand-written parser, because a generated
//! one would drag in a dependency and hide where the time goes. What
//! comes out is a [`Stmt`], which says what the query wants without
//! knowing anything about the tables. Turning that into something that
//! can run is [`crate::plan`].

use crate::{Error, Kind, Result, Value};

// ── Words ──────────────────────────────────────────────────────────────

/// How two things are compared in a WHERE.
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
            '(' | ')' | ',' | '*' | ';' | '.' | '-' | '+' => out.push(Tok::Sym(c)),
            _ => return Err(sql_err(&format!("I do not know what to do with {c}"))),
        }
        i += 1;
    }
    Ok(out)
}

fn sql_err(what: &str) -> Error { Error::Sql(what.to_string()) }

// ── Shape ──────────────────────────────────────────────────────────────

/// A value written into the query, or a slot to fill in when it runs.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Lit(Value),
    /// `?1` is slot 0.
    Param(usize),
}

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

#[derive(Debug, Clone, PartialEq)]
pub struct Cond {
    pub column: Name,
    pub cmp: Cmp,
    pub value: Expr,
}

/// The four things a query can work out over a lot of rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Func { Count, Sum, Min, Max }

/// One of them, and what it is over. `COUNT(*)` has nothing.
#[derive(Debug, Clone, PartialEq)]
pub struct Agg {
    pub func: Func,
    pub arg: Option<Name>,
}

/// A column to sort by, and which way.
#[derive(Debug, Clone, PartialEq)]
pub struct Order {
    pub name: Name,
    pub desc: bool,
}

/// A second table, joined on one column being equal to another.
#[derive(Debug, Clone, PartialEq)]
pub struct Join {
    pub table: String,
    pub left: Name,
    pub right: Name,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Project {
    All,
    Columns(Vec<Name>),
    /// A select list that is all aggregates, which gives one row back.
    Aggs(Vec<Agg>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ColDef {
    pub name: String,
    pub kind: Kind,
    pub primary: bool,
    pub null_ok: bool,
}

/// One statement, as written. Nothing here has been checked against a
/// real table yet.
#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    CreateTable { name: String, columns: Vec<ColDef>, if_missing: bool },
    CreateIndex { name: String, table: String, column: String, if_missing: bool },
    DropIndex { name: String, if_there: bool },
    Insert { table: String, columns: Vec<String>, values: Vec<Expr> },
    Select {
        table: String,
        join: Option<Join>,
        project: Project,
        filter: Vec<Cond>,
        order: Vec<Order>,
        limit: Option<usize>,
    },
    Update { table: String, sets: Vec<(String, Expr)>, filter: Vec<Cond> },
    Delete { table: String, filter: Vec<Cond> },
    Begin,
    Commit,
    Rollback,
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

impl Parser {
    fn peek(&self) -> Option<&Tok> { self.toks.get(self.at) }

    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.at).cloned();
        if t.is_some() { self.at += 1; }
        t
    }

    /// True (and step over it) when the next word is this one, whatever
    /// its case.
    fn eat_word(&mut self, want: &str) -> bool {
        if let Some(Tok::Word(w)) = self.peek() {
            if w.eq_ignore_ascii_case(want) { self.at += 1; return true; }
        }
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

    /// A column name, which may say its table first.
    fn qualified(&mut self) -> Result<Name> {
        let first = self.name()?;
        if self.eat_sym('.') {
            let column = self.name()?;
            return Ok(Name { table: Some(first), column });
        }
        Ok(Name { table: None, column: first })
    }

    fn statement(&mut self) -> Result<Stmt> {
        if self.eat_word("create") { return self.create(); }
        if self.eat_word("drop") { return self.drop_index(); }
        if self.eat_word("insert") { return self.insert(); }
        if self.eat_word("select") { return self.select(); }
        if self.eat_word("update") { return self.update(); }
        if self.eat_word("delete") { return self.delete(); }
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
        if self.eat_word("unique") {
            // A unique index is an ordinary one here. Refusing a
            // repeated value is phase 4 work that nothing asks for yet,
            // and pretending would be worse than saying so.
            self.want_word("index")?;
            return self.create_index();
        }
        if self.eat_word("index") { return self.create_index(); }
        self.want_word("table")?;
        let if_missing = if self.eat_word("if") {
            self.want_word("not")?;
            self.want_word("exists")?;
            true
        } else {
            false
        };
        let name = self.name()?;
        self.want_sym('(')?;
        let mut columns = Vec::new();
        loop {
            let col = self.name()?;
            let kind = self.kind()?;
            let mut primary = false;
            let mut null_ok = true;
            loop {
                if self.eat_word("primary") {
                    self.want_word("key")?;
                    primary = true;
                    null_ok = false;
                } else if self.eat_word("not") {
                    self.want_word("null")?;
                    null_ok = false;
                } else if self.eat_word("null") {
                    null_ok = true;
                } else {
                    break;
                }
            }
            columns.push(ColDef { name: col, kind, primary, null_ok });
            if !self.eat_sym(',') { break; }
        }
        self.want_sym(')')?;
        Ok(Stmt::CreateTable { name, columns, if_missing })
    }

    fn create_index(&mut self) -> Result<Stmt> {
        let if_missing = if self.eat_word("if") {
            self.want_word("not")?;
            self.want_word("exists")?;
            true
        } else {
            false
        };
        let name = self.name()?;
        self.want_word("on")?;
        let table = self.name()?;
        self.want_sym('(')?;
        let column = self.name()?;
        self.eat_word("asc");
        self.eat_word("desc");
        self.want_sym(')')?;
        Ok(Stmt::CreateIndex { name, table, column, if_missing })
    }

    fn drop_index(&mut self) -> Result<Stmt> {
        self.want_word("index")?;
        let if_there = if self.eat_word("if") { self.want_word("exists")?; true } else { false };
        let name = self.name()?;
        Ok(Stmt::DropIndex { name, if_there })
    }

    fn kind(&mut self) -> Result<Kind> {
        let word = self.name()?;
        let k = word.to_ascii_uppercase();
        Ok(match k.as_str() {
            "INTEGER" | "INT" | "BIGINT" => Kind::Int,
            "REAL" | "FLOAT" | "DOUBLE" => Kind::Real,
            "TEXT" | "VARCHAR" | "CHAR" => Kind::Text,
            "BLOB" => Kind::Blob,
            _ => return Err(sql_err(&format!("I do not know the type {word}"))),
        })
    }

    fn insert(&mut self) -> Result<Stmt> {
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
        Ok(Stmt::Insert { table, columns, values })
    }

    fn select(&mut self) -> Result<Stmt> {
        let project = if self.eat_sym('*') {
            Project::All
        } else if let Some(aggs) = self.aggregates()? {
            Project::Aggs(aggs)
        } else {
            let mut cols = Vec::new();
            loop {
                cols.push(self.qualified()?);
                if !self.eat_sym(',') { break; }
            }
            Project::Columns(cols)
        };
        self.want_word("from")?;
        let table = self.name()?;
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
        Ok(Stmt::Select { table, join, project, filter, order, limit })
    }

    /// A select list made only of COUNT, SUM, MIN and MAX. Anything else
    /// leaves the parser where it was.
    fn aggregates(&mut self) -> Result<Option<Vec<Agg>>> {
        let save = self.at;
        let mut out = Vec::new();
        loop {
            let Some(a) = self.one_aggregate()? else { self.at = save; return Ok(None) };
            out.push(a);
            if !self.eat_sym(',') { break; }
        }
        // Only a list that is entirely aggregates counts.
        if let Some(Tok::Word(w)) = self.peek() {
            if !w.eq_ignore_ascii_case("from") { self.at = save; return Ok(None); }
        }
        Ok(Some(out))
    }

    fn one_aggregate(&mut self) -> Result<Option<Agg>> {
        let save = self.at;
        let Some(Tok::Word(w)) = self.peek().cloned() else { return Ok(None) };
        let func = match w.to_ascii_uppercase().as_str() {
            "COUNT" => Func::Count,
            "SUM" => Func::Sum,
            "MIN" => Func::Min,
            "MAX" => Func::Max,
            _ => return Ok(None),
        };
        self.at += 1;
        if !self.eat_sym('(') { self.at = save; return Ok(None); }
        let arg = if self.eat_sym('*') { None } else { Some(self.qualified()?) };
        if !self.eat_sym(')') { return Err(sql_err("I expected ) after the column")); }
        if func != Func::Count && arg.is_none() {
            return Err(sql_err("only COUNT can take a star"));
        }
        Ok(Some(Agg { func, arg }))
    }

    fn join_clause(&mut self) -> Result<Option<Join>> {
        self.eat_word("inner");
        if !self.eat_word("join") { return Ok(None); }
        let table = self.name()?;
        self.want_word("on")?;
        let left = self.qualified()?;
        if self.peek() != Some(&Tok::Cmp(Cmp::Eq)) {
            return Err(sql_err("a join is on one column being equal to another"));
        }
        self.at += 1;
        let right = self.qualified()?;
        Ok(Some(Join { table, left, right }))
    }

    fn order_clause(&mut self) -> Result<Vec<Order>> {
        if !self.eat_word("order") { return Ok(Vec::new()); }
        self.want_word("by")?;
        let mut out = Vec::new();
        loop {
            let name = self.qualified()?;
            let desc = if self.eat_word("desc") { true } else { self.eat_word("asc"); false };
            out.push(Order { name, desc });
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

    /// Conditions joined by AND. OR waits for phase 4.
    fn where_clause(&mut self) -> Result<Vec<Cond>> {
        if !self.eat_word("where") { return Ok(Vec::new()); }
        let mut out = Vec::new();
        loop {
            let column = self.qualified()?;
            let cmp = match self.next() {
                Some(Tok::Cmp(c)) => c,
                _ => return Err(sql_err("I expected a comparison here")),
            };
            let value = self.expr()?;
            out.push(Cond { column, cmp, value });
            if !self.eat_word("and") { break; }
        }
        Ok(out)
    }

    fn expr(&mut self) -> Result<Expr> {
        // A sign in front of a number belongs to the number. Two minus
        // signs never reach here: the scanner reads them as a comment.
        let mut neg = false;
        loop {
            if self.eat_sym('-') { neg = !neg; continue; }
            if self.eat_sym('+') { continue; }
            break;
        }
        if neg {
            return match self.next() {
                Some(Tok::Int(i)) => Ok(Expr::Lit(Value::Int(-i))),
                Some(Tok::Real(r)) => Ok(Expr::Lit(Value::Real(-r))),
                _ => Err(sql_err("a minus sign has to be in front of a number")),
            };
        }
        match self.next() {
            Some(Tok::Int(i)) => Ok(Expr::Lit(Value::Int(i))),
            Some(Tok::Real(r)) => Ok(Expr::Lit(Value::Real(r))),
            Some(Tok::Str(s)) => Ok(Expr::Lit(Value::Text(s))),
            Some(Tok::Param(n)) => Ok(Expr::Param(n - 1)),
            Some(Tok::Word(w)) if w.eq_ignore_ascii_case("null") => Ok(Expr::Lit(Value::Null)),
            _ => Err(sql_err("I expected a value here")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_create_says_which_column_is_the_key() {
        let s = parse("CREATE TABLE kv (id INTEGER PRIMARY KEY, a INT NOT NULL, c TEXT)").unwrap();
        let Stmt::CreateTable { name, columns, if_missing } = s else { panic!() };
        assert_eq!(name, "kv");
        assert!(!if_missing);
        assert_eq!(columns.len(), 3);
        assert!(columns[0].primary);
        assert_eq!(columns[1].kind, Kind::Int);
        assert!(!columns[1].null_ok);
        assert!(columns[2].null_ok);
    }

    #[test]
    fn keywords_do_not_care_about_case() {
        let a = parse("select * from kv").unwrap();
        let b = parse("SELECT * FROM kv").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn an_insert_keeps_its_values_in_order() {
        let s = parse("INSERT INTO kv (id, a, c) VALUES (?1, 7, 'hi')").unwrap();
        let Stmt::Insert { table, columns, values } = s else { panic!() };
        assert_eq!(table, "kv");
        assert_eq!(columns, vec!["id", "a", "c"]);
        assert_eq!(values[0], Expr::Param(0));
        assert_eq!(values[1], Expr::Lit(Value::Int(7)));
        assert_eq!(values[2], Expr::Lit(Value::Text("hi".into())));
    }

    #[test]
    fn a_bare_question_mark_counts_itself() {
        let s = parse("SELECT a FROM kv WHERE id = ? AND a = ?").unwrap();
        let Stmt::Select { filter, .. } = s else { panic!() };
        assert_eq!(filter[0].value, Expr::Param(0));
        assert_eq!(filter[1].value, Expr::Param(1));
    }

    #[test]
    fn every_comparison_parses() {
        for (text, want) in [
            ("=", Cmp::Eq), ("<>", Cmp::Ne), ("!=", Cmp::Ne),
            ("<", Cmp::Lt), ("<=", Cmp::Le), (">", Cmp::Gt), (">=", Cmp::Ge),
        ] {
            let s = parse(&format!("SELECT a FROM kv WHERE id {text} 1")).unwrap();
            let Stmt::Select { filter, .. } = s else { panic!() };
            assert_eq!(filter[0].cmp, want, "{text}");
        }
    }

    #[test]
    fn a_number_can_be_negative() {
        let Stmt::Select { filter, .. } = parse("SELECT a FROM kv WHERE a < -7").unwrap() else { panic!() };
        assert_eq!(filter[0].value, Expr::Lit(Value::Int(-7)));
        let Stmt::Select { filter, .. } = parse("SELECT a FROM kv WHERE b >= -1.5").unwrap() else { panic!() };
        assert_eq!(filter[0].value, Expr::Lit(Value::Real(-1.5)));
        let Stmt::Select { filter, .. } = parse("SELECT a FROM kv WHERE a = +3").unwrap() else { panic!() };
        assert_eq!(filter[0].value, Expr::Lit(Value::Int(3)));
    }

    #[test]
    fn a_minus_still_starts_a_comment() {
        let s = parse("SELECT a FROM kv -- a < -7\n WHERE id = 1").unwrap();
        let Stmt::Select { filter, .. } = s else { panic!() };
        assert_eq!(filter.len(), 1);
    }

    #[test]
    fn a_string_can_hold_a_quote() {
        let s = parse("SELECT a FROM kv WHERE c = 'it''s'").unwrap();
        let Stmt::Select { filter, .. } = s else { panic!() };
        assert_eq!(filter[0].value, Expr::Lit(Value::Text("it's".into())));
    }

    #[test]
    fn select_takes_a_star_a_list_or_an_aggregate() {
        let Stmt::Select { project, .. } = parse("SELECT * FROM kv").unwrap() else { panic!() };
        assert_eq!(project, Project::All);
        let Stmt::Select { project, .. } = parse("SELECT a, c FROM kv").unwrap() else { panic!() };
        assert_eq!(project, Project::Columns(vec![Name::bare("a"), Name::bare("c")]));
        let Stmt::Select { project, .. } = parse("SELECT COUNT(*) FROM kv").unwrap() else { panic!() };
        assert_eq!(project, Project::Aggs(vec![Agg { func: Func::Count, arg: None }]));
    }

    #[test]
    fn every_aggregate_parses() {
        for (text, func) in [("SUM", Func::Sum), ("MIN", Func::Min), ("MAX", Func::Max)] {
            let s = parse(&format!("SELECT {text}(a) FROM kv")).unwrap();
            let Stmt::Select { project, .. } = s else { panic!() };
            assert_eq!(project, Project::Aggs(vec![Agg { func, arg: Some(Name::bare("a")) }]));
        }
        let s = parse("SELECT COUNT(*), SUM(a), MIN(b), MAX(c) FROM kv").unwrap();
        let Stmt::Select { project, .. } = s else { panic!() };
        let Project::Aggs(aggs) = project else { panic!() };
        assert_eq!(aggs.len(), 4);
        // Only COUNT may take a star.
        assert!(parse("SELECT SUM(*) FROM kv").is_err());
        // A column called count is still a column.
        let s = parse("SELECT count FROM kv").unwrap();
        let Stmt::Select { project, .. } = s else { panic!() };
        assert_eq!(project, Project::Columns(vec![Name::bare("count")]));
    }

    #[test]
    fn a_name_can_say_its_table() {
        let s = parse("SELECT a.x, y FROM a WHERE a.z = 1").unwrap();
        let Stmt::Select { project, filter, .. } = s else { panic!() };
        assert_eq!(
            project,
            Project::Columns(vec![
                Name { table: Some("a".into()), column: "x".into() },
                Name::bare("y"),
            ])
        );
        assert_eq!(filter[0].column, Name { table: Some("a".into()), column: "z".into() });
    }

    #[test]
    fn a_join_says_what_is_equal_to_what() {
        let s = parse("SELECT a.x FROM a JOIN b ON a.k = b.id WHERE b.y > 1").unwrap();
        let Stmt::Select { join, filter, .. } = s else { panic!() };
        let join = join.unwrap();
        assert_eq!(join.table, "b");
        assert_eq!(join.left, Name { table: Some("a".into()), column: "k".into() });
        assert_eq!(join.right, Name { table: Some("b".into()), column: "id".into() });
        assert_eq!(filter.len(), 1);
        assert!(parse("SELECT x FROM a INNER JOIN b ON a.k = b.id").is_ok());
        assert!(parse("SELECT x FROM a JOIN b ON a.k > b.id").is_err());
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
    fn an_index_is_made_and_dropped_by_name() {
        let s = parse("CREATE INDEX kv_a ON kv (a)").unwrap();
        assert_eq!(
            s,
            Stmt::CreateIndex {
                name: "kv_a".into(),
                table: "kv".into(),
                column: "a".into(),
                if_missing: false
            }
        );
        let s = parse("CREATE UNIQUE INDEX IF NOT EXISTS kv_a ON kv (a DESC)").unwrap();
        let Stmt::CreateIndex { if_missing, .. } = s else { panic!() };
        assert!(if_missing);
        assert_eq!(parse("DROP INDEX kv_a").unwrap(), Stmt::DropIndex { name: "kv_a".into(), if_there: false });
        assert_eq!(parse("DROP INDEX IF EXISTS kv_a").unwrap(), Stmt::DropIndex { name: "kv_a".into(), if_there: true });
    }

    #[test]
    fn an_update_takes_several_columns() {
        let s = parse("UPDATE kv SET a = ?1, c = 'x' WHERE id = ?2").unwrap();
        let Stmt::Update { sets, filter, .. } = s else { panic!() };
        assert_eq!(sets.len(), 2);
        assert_eq!(sets[0].1, Expr::Param(0));
        assert_eq!(filter[0].value, Expr::Param(1));
    }

    #[test]
    fn transactions_are_words_of_their_own() {
        assert_eq!(parse("BEGIN").unwrap(), Stmt::Begin);
        assert_eq!(parse("BEGIN TRANSACTION").unwrap(), Stmt::Begin);
        assert_eq!(parse("COMMIT").unwrap(), Stmt::Commit);
        assert_eq!(parse("END").unwrap(), Stmt::Commit);
        assert_eq!(parse("ROLLBACK").unwrap(), Stmt::Rollback);
    }

    #[test]
    fn a_comment_runs_to_the_end_of_the_line() {
        let s = parse("SELECT a FROM kv -- the rest of this is nothing\n WHERE id = 1").unwrap();
        let Stmt::Select { filter, .. } = s else { panic!() };
        assert_eq!(filter.len(), 1);
    }

    #[test]
    fn nonsense_comes_back_as_an_error_not_a_panic() {
        for bad in [
            "SELECT", "SELECT a FROM", "INSERT INTO kv VALUES", "WOBBLE kv",
            "SELECT a FROM kv WHERE", "SELECT a FROM kv WHERE id", "CREATE TABLE kv (a WOBBLE)",
            "SELECT a FROM kv WHERE c = 'never closed", "SELECT a FROM kv; SELECT a FROM kv",
            "SELECT a FROM kv LIMIT x", "SELECT a FROM kv WHERE id = ?0",
        ] {
            assert!(parse(bad).is_err(), "{bad} should not parse");
        }
    }

    #[test]
    fn a_trailing_semicolon_is_fine() {
        assert!(parse("SELECT a FROM kv;").is_ok());
    }
}
