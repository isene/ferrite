//! Closing a database and opening it again has to give the same
//! database back. These are the quiet half of phase 3; the loud half is
//! `src/bin/crash.rs`, which kills the process mid-write.

use ferrite::{Db, Durability, Kind, Value};

/// A directory of its own for each test, cleaned up afterwards.
struct Dir(std::path::PathBuf);

impl Dir {
    fn new(what: &str) -> Dir {
        let p = std::env::temp_dir().join(format!("ferrite-{what}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        Dir(p)
    }
    fn path(&self) -> &std::path::Path { &self.0 }
}

impl Drop for Dir {
    fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
}

fn rows_of(db: &Db, sql: &str) -> Vec<Vec<Value>> {
    db.query(sql, &[]).unwrap().rows().to_vec()
}

#[test]
fn what_went_in_comes_back_out() {
    let dir = Dir::new("roundtrip");
    {
        let mut db = Db::open(dir.path()).unwrap();
        db.execute("CREATE TABLE kv (id INTEGER PRIMARY KEY, a INTEGER, b REAL, c TEXT)", &[])
            .unwrap();
        for i in 0..100i64 {
            db.execute(
                "INSERT INTO kv (id, a, b, c) VALUES (?1, ?2, ?3, ?4)",
                &[Value::Int(i), Value::Int(i * 3), Value::Real(i as f64 / 4.0), Value::Text(format!("row {i}"))],
            )
            .unwrap();
        }
        db.execute("UPDATE kv SET a = 999 WHERE id = 7", &[]).unwrap();
        db.execute("DELETE FROM kv WHERE id < 5", &[]).unwrap();
        db.flush().unwrap();
    }
    let db = Db::open(dir.path()).unwrap();
    assert_eq!(rows_of(&db, "SELECT COUNT(*) FROM kv"), vec![vec![Value::Int(95)]]);
    assert_eq!(rows_of(&db, "SELECT a FROM kv WHERE id = 7"), vec![vec![Value::Int(999)]]);
    assert!(rows_of(&db, "SELECT id FROM kv WHERE id = 3").is_empty());
    assert_eq!(
        rows_of(&db, "SELECT c FROM kv WHERE id = 42"),
        vec![vec![Value::Text("row 42".into())]]
    );
}

#[test]
fn every_kind_of_value_survives() {
    let dir = Dir::new("values");
    let want = vec![
        Value::Int(i64::MIN),
        Value::Real(-0.5),
        Value::Text("it's ✓ a string".into()),
        Value::Null,
    ];
    {
        let mut db = Db::open(dir.path()).unwrap();
        let t = db
            .create_table("t", &[("i", Kind::Int), ("r", Kind::Real), ("s", Kind::Text), ("n", Kind::Int)])
            .unwrap();
        db.insert(t, 1, want.clone()).unwrap();
        db.insert(t, 2, vec![Value::Int(i64::MAX), Value::Real(f64::MAX), Value::Text(String::new()), Value::Null])
            .unwrap();
        db.flush().unwrap();
    }
    let db = Db::open(dir.path()).unwrap();
    let t = db.table_id("t").unwrap();
    assert_eq!(db.table(t).get(1), Some(&want));
    assert_eq!(db.table(t).get_at(2, 0), Some(&Value::Int(i64::MAX)));
    assert_eq!(db.table(t).get_at(2, 1), Some(&Value::Real(f64::MAX)));
}

#[test]
fn a_committed_transaction_is_kept_and_a_rolled_back_one_is_not() {
    let dir = Dir::new("txn");
    {
        let mut db = Db::open(dir.path()).unwrap();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER)", &[]).unwrap();
        db.execute("BEGIN", &[]).unwrap();
        for i in 0..10i64 {
            db.execute("INSERT INTO t (id, a) VALUES (?1, ?2)", &[Value::Int(i), Value::Int(i)]).unwrap();
        }
        db.execute("COMMIT", &[]).unwrap();

        db.execute("BEGIN", &[]).unwrap();
        db.execute("INSERT INTO t (id, a) VALUES (99, 99)", &[]).unwrap();
        db.execute("UPDATE t SET a = 0 WHERE id = 1", &[]).unwrap();
        db.execute("DELETE FROM t WHERE id = 2", &[]).unwrap();
        db.execute("ROLLBACK", &[]).unwrap();
        db.flush().unwrap();
    }
    let db = Db::open(dir.path()).unwrap();
    assert_eq!(rows_of(&db, "SELECT COUNT(*) FROM t"), vec![vec![Value::Int(10)]]);
    assert_eq!(rows_of(&db, "SELECT a FROM t WHERE id = 1"), vec![vec![Value::Int(1)]]);
    assert!(rows_of(&db, "SELECT id FROM t WHERE id = 99").is_empty());
}

#[test]
fn a_snapshot_keeps_everything_and_empties_the_log() {
    let dir = Dir::new("snapshot");
    {
        let mut db = Db::open(dir.path()).unwrap();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER)", &[]).unwrap();
        for i in 0..500i64 {
            db.execute("INSERT INTO t (id, a) VALUES (?1, ?2)", &[Value::Int(i), Value::Int(i)]).unwrap();
        }
        db.checkpoint().unwrap();
        assert_eq!(std::fs::metadata(dir.path().join("log")).unwrap().len(), 0);
        db.execute("INSERT INTO t (id, a) VALUES (1000, 1)", &[]).unwrap();
        db.flush().unwrap();
    }
    let db = Db::open(dir.path()).unwrap();
    assert_eq!(rows_of(&db, "SELECT COUNT(*) FROM t"), vec![vec![Value::Int(501)]]);
    assert_eq!(rows_of(&db, "SELECT a FROM t WHERE id = 499"), vec![vec![Value::Int(499)]]);
}

