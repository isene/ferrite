//! Where the time goes in a bulk load. Run it with:
//!
//! ```text
//! cargo test --release --features bench --test bulk_breakdown -- --ignored --nocapture
//! ```

use std::time::Instant;

use ferrite::{Db, Durability, Kind, Value};

const ROWS: u64 = 100_000;

fn dir(what: &str) -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let p = std::path::PathBuf::from(home).join(".cache").join(format!("ferrite-breakdown-{what}"));
    let _ = std::fs::remove_dir_all(&p);
    p
}

fn row(i: u64) -> Vec<Value> {
    vec![Value::Int(i as i64), Value::Real(i as f64 * 1.5), Value::Text(format!("row {i}"))]
}

#[test]
#[ignore]
fn where_the_time_goes() {
    // In memory, for the floor.
    let mut db = Db::new();
    let kv = db.create_table("kv", &[("a", Kind::Int), ("b", Kind::Real), ("c", Kind::Text)]).unwrap();
    let t0 = Instant::now();
    for i in 0..ROWS { db.insert(kv, i as i64, row(i)).unwrap(); }
    let memory = t0.elapsed();

    // On disk, one transaction, with each part timed on its own.
    let path = dir("disk");
    let mut db = Db::open_with(&path, Durability::Normal).unwrap();
    let kv = db.create_table("kv", &[("a", Kind::Int), ("b", Kind::Real), ("c", Kind::Text)]).unwrap();
    db.begin();
    let t0 = Instant::now();
    for i in 0..ROWS { db.insert(kv, i as i64, row(i)).unwrap(); }
    let building = t0.elapsed();
    let t0 = Instant::now();
    db.commit().unwrap();
    let committing = t0.elapsed();
    let log_after = std::fs::metadata(path.join("log")).map(|m| m.len()).unwrap_or(0);
    let snap_after = std::fs::metadata(path.join("snapshot")).map(|m| m.len()).unwrap_or(0);

    // The same again with the snapshot already out of the way, to see
    // what the commit costs without one.
    let path2 = dir("nosnap");
    let mut db2 = Db::open_with(&path2, Durability::Normal).unwrap();
    let kv2 = db2.create_table("kv", &[("a", Kind::Int), ("b", Kind::Real), ("c", Kind::Text)]).unwrap();
    db2.begin();
    for i in 0..ROWS { db2.insert(kv2, i as i64, row(i)).unwrap(); }
    let t0 = Instant::now();
    db2.commit().unwrap();
    let with_snapshot = t0.elapsed();
    let t0 = Instant::now();
    db2.checkpoint().unwrap();
    let a_snapshot_alone = t0.elapsed();

    let per = |d: std::time::Duration| d.as_secs_f64() * 1e9 / ROWS as f64;
    println!("\n{ROWS} rows, one transaction, NORMAL");
    println!("  in memory, no files      {:>8.0} ns a row  ({:.0} ms)", per(memory), memory.as_secs_f64() * 1e3);
    println!("  building the commit      {:>8.0} ns a row  ({:.0} ms)", per(building), building.as_secs_f64() * 1e3);
    println!("  writing the commit       {:>8.0} ns a row  ({:.0} ms)", per(committing), committing.as_secs_f64() * 1e3);
    println!("  of which, a snapshot     {:>8.0} ns a row  ({:.0} ms)", per(a_snapshot_alone), a_snapshot_alone.as_secs_f64() * 1e3);
    println!("  commit again, for check  {:>8.0} ns a row  ({:.0} ms)", per(with_snapshot), with_snapshot.as_secs_f64() * 1e3);
    println!("  log left {log_after} bytes, snapshot {snap_after} bytes");
    let total = building + committing;
    println!("  all of it                {:>8.0} ns a row  = {:.0} rows a second", per(total), ROWS as f64 / total.as_secs_f64());

    let _ = std::fs::remove_dir_all(&path);
    let _ = std::fs::remove_dir_all(&path2);
}
