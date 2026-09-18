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

## Phase 3: durability

An append-only log with a checksum on every record, a snapshot when the
log passes 4 MB, and two modes. Both tables below are ferrite with its
files on the same disk as SQLite's.

| workload | SQLite FULL | ferrite FULL | SQLite NORMAL | ferrite NORMAL |
|---|---|---|---|---|
| bulk insert | 1 184 987 | 1 112 358 | 1 261 124 | 1 435 231 |
| all reads | 422 223 | 1 649 275 | 421 944 | 1 991 607 |
| 95/5 read-update | 44 988 | 84 389 | 289 953 | 1 347 242 |
| 50/50 read-update | 10 310 | 11 288 | 116 886 | 737 909 |

**The crash gate is met.** 10,000 runs a mode, each one a child process
killed with SIGKILL while it was writing. Every key the parent had seen
committed was in the database afterwards, no run left a hole in the
middle, and no run left files that would not open.

**Every target is met but one.**

| workload | target | got | |
|---|---|---|---|
| all reads | 2 000 000 | 1 991 607 | met |
| 95/5, NORMAL | 1 000 000 | 1 347 242 | beaten |
| 50/50, NORMAL | 500 000 | 737 909 | beaten |
| 95/5, FULL | match SQLite | 84 389 against 44 988 | beaten |
| 50/50, FULL | match SQLite | 11 288 against 10 310 | beaten |
| bulk insert | 3 000 000 | 1 435 231 | short of the target, ahead of SQLite |

The bulk insert target was set at phase 0, before any of this existed,
and it assumed a load that never touches a disk. In memory ferrite does
3.1 million rows a second, which is what that number was about. With the
rows on a disk it does 1.4 million, against SQLite's 1.26 million.

### Where the bulk load was going wrong

It ran at 580 038 rows a second, half of SQLite, until the load was
taken apart and each part timed on its own.

| part | before | after |
|---|---|---|
| building the commit | 809 ns a row | 686 ns |
| writing the commit | 1 334 ns a row | 259 ns |
| all of it | 2 143 ns a row | 944 ns |

Three things were in the way.

- **Space was claimed by writing zeros, then the data went on top of
  them.** A 4.9 MB commit wrote 9.8 MB. Claiming pays for small
  appends, which would each lengthen the file, and costs double for a
  big one, which lengthens it once. Records over 256 KB now go straight
  down.
- **A snapshot fired straight after the big commit, writing everything
  a second time.** Size alone was the wrong question to ask. A hundred
  thousand rows loaded once fill the log with one record each, and a
  snapshot of them is the same size. A snapshot now waits until the log
  holds more than twice as many records as the database holds rows,
  which is what having something to compact looks like.
- **Every row was checked twice**, once on the way to the log and once
  by the table.

A fourth thing turned up in the crash runs rather than the bench. The
first claim was a megabyte, so opening a database and writing one row
cost a megabyte of zeros. The claim now starts at 64 KB and doubles.

### Two measurements that were wrong### Two measurements that were wrong

**Claiming log space made it worse before it made it better.** Every
append that lengthens a file makes the filesystem write its own journal
too, which is a second trip to the disk on every commit.

Claiming a megabyte ahead with `set_len` did not help. It made the file
longer without putting anything there, so the first write into each new
block paid the same cost anyway, and 50/50 in FULL fell from 4 600 to
1 020 operations a second. Writing real zeros put the blocks down for
good, and 95/5 in FULL went from 40 582 to 84 389.

**Copying every row on its way to the log cost over a microsecond an
insert.** Changes are now written as bytes as they happen, straight from
the row, and a commit is one buffer with its header filled in at the
end.

## Phase 4: indexes, joins, sorting and the aggregates

`CREATE INDEX`, an inner join, `ORDER BY`, `SUM`, `MIN` and `MAX`, on top
of what was already there.

**A lookup on a column that is not the key**, 100,000 rows, one value in
a hundred, so about a thousand rows come back each time.

| engine | walking | with an index | |
|---|---|---|---|
| ferrite | 5 039 µs | 286 µs | 18 times |
| SQLite | 3 574 µs | 68 µs | 53 times |

SQLite's indexed lookup is four times quicker than ferrite's, and that
is fair to say plainly. ferrite's index maps a value to the keys that
hold it, so every matching row costs a second walk down the row tree.
SQLite's index hands back its rowids in one run. Phase 5.

