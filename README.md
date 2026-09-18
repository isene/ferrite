# ferrite

<img src="img/ferrite.svg" align="right" width="150">

**An embedded SQL database in Rust that keeps its tables in memory.**

![Rust](https://img.shields.io/badge/language-Rust-f74c00) ![License](https://img.shields.io/badge/license-Unlicense-green) ![Platform](https://img.shields.io/badge/platform-Linux%20%7C%20macOS-blue) ![Stay Amazing](https://img.shields.io/badge/Stay-Amazing-important)

Built to beat SQLite at small reads and writes, and measured against it
on the same disk in the same mode. The comparison, speed and features
side by side, is one page: [isene.org/ferrite](https://isene.org/ferrite/).

Part of the [Fe₂O₃ suite](https://isene.github.io/fe2o3/).

## Read this before you store anything you care about

`Db::open` **defaults to NORMAL durability.** A commit is written at
once, so a program that crashes loses nothing. A power cut or a kernel
panic can lose the last few commits.

`Db::open_with(dir, Durability::Full)` forces every commit to the disk
before it returns. Nothing is lost, and writes cost about ten times as
much, because every commit waits for one fsync.

Both were killed with SIGKILL 10,000 times mid-write. Neither lost a
committed row and neither left a file that would not open.

## Speed

Operations a second on a table of 100,000 rows. Median of five runs,
ext4 on NVMe, SQLite 3.46 with WAL and prepared statements.

| workload | SQLite FULL | ferrite FULL | SQLite NORMAL | ferrite NORMAL |
|---|---:|---:|---:|---:|
| bulk insert | 1 166 367 | **1 567 386** | 1 261 951 | **1 870 768** |
| all reads | 421 320 | **1 584 627** | 424 433 | **1 922 235** |
| 95 reads, 5 updates | 47 612 | **82 260** | 290 882 | **1 383 413** |
| 50 reads, 50 updates | 10 173 | **11 325** | 116 955 | **702 545** |

One lookup through an index on a column that is not the key, about a
thousand rows matching, in microseconds:

| asking for | SQLite | ferrite |
|---|---:|---:|
| the key only | 125 | **53** |
| the indexed column | 122 | **54** |
| a text column, so the row is fetched | 570 | **316** |
| walking the whole table, no index | 3 700 | **2 700** |

Every number, and how it was measured, is in [BASELINE.md](BASELINE.md).
Nothing merges that is slower than what it replaces.

## What it speaks

CREATE TABLE, CREATE INDEX, INSERT, SELECT, UPDATE, DELETE, WHERE, one
inner join, ORDER BY, LIMIT, COUNT, SUM, MIN, MAX and transactions.
Five types, kept as declared: a string in an INTEGER column is refused.

Tens of thousands of generated statements have been run on ferrite and
on SQLite, in memory and on disk, with every answer compared.

## Using it

Not on crates.io yet. Point Cargo at a checkout:

```toml
[dependencies]
ferrite = { path = "../ferrite", package = "fe2o3-ferrite" }
```

```rust
use ferrite::{Db, Value};

let mut db = Db::open("/path/to/dir")?;
db.execute("CREATE TABLE IF NOT EXISTS kv (id INTEGER PRIMARY KEY, a INTEGER, c TEXT)", &[])?;
db.execute(
    "INSERT INTO kv (id, a, c) VALUES (?1, ?2, ?3)",
    &[Value::Int(1), Value::Int(7), Value::Text("hello".into())],
)?;

// Prepare once, run many times. A lookup by key costs the same as
// calling the table directly.
let get = db.prepare("SELECT c FROM kv WHERE id = ?1")?;
let out = get.query(&db, &[Value::Int(1)])?;
assert_eq!(out.rows()[0][0], Value::Text("hello".into()));
```

`Db::new()` is the same database in memory, gone when the program ends.

## What it is not

A server, a database shared between processes, or a home for more data
than the machine has memory. One thread in one process. If a table
would not fit in RAM, or two programs need the same file, use SQLite.

## The name

Ferrite is iron oxide, like Fe₂O₃. Ferrite cores were the memory of
early computers, and ferrite keeps its tables in memory. The logo is a
plane of core memory with one bit set.

## License

[Unlicense](LICENSE), public domain.