#[test]
fn a_dropped_table_stays_dropped() {
    let dir = Dir::new("dropped");
    {
        let mut db = Db::open(dir.path()).unwrap();
        db.execute("CREATE TABLE gone (id INTEGER PRIMARY KEY, a INTEGER)", &[]).unwrap();
        db.execute("INSERT INTO gone (id, a) VALUES (1, 1)", &[]).unwrap();
        db.execute("CREATE TABLE kept (id INTEGER PRIMARY KEY, a INTEGER)", &[]).unwrap();
        db.execute("INSERT INTO kept (id, a) VALUES (2, 2)", &[]).unwrap();
        db.drop_table("gone").unwrap();
        db.flush().unwrap();
    }
    let db = Db::open(dir.path()).unwrap();
    assert!(db.table_id("gone").is_err());
    assert_eq!(rows_of(&db, "SELECT a FROM kept WHERE id = 2"), vec![vec![Value::Int(2)]]);
}

#[test]
fn a_dropped_name_can_be_used_again() {
    let dir = Dir::new("recreated");
    {
        let mut db = Db::open(dir.path()).unwrap();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER)", &[]).unwrap();
        db.execute("INSERT INTO t (id, a) VALUES (1, 111)", &[]).unwrap();
        db.drop_table("t").unwrap();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER)", &[]).unwrap();
        db.execute("INSERT INTO t (id, a) VALUES (1, 222)", &[]).unwrap();
        db.flush().unwrap();
    }
    let db = Db::open(dir.path()).unwrap();
    assert_eq!(rows_of(&db, "SELECT a FROM t WHERE id = 1"), vec![vec![Value::Int(222)]]);
    assert_eq!(rows_of(&db, "SELECT COUNT(*) FROM t"), vec![vec![Value::Int(1)]]);
}

#[test]
fn the_same_holds_after_a_snapshot() {
    let dir = Dir::new("dropsnap");
    {
        let mut db = Db::open(dir.path()).unwrap();
        db.execute("CREATE TABLE gone (id INTEGER PRIMARY KEY, a INTEGER)", &[]).unwrap();
        db.execute("CREATE TABLE kept (id INTEGER PRIMARY KEY, a INTEGER)", &[]).unwrap();
        db.execute("INSERT INTO kept (id, a) VALUES (2, 2)", &[]).unwrap();
        db.drop_table("gone").unwrap();
        db.checkpoint().unwrap();
        db.flush().unwrap();
    }
    let db = Db::open(dir.path()).unwrap();
    assert!(db.table_id("gone").is_err());
    assert_eq!(rows_of(&db, "SELECT a FROM kept WHERE id = 2"), vec![vec![Value::Int(2)]]);
}

#[test]
fn full_durability_is_a_choice_and_normal_is_the_default() {
    let dir = Dir::new("modes");
    let db = Db::open(dir.path()).unwrap();
    assert_eq!(db.durability(), Some(Durability::Normal));
    drop(db);
    let mut db = Db::open_with(dir.path(), Durability::Full).unwrap();
    assert_eq!(db.durability(), Some(Durability::Full));
    db.set_durability(Durability::Normal);
    assert_eq!(db.durability(), Some(Durability::Normal));
    // A database with no files has no durability to speak of.
    assert_eq!(Db::new().durability(), None);
}

#[test]
fn opening_an_empty_directory_gives_an_empty_database() {
    let dir = Dir::new("empty");
    let db = Db::open(dir.path()).unwrap();
    assert_eq!(db.table_names().count(), 0);
    assert!(dir.path().join("log").exists());
}

#[test]
fn a_key_handed_out_once_is_not_handed_out_again_after_a_reopen() {
    let dir = Dir::new("nextkey");
    {
        let mut db = Db::open(dir.path()).unwrap();
        db.execute_batch(
            "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, a INTEGER);
             CREATE TABLE s (k TEXT PRIMARY KEY, v INTEGER DEFAULT 3, UNIQUE(v));
             INSERT INTO t (a) VALUES (1);
             INSERT INTO t (a) VALUES (2);
             INSERT INTO t (a) VALUES (3);
             DELETE FROM t WHERE id = 3;
             INSERT INTO s (k, v) VALUES ('a', 1);",
        )
        .unwrap();
        // With the deleted row gone from the files.
        db.checkpoint().unwrap();
    }
    let mut db = Db::open(dir.path()).unwrap();
    db.execute("INSERT INTO t (a) VALUES (4)", &[]).unwrap();
    assert_eq!(db.last_insert_key(), 4, "key 3 was used once and is not used again");
    // The unique index, the default and the hidden rowid came back too.
    assert!(db.execute("INSERT INTO s (k, v) VALUES ('b', 1)", &[]).is_err());
    db.execute("INSERT INTO s (k) VALUES ('b')", &[]).unwrap();
    assert_eq!(rows_of(&db, "SELECT k, v FROM s WHERE k = 'b'"), vec![vec![Value::Text("b".into()), Value::Int(3)]]);
    assert_eq!(rows_of(&db, "SELECT rowid FROM s WHERE k = 'b'"), vec![vec![Value::Int(2)]]);
}
