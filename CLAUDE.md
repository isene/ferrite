# ferrite: Claude Code instructions

An embedded SQL database in Rust that keeps its tables in memory. Part
of the Fe₂O₃ suite. The whole shape of the work is in `PLAN.md`; read it
before touching anything, and do not skip a phase gate.

## The one rule that decides arguments

**No number goes in a commit message, a README or a report unless the
bench printed it.** Phase 0 exists so that every later claim has a
measurement behind it. "Should be faster" is not a result.

- `BASELINE.md` holds the SQLite numbers to beat, and the machine they
  came from. Update it when the machine or the workload changes, never
  to make a result look better.
- Nothing merges slower than what it replaces. If a change is slower and
  you want it anyway, say so in the commit message with both numbers.

## The bench

```bash
PATH="/usr/bin:$PATH" cargo build --release --features bench
./target/release/ferrite-bench
```

SQLite comes from the system library, not a bundled copy, so both
engines are measured against the SQLite the machine actually has.

**It must run on a real disk.** `/tmp` here is tmpfs, which is memory:
fsync costs nothing there and the bench ends up measuring RAM. The first
run fooled itself exactly that way and reported a 3 µs fsync.

It now reads `/proc/self/mountinfo` and refuses tmpfs. The default is
`~/.cache/ferrite-bench`, and `FERRITE_BENCH_DIR` moves it.

## Design goals, in priority order

1. **No wasted CPU cycles.** An open, idle database makes zero syscalls.
   No background threads, no timers. Checkpoint on log size, never on a
   clock. Check with `strace -c`.
2. **Lightning fast.** Microsecond operations. A point read must not
   wait on a disk page.
3. **More battery life.** The two above, together.

## What it is not

No server, no network, no two processes on one file, no big-scan
analytics. Those are in `PLAN.md` under "Left out on purpose" and are
not open for quiet expansion.

## Rust, not assembly

The plan says assembly only for functions the profiler names. A small
query waits on the disk and on memory; the language cannot shorten a
wait. Do not hand-write assembly on a hunch.

## Dependencies

Justify every one. Right now there is exactly one, `rusqlite`, it is
optional, and only the bench pulls it in. A normal `cargo build` fetches
nothing. Keep it that way: the library itself should stay dependency
free.

## Crate names

```toml
[package]
name = "fe2o3-ferrite"   # crates.io
[lib]
name = "ferrite"          # use ferrite::...
```

## Release flow

Same as the rest of the suite: bump the version, build, commit as
"Subject (vX.Y.Z)", tag, push both. Keep ferrite off the Fe₂O₃ landing
page until there is a release someone can install.

## Testing

- Unit tests beside the code.
- From phase 2: random SQL run against both engines, and every answer
  must match SQLite's. A difference is a bug in ferrite until proven
  otherwise.
- Phase 3 gate: 10,000 `kill -9` runs mid-write lose no committed row in
  FULL mode and never corrupt the file.

## Open decisions that are the user's, not yours

`PLAN.md` ends with two. Ask before the phase that needs the answer, and
do not pick for him:

- Default durability mode, before phase 3.
- The first real user, before phase 6.
