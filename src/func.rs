//! LIKE, and the functions a query can call on plain values.
//!
//! Each one answers the way SQLite does, the odd corners included, so
//! that a query moved over from SQLite means the same thing here.

use std::borrow::Cow;

use crate::sql::Scalar;
use crate::{Error, Result, Value};

fn err(what: &str) -> Error { Error::Sql(what.to_string()) }

/// A value as text, the way LIKE and the text functions see it. Null
/// has none. A real is written the way Rust writes it, which is also
/// SQLite's way for everyday numbers.
pub fn text(v: &Value) -> Option<Cow<'_, str>> {
    match v {
        Value::Null => None,
        Value::Text(t) => Some(Cow::Borrowed(t)),
        Value::Int(i) => Some(Cow::Owned(i.to_string())),
        Value::Real(r) => Some(Cow::Owned(format!("{r:?}"))),
        Value::Blob(b) => Some(String::from_utf8_lossy(b)),
    }
}

/// A value as text, where null has been ruled out already.
fn shown(v: &Value) -> Cow<'_, str> { text(v).unwrap_or_default() }

/// A value as a whole number, for a place or a length.
fn whole(v: &Value) -> i64 {
    match v {
        Value::Int(i) => *i,
        Value::Real(r) => *r as i64,
        Value::Text(t) => t.trim().parse::<f64>().map_or(0, |r| r as i64),
        Value::Null | Value::Blob(_) => 0,
    }
}

// ── LIKE ───────────────────────────────────────────────────────────────

/// How many bytes the character starting with this byte takes.
fn width(first: u8) -> usize {
    match first {
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xFF => 4,
        _ => 1,
    }
}

/// True when `text` fits `pattern`. A `%` stands for any run of
/// characters, an empty one too, and a `_` for exactly one. A letter
/// from a to z fits in either case. Any other letter has to be the
/// very same one, so `Æ` does not fit `æ`.
pub fn like(pattern: &str, text: &str) -> bool {
    let (p, t) = (pattern.as_bytes(), text.as_bytes());
    let (mut i, mut j) = (0, 0);
    // Where the pattern goes on after the last `%`, and how far into
    // the text that `%` has been stretched.
    let mut back: Option<(usize, usize)> = None;
    while j < t.len() {
        match p.get(i) {
            Some(b'%') => {
                i += 1;
                back = Some((i, j));
            }
            Some(b'_') => {
                i += 1;
                j += width(t[j]);
            }
            Some(c) if c.eq_ignore_ascii_case(&t[j]) => {
                i += 1;
                j += 1;
            }
            _ => match back {
                // Let the `%` take one more character and try again.
                Some((after, at)) => {
                    let at = at + width(t[at]);
                    back = Some((after, at));
                    i = after;
                    j = at;
                }
                None => return false,
            },
        }
    }
    while p.get(i) == Some(&b'%') { i += 1; }
    i == p.len()
}

// ── Functions ──────────────────────────────────────────────────────────

