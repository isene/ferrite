# The baseline to beat

Phase 0 of [PLAN.md](PLAN.md). SQLite doing the work ferrite means to
do, measured before any engine code exists, so that every later phase
has a number to beat.

Reproduce with:

```bash
PATH="/usr/bin:$PATH" cargo build --release --features bench
FERRITE_BENCH_GOES=5 ./target/release/ferrite-bench
```

## The machine

- Intel Core Ultra 7 255H, 16 cores
- ext4 on NVMe (`/dev/nvme0n1p7`), in `~/.cache/ferrite-bench`
- SQLite 3.46.1 from the system library, WAL, statements prepared once
- 100,000 rows, 200,000 reads, 20,000 mixed operations
- Five goes at each workload; the table shows the middling one

## The floor under every durable commit

One 4 KB append plus fsync: **393 µs** at the median, 2,259 µs at p99.

Across runs that median wandered between 390 and 640 µs. It is the
price of promising that a committed row survives a power cut, and both
engines pay it. Nothing ferrite does can go below it.

## synchronous=FULL

Every commit reaches the disk before it returns.

| workload | ops/s | p50 µs | p99 µs | p99.9 µs | CPU µs/op |
|---|---|---|---|---|---|
| bulk insert | 1 184 987 | 0.65 | 0.90 | 6.3 | 0.82 |
| all reads | 422 223 | 2.05 | 3.53 | 5.3 | 2.36 |
| 95/5 read-update | 44 988 | 2.62 | 530.43 | 1752.1 | 5.64 |
| 50/50 read-update | 10 310 | 14.35 | 353.95 | 633.5 | 19.25 |

## synchronous=NORMAL

Commits are written but not forced to the disk. A power cut can lose the
last few.

| workload | ops/s | p50 µs | p99 µs | p99.9 µs | CPU µs/op |
|---|---|---|---|---|---|
| bulk insert | 1 261 124 | 0.64 | 0.92 | 6.5 | 0.79 |
| all reads | 421 944 | 2.06 | 3.40 | 5.5 | 2.37 |
| 95/5 read-update | 289 953 | 2.11 | 8.28 | 11.8 | 3.07 |
| 50/50 read-update | 116 886 | 4.71 | 10.25 | 33.0 | 7.35 |

## What the numbers say

**The durability switch beats any amount of tuning.** Six times on 95/5,
eleven times on 50/50. That is the whole difference between forcing each
commit to the disk and not.

**Reads do not care about it at all.** 422,000 a second either way, and
2.36 µs of CPU for each. That CPU figure is what ferrite has to cut: a
lookup in a Rust B-tree map over 100,000 keys is a fraction of it, and
the rest is SQLite decoding a page it has already cached.

**The disk stalls, and it stalls hard.** The worst single operation in a
run reached 5.5 ms on the bulk load and 6.5 ms on 95/5, and one go at
50/50 dropped to 868 operations a second when something in the kernel or
the drive took seconds to come back. ferrite will meet the same disk, so
its own tail will look like this too.

## Proposed targets

To be confirmed before phase 1 starts.

| workload | SQLite | ferrite target | why |
|---|---|---|---|
| all reads | 422 223 /s, 2.36 µs CPU | 2 000 000 /s, under 0.5 µs CPU | no page to decode, no lock to take |
| bulk insert | 1 261 124 /s | 3 000 000 /s | rows go straight into memory, one log record for the batch |
| 95/5, NORMAL | 289 953 /s | 1 000 000 /s | the read side carries it |
| 50/50, NORMAL | 116 886 /s | 500 000 /s | as above |
| 95/5, FULL | 44 988 /s | match, within 10% | one fsync each, same disk |
| 50/50, FULL | 10 310 /s | match, within 10% | as above |

Say the two FULL rows plainly: ferrite will not beat SQLite there. Both
engines wait on the same disk for the same reason.

## Where ferrite stands

Phase 1: tables in memory, a B-tree map under an integer primary key,
and a plain Rust API. No log and no fsync yet, so there is one set of
numbers rather than one per durability mode.

| workload | ops/s | p50 µs | p99 µs | CPU µs/op | against the target |
|---|---|---|---|---|---|
| bulk insert | 3 152 865 | 0.27 | 0.40 | 0.32 | met |
| all reads | 1 951 034 | 0.39 | 0.99 | 0.52 | just under |
| 95/5 read-update | 2 432 759 | 0.32 | 0.71 | 0.40 | not comparable yet |
| 50/50 read-update | 2 281 932 | 0.34 | 0.87 | 0.45 | not comparable yet |

**The phase 1 gate is met.** A point read runs at 1 951 034 a second
against SQLite's 422 223, and costs 0.52 µs of CPU against 2.36 µs.
That is 4.6 times the throughput for a fifth of the CPU.

**The read target is missed by a hair**, 1.95 million against 2 million,
and 0.52 µs of CPU against 0.50. Phase 5 is where speed gets worked on
one measured step at a time, so it stays as it is until then.

**The two mixed rows prove nothing yet.** ferrite writes to memory and
stops there, while SQLite writes to a disk. Phase 3 adds the log, and
those two rows will fall. Compare them then, not now.

## Phase 2: SQL

A hand-written scanner and parser, planning done once, and statements
kept and run many times.

| workload | plain Rust | through SQL |
|---|---|---|
| bulk insert | 3 101 699 /s | 2 355 727 /s |
| all reads | 2 084 424 /s | 1 786 465 /s |
| 95/5 read-update | 2 800 138 /s | 2 195 300 /s |
| 50/50 read-update | 2 416 769 /s | 1 629 909 /s |

**The gate is met.** A prepared lookup by key measures 250 ns against
the direct call's 285 ns, with the two running in alternating batches
over the same rows.

Read that as "planning costs less than this bench can resolve", not as
"SQL is faster than not using SQL". A random lookup in 100,000 rows is
mostly waiting for memory, and 35 ns of the 285 is inside the noise that
code layout moves about. The gate asked for within 10%, and nothing here
is slower.

The two tables above tell a different story from the gate, because they
were measured minutes apart while the machine drifted. That is why the
gate has its own measurement.

### Random SQL against SQLite

60,000 generated statements over eleven seeds, with 60,000 answers
compared. Every one matches SQLite.

It found four real faults, none of which a hand-written test had caught:

- A negative number would not parse at all.
- `SELECT COUNT(*) ... LIMIT 2` counted two rows instead of counting all
  of them and then returning one row.
- `WHERE id = 4 AND id > 6` threw the second condition away and returned
  row 4.
- `WHERE id > 6 AND id < 3` asked a B-tree for a backwards range and
  brought the process down.

One difference is on purpose: SQLite will put a string in an INTEGER
column, and ferrite refuses.

## A trap this bench fell into

The first run reported a 3 µs fsync and near-identical FULL and NORMAL
numbers. `/tmp` on this machine is tmpfs, which is memory, so fsync cost
nothing and the whole write measurement was of RAM.

The bench now reads `/proc/self/mountinfo` and refuses to run on tmpfs
or ramfs.
