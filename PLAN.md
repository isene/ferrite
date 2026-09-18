# ferrite: the plan

An embedded SQL database in Rust that beats SQLite at small reads and
writes. It ships as one crate in the Fe2O3 suite, public domain.

## What it can and cannot beat

- Reads are the big win. The tables live in RAM, so a lookup never
  decodes a disk page.
- Batched writes get much faster, because many rows share one fsync.
- A single commit that must survive a power cut will be no faster than
  SQLite. One fsync took 636 µs at the median on the build laptop.
  Both engines pay that.

## Left out on purpose

- No server and no network
- No two processes sharing one file
- No views, triggers, window functions or subqueries at first
- No big-scan analytics

## Phase 0: the benchmark, before any engine code

- A bench tool runs the same workload on SQLite and on ferrite.
- Workloads: all reads, 95/5 read-update, 50/50, and bulk insert.
- It reports operations per second, p50 and p99 latency, and CPU time
  per operation.
- SQLite gets its best settings: WAL mode and prepared statements. That
  keeps the comparison fair.
- The baseline numbers set the targets for every later phase.

## Phase 1: the core, no SQL yet

- Types: integer, real, text, blob, null.
- Rows kept in memory per table, with a primary-key index on Rust's
  standard B-tree map.
- A plain Rust API for insert, get, update and delete.
- Gate: point reads beat SQLite's prepared statements on the bench.

## Phase 2: SQL

- A hand-written parser for CREATE TABLE, INSERT, SELECT, UPDATE,
  DELETE, WHERE, BEGIN and COMMIT.
- Prepared statements: parse and plan once, run many times.
- Gate: a prepared lookup by key costs within 10% of the plain Rust
  call.
- Correctness: random SQL runs on both engines, and every answer must
  match SQLite's.

## Phase 3: durability

- An append-only log, with every commit as one record.
- Two modes. FULL fsyncs each commit. NORMAL gathers commits into one
  fsync.
- A snapshot when the log passes a size, never on a timer.
- On open: load the snapshot, then replay the log.
- Gate: 10,000 kill -9 runs mid-write lose no committed row in FULL
  mode, and never corrupt the file.

## Phase 4: the rest of basic SQL

- Secondary indexes, joins through an index, ORDER BY, LIMIT, COUNT,
  SUM, MIN and MAX.
- Same gate: every answer matches SQLite's.

## Phase 5: speed, one measured step at a time

- Try an adaptive radix tree in place of the B-tree map. Keep it only
  if the bench says so.
- Allocate rows from one memory arena per table.
- Key comparison and hashing with the CRC32 and SIMD instructions.
- Assembly only for the functions the profiler names, compiled
  statements included.

## Phase 6: other users

- A C interface, so any language can call it.
- A small shell built on crust.
- One of Geir's own tools as the first real user.

## Rules for every phase

- Cold when idle: no background threads and no timers. An idle open
  database makes zero syscalls, checked with strace.
- Each step lands with its tests and its bench numbers. Nothing merges
  slower than what it replaces.
- Crate name `fe2o3-ferrite`, library name `ferrite`, like the rest of
  the suite.

## Open decisions for Geir

- Default durability. Recommended: FULL, with NORMAL as an opt-in. Data
  safety is the default, and speed is a choice.
- First real user. Recommended: tock. It is small, runs on SQLite
  already, and he uses it daily.

## Why these choices

- **Rust, not assembly.** A small query spends its time waiting on the
  disk and on memory. The language cannot shorten a wait. Rust's
  compiler catches whole classes of data-losing bugs before they run.
- **Tables in RAM.** A read never waits on a disk page, only on
  memory.
- **No locks.** One thread owns the data. Locks and latches eat most of
  the time in a classic engine.
- **Prepared statements.** Parsing SQL costs more than a small lookup.
  Doing it once removes that cost from every later call.

## Ideas it borrows

- H-Store and VoltDB: one core per slice of data, no locks.
- HyPer and Umbra: queries compiled to machine code.
- The adaptive radix tree: an in-memory index shaped for CPU caches.
- Differential testing against SQLite, the way SQLancer finds bugs.