/// Work a function out from its arguments. A null among them gives
/// null back, for every function here.
pub fn call(f: Scalar, args: &[Cow<'_, Value>]) -> Result<Value> {
    if args.iter().any(|a| **a == Value::Null) { return Ok(Value::Null); }
    Ok(match f {
        Scalar::Substr => substr(&args[0], whole(&args[1]), args.get(2).map(|n| whole(n))),
        Scalar::Instr => {
            let (hay, needle) = (shown(&args[0]), shown(&args[1]));
            Value::Int(match hay.find(&*needle) {
                Some(at) => hay[..at].chars().count() as i64 + 1,
                None => 0,
            })
        }
        Scalar::JsonExtract => return json_extract(&shown(&args[0]), &shown(&args[1])),
    })
}

/// `substr(x, start)` or `substr(x, start, length)`, counting
/// characters from 1. A start below zero counts from the end, and a
/// length below zero takes the characters in front of the start. The
/// steps are SQLite's own, which is what makes the corners agree:
/// `substr('hello', 0, 3)` is `he`.
fn substr(x: &Value, start: i64, length: Option<i64>) -> Value {
    let (mut p1, mut p2) = (start, length.unwrap_or(i64::MAX / 2));
    let backwards = p2 < 0;
    if backwards { p2 = p2.saturating_neg(); }
    if p1 < 0 {
        // Only a start counted from the end needs to know how long the
        // text is, so only then is all of it walked.
        let len = match x {
            Value::Blob(b) => b.len(),
            other => text(other).map_or(0, |t| t.chars().count()),
        };
        p1 = p1.saturating_add(len as i64);
        if p1 < 0 {
            p2 = p2.saturating_add(p1).max(0);
            p1 = 0;
        }
    } else if p1 > 0 {
        p1 -= 1;
    } else if p2 > 0 {
        p2 -= 1;
    }
    if backwards {
        p1 -= p2;
        if p1 < 0 {
            p2 += p1;
            p1 = 0;
        }
    }
    let (from, count) = (p1 as usize, p2.max(0) as usize);
    match x {
        Value::Blob(b) => Value::Blob(b.iter().skip(from).take(count).copied().collect()),
        other => Value::Text(text(other).unwrap_or_default().chars().skip(from).take(count).collect()),
    }
}

// ── JSON ───────────────────────────────────────────────────────────────

/// One step into a JSON value: a key of an object, or a place in a list.
#[derive(Debug, PartialEq)]
enum Step {
    Key(String),
    At(usize),
}

/// Read a path like `$.a.b[2]`.
fn steps_of(path: &str) -> Result<Vec<Step>> {
    let bad = || err(&format!("I cannot read the JSON path {path}"));
    let mut rest = path.strip_prefix('$').ok_or_else(bad)?;
    let mut out = Vec::new();
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix('[') {
            let (number, tail) = after.split_once(']').ok_or_else(bad)?;
            out.push(Step::At(number.parse().map_err(|_| bad())?));
            rest = tail;
        } else if let Some(after) = rest.strip_prefix(".\"") {
            let (key, tail) = after.split_once('"').ok_or_else(bad)?;
            out.push(Step::Key(key.to_string()));
            rest = tail;
        } else if let Some(after) = rest.strip_prefix('.') {
            let end = after.find(['.', '[']).unwrap_or(after.len());
            if end == 0 { return Err(bad()); }
            out.push(Step::Key(after[..end].to_string()));
            rest = &after[end..];
        } else {
            return Err(bad());
        }
    }
    Ok(out)
}

/// A place in a JSON text. Nothing is built from the text: a value is
/// stepped over, or read where it stands.
struct Json<'a> {
    b: &'a [u8],
    at: usize,
}

