# ferrite

An embedded SQL database in Rust, built to beat SQLite at small reads and
writes. Part of the [Fe2O3](https://github.com/isene/fe2o3) suite.

**Status:** phase 5 under way. There is nothing to install yet. The plan is
in [PLAN.md](PLAN.md) and the numbers are in [BASELINE.md](BASELINE.md).

## Read this before you store anything you care about

`Db::open` **defaults to NORMAL durability.** A commit is written at
once, so a program that crashes loses nothing. A power cut or a kernel
panic can lose the last few commits.

`Db::open_with(dir, Durability::Full)` forces every commit to the disk
before it returns. Nothing is lost, and writes cost about ten times as
much, because every commit waits for one fsync.

Both were killed with SIGKILL 10,000 times mid-write. Neither lost a
committed row and neither left a file that would not open.

It speaks CREATE TABLE, CREATE INDEX, INSERT, SELECT, UPDATE, DELETE,
WHERE, an inner join, ORDER BY, LIMIT, COUNT, SUM, MIN, MAX and
transactions. With its files on the same disk as SQLite's, it
reads about 2 million rows a second against SQLite's 422 thousand, and
does 738 thousand mixed operations against SQLite's 117 thousand.

60,000 generated statements have been run on ferrite and on SQLite with
every answer compared. Durability comes in phase 3, and the write
numbers will change when it does.

**The name:** ferrite is iron oxide, like Fe2O3. Ferrite cores were the
memory of early computers, and ferrite keeps its tables in memory.

**License:** [Unlicense](LICENSE), public domain.