`COUNT(*)` is the one place ferrite is far ahead, at 0.1 µs against 33.
A count with nothing else to check never needs the rows: the table knows
how many it holds, and an index knows how many hold a given value.
Fetching each row to add one to a counter is work for nothing.

**What the random SQL now covers.** Two tables, joins both ways round,
joins onto a plain column, ORDER BY in both directions, the four
aggregates, and indexes made and dropped as the run goes. Both in memory
and on disk, with the database closed and opened again every fifty
rounds. Every answer matches SQLite.

Two things the test taught rather than the engine:

- **A LIMIT with no ORDER BY is not comparable between engines.** It
  takes whichever rows the engine walked first, and making an index
  changes that. Both answers are right and they are not the same, so the
  generator no longer writes one.
- **How many rows changed is only a question for statements that change
  rows.** After a CREATE INDEX, SQLite reports whatever the count
  happened to be beforehand.

**One thing to know about prepared statements.** A plan is settled
when the statement is prepared, index and all. One prepared before an
index was made still answers correctly, and still walks the table.
Prepare it again to pick the index up.

## Phase 5: speed, one measured step at a time

Three steps, each one asked for by a number. At the end of them ferrite
matches or beats SQLite on every row of the bench.

### The checksum was most of a commit

A bulk load of 100,000 rows spent 23 of its 26 milliseconds working out
a checksum. The profiler named it, so it got the instruction.

CRC32C is the same idea as the CRC32 in a zip file with one constant
changed, and it has been a single x86-64 instruction since 2008. Over
5 MB: **23.1 ms by a table in memory, 1.5 ms by the instruction.** The
table version is still there for a processor without it, and is tested
against it over six hundred lengths and both alignments.

### A query that wants only what the index holds never needs the rows

An index already holds the keys, so `SELECT id FROM t WHERE a = ?` is
answered from it alone. It now holds each row's own value too, so
`SELECT a FROM t WHERE a = ?` is as well.

That second one needed a decision. 5 and 5.0 are equal in SQL, so they
share one index entry, and the entry itself cannot say which of them a
given row holds. Reading the column off the entry would sometimes hand
back the wrong one.

So each row keeps its own value beside its key, at the cost of one value
per indexed row. A test puts 5 and 5.0 in the same index and checks that
each row gets its own back.

### Rows in a slab, slots in the index

Fetching a row through an index cost a second walk down the key tree
for every match, and on a thousand matches that was 6% slower than
SQLite. Now the rows sit in one vector, the key tree maps a key to a
slot in it, and so does every index. A row found through an index is an
array lookup away.

| asking for, through an index | before | after | SQLite |
|---|---|---|---|
| the key only | 256 µs | 53 µs | 125 µs |
| the indexed column | 363 µs | 54 µs | 122 µs |
| a text column | 549 µs | 316 µs | 570 µs |

The SQLite numbers went up along the way, and that is the comparison
getting fairer, not SQLite getting slower: its side of the bench now
copies the value out and builds a row for it, which is what ferrite's
side has to do. Walking the whole table got quicker too, 3 400 µs down
to 2 700, because the rows are now next to each other in memory.

Two bugs came out while the slab went in, both in things the random SQL
never does:

- ROLLBACK put the rows back and left the indexes as they were, so a
  rolled back insert stayed findable through an index.
- Dropping a table left its indexes standing.

### Where it stands, same disk, same mode

| workload | SQLite FULL | ferrite FULL | SQLite NORMAL | ferrite NORMAL |
|---|---|---|---|---|
| bulk insert | 1 166 367 | 1 567 386 | 1 261 951 | 1 870 768 |
| all reads | 421 320 | 1 584 627 | 424 433 | 1 922 235 |
| 95/5 read-update | 47 612 | 82 260 | 290 882 | 1 383 413 |
| 50/50 read-update | 10 173 | 11 325 | 116 955 | 702 545 |

Point reads in memory: 2 330 720 a second, and a prepared lookup costs
no more than the direct call.

### What was left alone

The adaptive radix tree in the plan. Point reads are five times SQLite,
and no measurement is asking for more.

## A trap this bench fell into

The first run reported a 3 µs fsync and near-identical FULL and NORMAL
numbers. `/tmp` on this machine is tmpfs, which is memory, so fsync cost
nothing and the whole write measurement was of RAM.

The bench now reads `/proc/self/mountinfo` and refuses to run on tmpfs
or ramfs.