impl Json<'_> {
    fn space(&mut self) {
        while matches!(self.b.get(self.at), Some(b' ' | b'\t' | b'\n' | b'\r')) { self.at += 1; }
    }

    fn eat(&mut self, want: u8) -> bool {
        if self.b.get(self.at) == Some(&want) { self.at += 1; return true; }
        false
    }

    fn want(&mut self, want: u8) -> Option<()> { self.eat(want).then_some(()) }

    /// Step over one value of any kind. None when what stands there is
    /// not JSON.
    fn skip(&mut self) -> Option<()> {
        self.space();
        match *self.b.get(self.at)? {
            b'{' => {
                self.at += 1;
                self.space();
                if self.eat(b'}') { return Some(()); }
                loop {
                    self.space();
                    self.string()?;
                    self.space();
                    self.want(b':')?;
                    self.skip()?;
                    self.space();
                    if !self.eat(b',') { return self.want(b'}'); }
                }
            }
            b'[' => {
                self.at += 1;
                self.space();
                if self.eat(b']') { return Some(()); }
                loop {
                    self.skip()?;
                    self.space();
                    if !self.eat(b',') { return self.want(b']'); }
                }
            }
            b'"' => self.string().map(|_| ()),
            b't' => self.word("true"),
            b'f' => self.word("false"),
            b'n' => self.word("null"),
            b'-' | b'0'..=b'9' => {
                let start = self.at;
                while matches!(self.b.get(self.at), Some(b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')) {
                    self.at += 1;
                }
                std::str::from_utf8(&self.b[start..self.at]).ok()?.parse::<f64>().ok().map(|_| ())
            }
            _ => None,
        }
    }

    fn word(&mut self, word: &str) -> Option<()> {
        if !self.b[self.at..].starts_with(word.as_bytes()) { return None; }
        self.at += word.len();
        Some(())
    }

    fn hex4(&mut self) -> Option<u32> {
        let digits = std::str::from_utf8(self.b.get(self.at..self.at + 4)?).ok()?;
        self.at += 4;
        u32::from_str_radix(digits, 16).ok()
    }

    /// Read a string, with its backslashes worked out.
    fn string(&mut self) -> Option<String> {
        self.want(b'"')?;
        let mut out = Vec::new();
        loop {
            let c = *self.b.get(self.at)?;
            self.at += 1;
            match c {
                b'"' => break,
                b'\\' => {
                    let e = *self.b.get(self.at)?;
                    self.at += 1;
                    match e {
                        b'"' | b'\\' | b'/' => out.push(e),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'u' => {
                            let mut code = self.hex4()?;
                            // A character past the first 65,536 comes
                            // as two halves, one after the other.
                            if (0xD800..0xDC00).contains(&code) && self.b[self.at..].starts_with(b"\\u") {
                                self.at += 2;
                                let low = self.hex4()?;
                                code = 0x10000 + ((code - 0xD800) << 10) + (low.wrapping_sub(0xDC00) & 0x3FF);
                            }
                            let ch = char::from_u32(code).unwrap_or('\u{FFFD}');
                            out.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes());
                        }
                        _ => return None,
                    }
                }
                _ => out.push(c),
            }
        }
        String::from_utf8(out).ok()
    }

    /// Follow the steps down from the value standing here, and stop at
    /// the first byte of what they lead to. None when they lead nowhere.
    fn find(&mut self, steps: &[Step]) -> Option<()> {
        for step in steps {
            self.space();
            match (step, *self.b.get(self.at)?) {
                (Step::Key(want), b'{') => {
                    self.at += 1;
                    loop {
                        self.space();
                        let key = self.string()?;
                        self.space();
                        self.want(b':')?;
                        if key == *want { break; }
                        self.skip()?;
                        self.space();
                        self.want(b',')?;
                    }
                }
                (Step::At(n), b'[') => {
                    self.at += 1;
                    for _ in 0..*n {
                        self.skip()?;
                        self.space();
                        self.want(b',')?;
                    }
                    self.space();
                    if self.b.get(self.at) == Some(&b']') { return None; }
                }
                _ => return None,
            }
        }
        self.space();
        Some(())
    }

    /// The value standing here, as SQL sees it: text and numbers as
    /// themselves, true and false as 1 and 0, and an object or a list
    /// as its JSON text with the spaces taken out.
    fn value(&mut self) -> Option<Value> {
        let start = self.at;
        Some(match *self.b.get(start)? {
            b'"' => Value::Text(self.string()?),
            b't' => Value::Int(1),
            b'f' => Value::Int(0),
            b'n' => Value::Null,
            b'{' | b'[' => {
                self.skip()?;
                Value::Text(packed(&self.b[start..self.at]))
            }
            _ => {
                self.skip()?;
                let number = std::str::from_utf8(&self.b[start..self.at]).ok()?;
                match number.parse::<i64>() {
                    Ok(i) => Value::Int(i),
                    Err(_) => Value::Real(number.parse().ok()?),
                }
            }
        })
    }
}

