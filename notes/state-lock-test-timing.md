# Note: timing sensitivity of the state-lock CLI test

Investigation date: 2026-11-05
Task: T070 (`looper:finding:state-lock-test-timing-sensitivity`)

## What was reported

The test `a_second_process_waits_for_the_state_lock_and_cannot_erase_committed_history`
in `tests/cli/state_recovery.rs` failed once in a full `cargo test --locked`
run on 2026-11-05 while a parallel release build loaded the machine. Two
re-runs of the full suite, and one run of the test alone, passed without a code
change. The output of the failing run was not kept, so its failing assertion is
unknown. The investigation reproduced the failure, found two causes, and fixed
both in the test.

## How it was reproduced

The test ran repeatedly while CPU spinners, and sometimes a parallel release
build, loaded an 8-core machine. The full `cli` suite also ran under the same
load. Both failure modes below appeared within a few iterations.

## Root cause 1: the holder line is not the park

The test waited for `~/.skillctl/state.db.lock` to name the child pid, then
declared the lock held and started a second install that must fail.

That signal is wrong. One `skillctl install` runs several coordinated
transactions: the bundled-skill bootstrap first, then the install transaction
(`src/runtime.rs` calls `ensure_bundled_skill` before `dispatch`). Each
transaction takes and releases the coordination lock, and the holder line is
written by the first one.

Measured with `scripts/state-lock-timing-probe.py`:

| Machine state | Holder line | Install `after-state` boundary |
| --- | --- | --- |
| Idle | 4 to 18 ms | 530 to 700 ms |
| 28 CPU spinners on 8 cores | 14 to 43 ms | 3495 to 21189 ms |

The lock is acquirable between the two signals: 2 to 4 ms when idle, and an
earlier coarser probe that sampled every 2 ms saw two free windows totalling
26 ms before the boundary. The test therefore started process A during a
window in which process B did not hold the lock. Process A finished a whole
install inside that window and the assertion `process A must not write while
the lock is held` failed:

```text
process A must not write while the lock is held:
Installed 1 skill (bug-triage) into workspace scope.
```

Two of twelve runs failed this way under ten spinners. The window is narrow on
an idle machine and stretches with load, which is why the failure appeared only
under a parallel build.

Fix: the test now waits for the child's install journal to record the
`after-state` step (`<state_root>/journal/<workspace>/<operation>/journal.json`).
`checkpoint` records that step immediately before it parks, and while the child
parks it holds the lock without releasing it. Journals are written atomically,
so the test reads whole records.

## Root cause 2: the direct database read races the writer's commit

Mid-flight assertions read `install_records` straight from SQLite, because a
`skillctl` command would wait for the lock instead. That read bypasses the
coordination lock, and while the held process commits, SQLite keeps readers
out. `rusqlite` 0.32.1 waits five seconds by default
(`sqlite3_busy_timeout(db, 5000)`).

Measured by reading the database in a loop against a working install:

- 29862 reads over 12 seconds on an idle machine; the longest wait was
  1638 ms; no read failed.
- Under CPU saturation the writer's commit outlasted the five-second default
  and the read failed:

```text
install query prepares: SqliteFailure(Error { code: DatabaseBusy,
extended_code: 5 }, Some("database is locked"))
```

One of eight runs failed this way under eight spinners. An earlier loop of ten
runs under ten spinners failed twice more before any fix, but kept no logs, so
those two runs cannot be attributed to one cause.

Fix: `install_rows` now sets a 60-second busy timeout, so the read observes
in-flight state instead of racing the commit.

## What was not the cause

- The 10-second poll deadline. The holder line appeared within 4 to 49 ms even
  under saturation, about 200 times inside the deadline. The deadline was
  still worth removing: when it tripped, the assertion printed nothing, and
  the child's output went to `/dev/null`, so the failure could not be
  explained afterwards.
- T043 `discover`. `skillctl --json discover` against an unreachable origin
  creates no `~/.skillctl` directory, so it never touches the coordination
  lock.
- Cross-test interference. Every test gets its own `HOME`, and the state store
  and its lock live under `$HOME/.skillctl` (`src/state.rs`,
  `default_state_root`), so no two tests share a lock.
- Process A's 1-second lock timeout. The child holds the lock until the test
  writes the sentinel, so that timeout can only end one way.

## Changes made

`tests/cli/state_recovery.rs`:

- `wait_for_hold_boundary` replaces the holder-line poll. It ends when the
  child's install journal records `after-state`, when the child exits, or when
  a 60-second bound passes. `SKILLCTL_TEST_HOLD_WAIT_MS` raises the bound on
  slow runners. The bound stays below the child's own hold timeout.
