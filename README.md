# ferrite

An embedded SQL database in Rust, built to beat SQLite at small reads and
writes. Part of the [Fe2O3](https://github.com/isene/fe2o3) suite.

**Status:** phase 1 done. There is nothing to install yet. The plan is
in [PLAN.md](PLAN.md) and the numbers are in [BASELINE.md](BASELINE.md).

Point reads run at 1.95 million a second against SQLite's 422 thousand,
and cost a fifth of the CPU. Durability comes in phase 3, and those
numbers will change.

**The name:** ferrite is iron oxide, like Fe2O3. Ferrite cores were the
memory of early computers, and ferrite keeps its tables in memory.

**License:** [Unlicense](LICENSE), public domain.
