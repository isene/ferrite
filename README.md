# ferrite

An embedded SQL database in Rust, built to beat SQLite at small reads and
writes. Part of the [Fe2O3](https://github.com/isene/fe2o3) suite.

**Status:** phase 2 done. There is nothing to install yet. The plan is
in [PLAN.md](PLAN.md) and the numbers are in [BASELINE.md](BASELINE.md).

It speaks CREATE TABLE, INSERT, SELECT, UPDATE, DELETE, WHERE, LIMIT,
COUNT and transactions. Point reads run at about 2 million a second
against SQLite's 422 thousand, and a prepared lookup costs no more than
calling the engine directly.

60,000 generated statements have been run on ferrite and on SQLite with
every answer compared. Durability comes in phase 3, and the write
numbers will change when it does.

**The name:** ferrite is iron oxide, like Fe2O3. Ferrite cores were the
memory of early computers, and ferrite keeps its tables in memory.

**License:** [Unlicense](LICENSE), public domain.
