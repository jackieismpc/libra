# SQLite pool lifecycle

Libra disables SQLx 0.9.0's idle-timeout and maximum-lifetime reaper for its
local SQLite pools through `internal::db::sqlite_pool_options`. This applies
to repository/global databases, read-only alternate configuration, and migration
reference databases. The sibling `libvault-rs` SQLite backend uses SQLx 0.9.0
and disables the same timers for all CRUD/list operations; Libra uses that
backend directly rather than a second compatibility pool. Connection limits,
acquisition timeouts, busy timeouts, and explicit pool closure retain their
existing behavior. Idle SQLite connections remain open until the pool closes
or is dropped; long-running processes therefore retain these connections
instead of periodically retiring them.

This works around a SQLx connection-return race that hung the full test suite
in `command_test::command::merge_test::merge_rename_limit_reports_a_notice_and_still_merges`.
The `libra --json merge feature` child consumed a CPU continuously while its
parent waited for exit. Sampling the live child's hot thread resolved to
SQLx 0.9.0 `pool/inner.rs:548–549`, the maintenance task's
`for _ in 0..pool.num_idle()` loop and `try_acquire()` call.

SQLx's `PoolInner::release` pushes a connection and releases its semaphore
permit before incrementing `num_idle`. Another worker can pop the connection
and decrement the still-zero unsigned count. If maintenance snapshots that
transient underflow, its range has `usize::MAX` iterations. A failed
`try_acquire()` does not await or check cancellation inside that range, so
the task can prevent Tokio runtime shutdown even after the pool is closed.
The observed merge computation completed on copied repositories, including
under `--dry-run`; repeated ordinary runs did not reliably trigger the race.

Disabling **both** timers prevents SQLx from spawning this reaper. Disabling
only one leaves the other active. The regression
`internal::db::tests::sqlite_pools_disable_the_racy_reaper_and_preserve_single_connection`
checks the actual pools opened by both database factories and exercises
concurrent reads and explicit closure with a deadline. The reads exercise pool
acquisition and return without timing 128 FULL-synchronous disk commits; the
previous write-based test exceeded its deadline on a disk-backed `TMPDIR`
under full-suite load. Database creation still uses temporary files, and neither
factory migrates schema or opens real global/system configuration databases in
this test. The existing merge integration test covers the
original command workflow. A dependency upgrade should only remove this
workaround after verifying that the upstream idle-count publication race and
unbounded maintenance loop are fixed.

Libra depends on the published `libvault 0.4.0` crate. Keep the published
crate and this consumer on the same SQLx 0.9.0 pool lifecycle contract.
Build these two checkouts together. Before distributing a standalone Libra
Updating the SQLx version alone does not remove the race: 0.9.0 still needs
the timer guard.