- The held child sets `SKILLCTL_LIFECYCLE_HOLD_TIMEOUT_MS` explicitly, so it
  always outwaits the test and never releases the lock on its own.
- The child's stdout and stderr are captured in the test workspace, and every
  failure path quotes them together with the child's journals.
- `install_rows` sets a 60-second busy timeout on its direct read.
- `a_held_process_that_exits_early_is_reported_with_its_output` pins the new
  diagnostics.

`scripts/state-lock-timing-probe.py` measures the gap between the two signals.
Run it before changing the wait or the hold constants.

`notes/state-lock-test-timing.md` is this note.

## Validation

- 12 of 12 runs of the test under 10 CPU spinners. The same loop failed 2 of
  12 runs before the fix.
- 12 of 12 runs under 14 spinners plus a parallel release build, with a load
  average of 42 on 8 cores.
- The full `cli` suite, 157 tests, passed under that load in 42.8 seconds.
- Full gates on an idle machine: `cargo fmt --check`,
  `cargo clippy --all-targets --all-features --locked -- -D warnings`,
  `cargo test --locked`, and `cargo doc --locked --no-deps`.

## Product observations

Both observations were recorded here on 2026-11-05 and are tracked as tasks:

- Read commands such as `list` and `history` open the store without the
  coordination lock. A user can meet the same `database is locked` error while
  another process commits, because the five-second default can be outlasted by
  a starved writer. Task T079 tracks this and resolved it on 2026-11-06; see
  the next section.
- One command releases the coordination lock between its transactions, so a
  second process can write inside one command. Task T066 tracks the lock
  granularity decision.

## Task T079: read commands and the busy timeout

Date: 2026-11-06

The product fix raises the SQLite busy timeout of every state connection from
the `rusqlite` default of five seconds to sixty seconds
(`src/state.rs`, `configure_connection`). `SKILLCTL_STATE_BUSY_TIMEOUT_MS`
overrides it, mirroring `SKILLCTL_LOCK_TIMEOUT_MS` for the coordination lock.

Why sixty seconds:

- The worst observed wait for one read against a committing install was
  1638 ms on an idle machine, and one read failed after the five-second
  default under eight CPU spinners.
- The worst observed time to reach an install boundary under a load average of
  42 was 21 seconds, so sixty seconds keeps roughly three times headroom.
- Reads stay short in practice: a held transaction parks between its SQLite
  transactions, not inside one, so a reader waits out one commit and not the
  whole hold.

Tests:

- `read_commands_wait_for_a_committing_writer_instead_of_failing` in
  `tests/cli/state_recovery.rs` holds one exclusive write transaction for six
  seconds, longer than the old default, and proves `skillctl history` waits and
  then observes the committed row. It fails against the five-second default.
- `the_read_busy_timeout_is_configurable` proves the environment override
  reaches the read connection.
- `state_connections_wait_longer_than_the_default_busy_timeout` in
  `src/state/tests.rs` pins the timeout on both store open paths.

### Why not WAL mode

Enabling write-ahead logging would let readers proceed without waiting, but it
was rejected for now:

- `record_migration_backup` in `src/state/journal.rs` copies `state.db` with a
  raw file copy. In WAL mode recent commits can still live only in
  `state.db-wal`, so the migration safety net would silently miss them.
- Every connection, including read commands, would need to create and write the
  `-wal` and `-shm` sidecar files. That fails on read-only media and on network
  filesystems, which do not support WAL shared memory. Some users keep `$HOME`
  on such a filesystem.
- The staged replacement, journal restore, and interruption tests were
  validated in rollback-journal mode, and Windows lock semantics are still
  unverified (T065).
- Writers already serialize through the coordination lock, so WAL only helps
  the uncoordinated read path, which the raised timeout already covers.

The busy timeout does not preclude WAL: task T066 should re-evaluate WAL when
lock granularity work resumes.

## Task T080: the same assertion under parallel test load

Date: 2026-11-06

Papercut `pc_01M6PP1QQASGK4VQ9JKTCSZW30` (logged 2026-11-05T20:22:49Z)
reported one failure of
`a_second_process_waits_for_the_state_lock_and_cannot_erase_committed_history`
at `tests/cli/state_recovery.rs:270` in a loaded parallel
`cargo test --test cli` run. The test passed in isolation, in three later
full-suite runs, and in three runs of the base commit.

The report's context SHA, `8cf5035` (`Integrate T043`), predates the T070 fix
by seven minutes. That tree still waited on the holder line, sent the child's
output to `/dev/null`, and left the hold at its 60-second default, so the
observed failure is root cause 1 above. The added parallel tests were the T043
`discover` suite. The journal-boundary wait that closed that window is already
in this branch.

