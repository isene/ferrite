//! ferrite: an embedded SQL database that keeps its tables in memory.
//!
//! There is no engine here yet. `PLAN.md` starts with a benchmark, so
//! that every later phase has a number to beat rather than a hope. The
//! bench lives in `src/bin/bench.rs` and runs behind the `bench`
//! feature, which is the only thing that pulls SQLite in.
//!
//! Phase 1 puts the core here: the value types, a table, and a
//! primary-key index.

/// What a value in a column can be. SQLite's five, and no more.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_knows_what_it_is() {
        assert_eq!(Value::Int(1).type_name(), "integer");
        assert_eq!(Value::Text("x".into()).type_name(), "text");
        assert_eq!(Value::Null.type_name(), "null");
    }
}