/// A piece of JSON with the spaces between its parts taken out. The
/// spaces inside its strings stay.
fn packed(json: &[u8]) -> String {
    let mut out = Vec::with_capacity(json.len());
    let (mut in_string, mut escaped) = (false, false);
    for &c in json {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_string = false;
            }
        } else if c == b'"' {
            in_string = true;
            out.push(c);
        } else if !matches!(c, b' ' | b'\t' | b'\n' | b'\r') {
            out.push(c);
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `json_extract(doc, path)`: the value a path leads to inside a JSON
/// text, or null when it leads nowhere. The whole text has to be JSON,
/// not only the part asked for, as in SQLite.
fn json_extract(doc: &str, path: &str) -> Result<Value> {
    let steps = steps_of(path)?;
    let mut all = Json { b: doc.as_bytes(), at: 0 };
    let whole = all.skip().is_some();
    all.space();
    if !whole || all.at != all.b.len() {
        return Err(err("json_extract was given text that is not JSON"));
    }
    let mut j = Json { b: doc.as_bytes(), at: 0 };
    Ok(j.find(&steps).and_then(|()| j.value()).unwrap_or(Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> Value { Value::Text(s.to_string()) }

    fn run(f: Scalar, args: &[Value]) -> Value {
        let args: Vec<Cow<'_, Value>> = args.iter().map(Cow::Borrowed).collect();
        call(f, &args).unwrap()
    }

    #[test]
    fn like_knows_percent_underscore_and_the_case_of_plain_letters() {
        for (pattern, text, want) in [
            ("alpha", "ALPHA", true),
            ("a%", "alpha", true),
            ("%a", "alpha", true),
            ("%ph%", "alpha", true),
            ("a_p%", "alpha", true),
            ("a_p", "alpha", false),
            ("%", "", true),
            ("", "", true),
            ("", "a", false),
            ("_", "", false),
            ("_", "é", true),
            ("%é_", "caféx", true),
            ("%a%a%", "banana", true),
            ("%a%a%a%a%", "banana", false),
            ("b%na", "banana", true),
            ("æ", "Æ", false),
            ("%@%", "a@b", true),
            ("%@%", "ab", false),
        ] {
            assert_eq!(like(pattern, text), want, "{text} LIKE {pattern}");
        }
    }

    #[test]
    fn substr_counts_characters_the_way_sqlite_does() {
        let cut = |start, length: Option<i64>| {
            let mut args = vec![t("hello"), Value::Int(start)];
            args.extend(length.map(Value::Int));
            run(Scalar::Substr, &args)
        };
        assert_eq!(cut(1, Some(2)), t("he"));
        assert_eq!(cut(2, None), t("ello"));
        assert_eq!(cut(0, Some(3)), t("he"));
        assert_eq!(cut(-3, None), t("llo"));
        assert_eq!(cut(-3, Some(2)), t("ll"));
        assert_eq!(cut(2, Some(-1)), t("h"));
        assert_eq!(cut(4, Some(-2)), t("el"));
        assert_eq!(cut(9, Some(2)), t(""));
        assert_eq!(cut(-9, Some(6)), t("he"));
        assert_eq!(run(Scalar::Substr, &[t("héllo"), Value::Int(2), Value::Int(2)]), t("él"));
        assert_eq!(run(Scalar::Substr, &[Value::Int(12345), Value::Int(2), Value::Int(2)]), t("23"));
        assert_eq!(run(Scalar::Substr, &[Value::Null, Value::Int(1)]), Value::Null);
        assert_eq!(run(Scalar::Substr, &[t("x"), Value::Null]), Value::Null);
    }

    #[test]
    fn instr_says_where_or_zero() {
        assert_eq!(run(Scalar::Instr, &[t("héllo"), t("l")]), Value::Int(3));
        assert_eq!(run(Scalar::Instr, &[t("hello"), t("z")]), Value::Int(0));
        assert_eq!(run(Scalar::Instr, &[t("abc"), t("")]), Value::Int(1));
        assert_eq!(run(Scalar::Instr, &[t("abc"), Value::Null]), Value::Null);
    }

    #[test]
    fn json_extract_follows_a_path_and_gives_sql_values_back() {
        let doc = r#" { "a" : { "b" : [1, 2.5, "x", null, true, false] }, "k" : "v\"é\n", "n" : -7, "e" : 1e3, "o" : { } } "#;
        let get = |path: &str| run(Scalar::JsonExtract, &[t(doc), t(path)]);
        assert_eq!(get("$.a.b[0]"), Value::Int(1));
        assert_eq!(get("$.a.b[1]"), Value::Real(2.5));
        assert_eq!(get("$.a.b[2]"), t("x"));
        assert_eq!(get("$.a.b[3]"), Value::Null);
        assert_eq!(get("$.a.b[4]"), Value::Int(1));
        assert_eq!(get("$.a.b[5]"), Value::Int(0));
        assert_eq!(get("$.a.b[6]"), Value::Null);
        assert_eq!(get("$.a.b"), t(r#"[1,2.5,"x",null,true,false]"#));
        assert_eq!(get("$.a"), t(r#"{"b":[1,2.5,"x",null,true,false]}"#));
        assert_eq!(get("$.k"), t("v\"é\n"));
        assert_eq!(get("$.\"k\""), t("v\"é\n"));
        assert_eq!(get("$.n"), Value::Int(-7));
        assert_eq!(get("$.e"), Value::Real(1000.0));
        assert_eq!(get("$.o"), t("{}"));
        assert_eq!(get("$.missing"), Value::Null);
        assert_eq!(get("$.k.deeper"), Value::Null);
        assert_eq!(run(Scalar::JsonExtract, &[t("[10, 20]"), t("$[1]")]), Value::Int(20));
        assert_eq!(run(Scalar::JsonExtract, &[t("7"), t("$")]), Value::Int(7));
        assert_eq!(run(Scalar::JsonExtract, &[Value::Null, t("$.a")]), Value::Null);
    }

    #[test]
    fn json_extract_refuses_what_is_not_json_and_a_path_it_cannot_read() {
        let bad = |doc: &str, path: &str| {
            let args = [Cow::Owned(t(doc)), Cow::Owned(t(path))];
            call(Scalar::JsonExtract, &args).is_err()
        };
        assert!(bad("alpha", "$.a"));
        assert!(bad("{\"a\":1", "$.a"));
        assert!(bad("{\"a\":1} x", "$.a"));
        assert!(bad("{\"a\":1}", "a"));
        assert!(bad("{\"a\":1}", "$.a["));
    }
}