T080 closes the window that remained: the hold still ended on a timer inside
the child.

- The test pinned the child's hold deadline to 120 seconds. The deadline is
  absolute from the park, so a test thread starved past it by parallel load
  lost the lock silently: the child timed out, rolled back, exited, and
  process A's expected `timed out` failure became a successful install. Same
  assertion, same message, still load-dependent.
- A test that panicked between the park and the sentinel also leaked the
  parked child. The harness deletes the workspace on drop but reclaims no
  children.

Fix in `tests/cli/state_recovery.rs`:

- `HeldChild` owns the child and its sentinel. `release_and_wait` writes the
  sentinel and reaps the child. `Drop` kills a child that did not finish, so
  every exit path, including a panic, reclaims it.
- `HELD_HOLD_TIMEOUT_MS` rises to 3,600,000, one hour. With the guard
  guaranteeing reclamation, the deadline is a hung-test backstop that no
  scheduling delay reaches. Inside the test the hold now ends only at the
  sentinel, which is the deterministic hold the papercut asked for. A longer
  `SKILLCTL_LOCK_TIMEOUT_MS` for process A was not needed: once the park is
  proven, the one-second timeout can only end one way.

Validation on 2026-11-06 (8 cores):

- 12 of 12 runs of the test alone under 16 CPU spinners, with the load
  average climbing from 3.0 to 22.7 and single runs stretching from 2 s to
  16 s.
- 3 of 3 runs of `cargo test --locked --test cli`, the papercut's command,
  under the same spinners: 228 tests passed each time at load averages of
  31.4 to 34.4, with a suite time of 102 to 132 s.
- Full gates on an idle machine: `cargo fmt --check`,
  `cargo clippy --all-targets --all-features --locked -- -D warnings`,
  `cargo test --locked`, and
  `RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps`.

## Task T096: the failure text when a read runs out of the busy timeout

Date: 2026-11-06

T079 fixed the failure, and this task fixed the message a read still meets
when it exhausts the wait. Reproduced on 2026-11-06 by reverting the default
to five seconds in `tests/cli/state_recovery.rs`: the read failed with
`failed to read schema version in local state store ...: database is locked`.
That text names no control, so a user who wanted a shorter or longer wait had
no pointer, and a user who reached for `SKILLCTL_LOCK_TIMEOUT_MS` changed the
wrong wait.

The fix maps `DatabaseBusy` from any state query to
`AppError::LocalStateBusyTimeout` (`src/error.rs`). The message states the
wait it used, keeps the SQLite text, and names both controls:

```text
timed out after 250 ms waiting to read schema version in local state store
'<home>/.skillctl/state.db': database is locked. Another process held the
database lock longer than the state busy timeout. Set
SKILLCTL_STATE_BUSY_TIMEOUT_MS to change how long this command waits.
SKILLCTL_LOCK_TIMEOUT_MS and the state coordination lock control a different
wait
```

Only `DatabaseBusy` is mapped. `DatabaseLocked` is a shared-cache lock inside
one connection, which a longer busy timeout does not survive, so it keeps the
plain query failure. The exit status stays `1`, matching
`AppError::LocalStateQuery` and `AppError::LocalStateLockTimeout`.

Tests:

- `the_read_busy_timeout_is_configurable` in `tests/cli/state_recovery.rs`
  now also requires both control names in the child's output.
- `a_busy_timeout_failure_names_the_wait_controls` in `src/state/tests.rs`
  takes one real `DatabaseBusy` error from a held writer and checks the
  mapping, the message, and the exit status.
- `other_query_failures_keep_the_plain_query_error` pins that only a busy
  lock rewrites the failure.

## How to re-measure

```bash
cargo build --locked
python3 scripts/state-lock-timing-probe.py
python3 scripts/state-lock-timing-probe.py --runs 10 --load
```

`--load` starts two CPU spinners per core and stops them afterwards. The probe
needs `fcntl`, so it runs on Unix only.

## Open questions

- Which assertion failed in the reported run is unknown. Both reproduced
  failure modes fit a single failure under load.
- The worst observed time to reach the boundary was 21 seconds under a load
  average of 42. The 60-second bound has about three times headroom over that;
  raise it with `SKILLCTL_TEST_HOLD_WAIT_MS` if a runner is slower still.
- Windows behavior of the lock and of these tests is unverified; T065 tracks it.

## References

- Test: `tests/cli/state_recovery.rs`
- Lock: `src/state/lock.rs`
- Hold and failpoint boundaries: `src/lifecycle.rs`
- Journal layout: `src/state/journal.rs`
- Related tasks: T065 (Windows validation), T066 (lock granularity)
