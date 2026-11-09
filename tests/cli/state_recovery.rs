use super::*;

/// Boundaries a terminated operation can stop at. Each entry must leave the
/// old state, the new state, or an explicit recoverable operation behind.
const TERMINATION_BOUNDARIES: &[&str] = &[
    "install:opened",
    "install:after-state",
    "install:committing",
];

/// Run `skillctl` in one directory and return the parsed JSON payload.
fn run_json(home: &Path, workspace: &Path, args: &[&str]) -> (bool, Value) {
    let output = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace)
        .env("HOME", home)
        .args(["--json"])
        .args(args)
        .output()
        .expect("command launches");

    let payload: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "command {:?} produced invalid JSON: {error}; stdout={} stderr={}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });

    // Exit code 2 means success with warnings, so trust the payload's `ok`.
    let ok = payload["ok"].as_bool().unwrap_or(false);
    (ok, payload)
}

fn history_summaries(home: &Path, workspace: &Path) -> Vec<String> {
    let (ok, payload) = run_json(home, workspace, &["history"]);
    assert!(ok, "history command succeeds: {payload}");
    payload["data"]["entries"]
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .map(|entry| entry["summary"].as_str().unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default()
}

fn installed_skill_ids(home: &Path, workspace: &Path) -> Vec<String> {
    let (ok, payload) = run_json(home, workspace, &["list"]);
    assert!(ok, "list command succeeds: {payload}");
    payload["data"]["skills"]
        .as_array()
        .map(|skills| {
            skills
                .iter()
                .map(|skill| skill["skill"].as_str().unwrap_or_default().to_string())
                .filter(|skill| skill != BUNDLED_SKILL)
                .collect()
        })
        .unwrap_or_default()
}

/// Identifier of the bundled management skill, which every command installs.
const BUNDLED_SKILL: &str = "skillctl";

/// How long a direct database read waits for the held process to commit.
///
/// `rusqlite` waits five seconds by default. Under CPU saturation a starved
/// writer held its commit longer than that, and the read failed with
/// `database is locked`.
const READ_BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Read install rows straight from the shared database.
///
/// While one process holds the coordination lock, other `skillctl` processes
/// wait for it, so mid-flight assertions read the database directly. The
/// bundled management skill is excluded because it is not under test.
///
/// The direct read bypasses the coordination lock, so it can meet the held
/// process while that process commits. SQLite then keeps the reader out until
/// the commit ends. A busy timeout well above the writer's worst commit under
/// load keeps this an observation of in-flight state instead of a race with
/// it. See notes/state-lock-test-timing.md.
fn install_rows(home: &Path) -> Vec<String> {
    let database = home.join(".skillctl").join("state.db");
    let connection = Connection::open(database).expect("database opens");
    connection
        .busy_timeout(READ_BUSY_TIMEOUT)
        .expect("read busy timeout is set");
    let mut statement = connection
        .prepare("SELECT skill_id FROM install_records ORDER BY skill_id")
        .expect("install query prepares");
    statement
        .query_map([], |row| row.get::<_, String>(0))
        .expect("install query runs")
        .collect::<Result<Vec<_>, _>>()
        .expect("install rows read")
        .into_iter()
        .filter(|skill| skill != BUNDLED_SKILL)
        .collect()
}

fn install_skill(home: &Path, workspace: &Path, source: &str, skill: &str) {
    let (ok, payload) = run_json(
        home,
        workspace,
        &["--no-input", "--name", skill, "install", source],
    );
    assert!(ok, "install of {skill} succeeds: {payload}");
}

/// How long the two-process test waits for the held child to reach the
/// boundary it parks at.
///
/// The wait ends when the child reaches the boundary or when the child exits,
/// so this bound only stops a hung run. Keep it below
/// `HELD_HOLD_TIMEOUT_MS` so a slow child is reported by the wait, with
/// diagnostics, and not by its own hold timeout silently releasing the lock.
const HOLD_BOUNDARY_WAIT: std::time::Duration = std::time::Duration::from_secs(60);
/// Environment variable that raises `HOLD_BOUNDARY_WAIT` on slow machines.
const HOLD_BOUNDARY_WAIT_ENV: &str = "SKILLCTL_TEST_HOLD_WAIT_MS";
/// Pause between two reads of the child's journal.
const HOLD_BOUNDARY_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(20);
/// How long the held child waits at its boundary before it gives up.
///
/// This is a backstop against a hung test, not a timing tool. The child must
/// outwait every step the test runs while it holds the lock, and parallel
/// test load can starve this thread for seconds at a time, so the deadline
/// sits far beyond any scheduling delay. `HeldChild` reclaims the child on
/// every exit path, so a deadline this large cannot leak a process.
const HELD_HOLD_TIMEOUT_MS: u64 = 3_600_000;

/// Test-side bound for `wait_for_hold_boundary`.
fn hold_boundary_wait_bound() -> std::time::Duration {
    std::env::var(HOLD_BOUNDARY_WAIT_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map_or(HOLD_BOUNDARY_WAIT, std::time::Duration::from_millis)
}

/// Files that capture a held child's output so a failure can quote it.
struct HeldOutput {
    stdout: PathBuf,
    stderr: PathBuf,
}

impl HeldOutput {
    fn new(root: &Path) -> Self {
        Self {
            stdout: root.join("held-process.out"),
            stderr: root.join("held-process.err"),
        }
    }

    fn report(&self) -> String {
        format!(
            "held process stdout: {}; held process stderr: {}",
            read_capture(&self.stdout),
            read_capture(&self.stderr)
        )
    }
}

fn read_capture(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| format!("<unreadable: {error}>"))
}

/// Owned handle to a held child, so the test reclaims it on every path.
///
/// The child parks at its boundary until the sentinel appears or its own
/// hold deadline passes. Without an owner, a test that panics before the
/// sentinel leaks a child that still holds the coordination lock. Worse, a
/// starved test thread can outlast a short deadline: the child then releases
/// the lock mid-test, and process A's expected lock timeout turns into a
/// successful install. The guard closes both holes. Drop kills the child, so
/// only the sentinel or the test's own exit ends the hold, and
/// `HELD_HOLD_TIMEOUT_MS` is a backstop no scheduling delay can reach.
struct HeldChild {
    child: std::process::Child,
    sentinel: PathBuf,
    finished: bool,
}

impl HeldChild {
    fn new(child: std::process::Child, sentinel: PathBuf) -> Self {
        Self {
            child,
            sentinel,
            finished: false,
        }
    }

    /// Release the child at the sentinel and wait for it to finish.
    ///
    /// The child fails at its failpoint and rolls back, so this returns its
    /// failing status.
    fn release_and_wait(&mut self) -> std::process::ExitStatus {
        fs::write(&self.sentinel, b"go").expect("sentinel writes");
        let status = self.child.wait().expect("held command finishes");
        self.finished = true;
        status
    }
}

impl Drop for HeldChild {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Redirect one child stream into a file the test can quote on failure.
fn capture_stream(path: &Path) -> std::process::Stdio {
    let file = fs::File::create(path)
        .unwrap_or_else(|error| panic!("capture file {} is created: {error}", path.display()));
    file.into()
}

/// Wait until the held child parks at the boundary this test needs.
///
/// The lock file cannot prove that. One command runs several coordinated
/// transactions and releases the lock between them, so the holder line appears
/// long before the child parks, and a second process can complete a whole
/// install inside that gap. The journal proves the child reached the
/// `install:after-state` boundary, where it holds the lock without releasing
/// it until the test writes the sentinel.
///
/// The wait ends when the child reaches the boundary, when the child exits, or
/// when `bound` passes. Every early end reports the child status, its
/// journals, and its own output, so a failure on a loaded machine explains
/// itself instead of failing an opaque timing assertion.
fn wait_for_hold_boundary(
    child: &mut std::process::Child,
    state_root: &Path,
    lock_file: &Path,
    output: &HeldOutput,
    bound: std::time::Duration,
) {
    let started = std::time::Instant::now();
    loop {
        if install_journal_reached_after_state(state_root, child.id()) {
            let contents = fs::read_to_string(lock_file).unwrap_or_default();
            assert!(
                contents.contains(&format!("pid={}", child.id())),
                "the parked process must hold the lock: {contents}; {}",
                output.report()
            );
            return;
        }

        // A child that exited never reaches the boundary, so fail fast with
        // the child's own words instead of waiting out the bound.
        if let Some(status) = child.try_wait().expect("held child status is readable") {
            panic!(
                "the held process exited with {status} before it reached the \
                 install:after-state boundary; {}; {}",
                describe_journals(state_root, child.id()),
                output.report()
            );
        }

        if started.elapsed() >= bound {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "the held process did not reach the install:after-state boundary \
                 within {} ms; {}; {}",
                bound.as_millis(),
                describe_journals(state_root, child.id()),
                output.report()
            );
        }

        std::thread::sleep(HOLD_BOUNDARY_POLL_INTERVAL);
    }
}

/// Whether the install journal of one process reached the `after-state` step.
fn install_journal_reached_after_state(state_root: &Path, pid: u32) -> bool {
    journal_records(state_root, pid).into_iter().any(|record| {
        record["operation"] == "install"
            && steps_of(&record).iter().any(|step| step == "after-state")
    })
}

/// One line per journal of one process, so a failure shows how far it got.
fn describe_journals(state_root: &Path, pid: u32) -> String {
    let records = journal_records(state_root, pid);
    if records.is_empty() {
        return format!("no journal records pid {pid}");
    }
    records
        .into_iter()
        .map(|record| {
            format!(
                "journal operation={} state={} steps={:?}",
                record["operation"],
                record["state"],
                steps_of(&record)
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// Read the journal records of one process from `<state_root>/journal`.
///
/// Journals are written atomically, so a reader sees whole records only.
fn journal_records(state_root: &Path, pid: u32) -> Vec<Value> {
    let journal_root = state_root.join("journal");
    let Ok(workspaces) = fs::read_dir(&journal_root) else {
        return Vec::new();
    };

    let mut records = Vec::new();
    for workspace in workspaces.flatten() {
        let Ok(operations) = fs::read_dir(workspace.path()) else {
            continue;
        };
        for operation in operations.flatten() {
            let Ok(contents) = fs::read_to_string(operation.path().join("journal.json")) else {
                continue;
            };
            let Ok(record) = serde_json::from_str::<Value>(&contents) else {
                continue;
            };
            if record["pid"].as_u64() == Some(u64::from(pid)) {
                records.push(record);
            }
        }
    }
    records
}

fn steps_of(record: &Value) -> Vec<String> {
    record["steps"]
        .as_array()
        .map(|steps| {
            steps
                .iter()
                .filter_map(|step| step.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn terminated_operations_leave_an_explicit_recoverable_state() {
    for boundary in TERMINATION_BOUNDARIES {
        let workspace = TestWorkspace::new();
        let home = workspace.home_path();
        workspace.write_skill_source("shared-skills", "release-notes");
        install_skill(&home, workspace.path(), "shared-skills", "release-notes");

        let committed_history = history_summaries(&home, workspace.path());
        assert!(
            committed_history
                .iter()
                .any(|summary| summary.contains("release-notes")),
            "the committed history is recorded before the interruption: {committed_history:?}"
        );

        workspace.write_skill_source("shared-skills", "bug-triage");
        let terminated = Command::cargo_bin("skillctl")
            .expect("binary exists")
            .current_dir(workspace.path())
            .env("HOME", &home)
            .env("SKILLCTL_TERMINATE_AT", boundary)
            .args([
                "--no-input",
                "--name",
                "bug-triage",
                "install",
                "shared-skills",
            ])
            .output()
            .expect("terminated command launches");
        assert_eq!(
            terminated.status.code(),
            Some(70),
            "the child must stop at {boundary}"
        );

        // The interruption must leave either the old state or an explicit
        // recoverable operation, never a silently stranded database.
        let (ok, preview) = run_json(&home, workspace.path(), &["doctor", "--repair-state"]);
        assert!(ok, "repair preview succeeds at {boundary}: {preview}");
        let pending = preview["data"]["pending_operations"].as_array();
        assert_eq!(
            pending.map(Vec::len),
            Some(1),
            "one recoverable operation is reported at {boundary}: {preview}"
        );
        assert!(
            pending.is_some_and(|operations| operations[0]["operation"] == "install"),
            "the pending operation names its command: {preview}"
        );

        let (ok, applied) = run_json(
            &home,
            workspace.path(),
            &["doctor", "--repair-state", "--apply"],
        );
        assert!(ok, "repair apply succeeds at {boundary}: {applied}");
        assert!(
            !applied["data"]["repairs"]
                .as_array()
                .is_some_and(Vec::is_empty),
            "the repair reports what it restored at {boundary}: {applied}"
        );

        // The next ordinary command works and the committed history survives.
        assert_eq!(
            installed_skill_ids(&home, workspace.path()),
            vec!["release-notes".to_string()],
            "the interrupted install is undone at {boundary}"
        );
        assert!(
            history_summaries(&home, workspace.path())
                .iter()
                .any(|summary| summary.contains("release-notes")),
            "the committed history survives recovery at {boundary}"
        );

        let (ok, _) = run_json(&home, workspace.path(), &["doctor", "--repair-state"]);
        assert!(ok, "a follow-up preview succeeds at {boundary}");
        let (ok, payload) = run_json(&home, workspace.path(), &["doctor"]);
        assert!(ok, "doctor runs after recovery at {boundary}: {payload}");
    }
}

#[test]
fn a_second_process_waits_for_the_state_lock_and_cannot_erase_committed_history() {
    let shared = TestWorkspace::new();
    let home = shared.home_path();
    let workspace_a = shared.path().join("ws-a");
    let workspace_b = shared.path().join("ws-b");
    for workspace in [&workspace_a, &workspace_b] {
        fs::create_dir_all(workspace).expect("workspace exists");
        write_workspace_skill_at(
            workspace,
            "shared-skills/.agents/skills/release-notes",
            "release-notes",
        );
        write_workspace_skill_at(
            workspace,
            "shared-skills/.agents/skills/bug-triage",
            "bug-triage",
        );
    }

    // Process A commits history first.
    install_skill(&home, &workspace_a, "shared-skills", "release-notes");
    let committed_history = history_summaries(&home, &workspace_a);
    assert!(
        committed_history
            .iter()
            .any(|summary| summary.contains("release-notes")),
        "process A has committed history: {committed_history:?}"
    );

    // Process B holds the coordination lock mid-transaction.
    let sentinel = shared.path().join("release-b");
    let held_output = HeldOutput::new(shared.path());
    let binary = env!("CARGO_BIN_EXE_skillctl");
    let mut held = HeldChild::new(
        std::process::Command::new(binary)
            .current_dir(&workspace_b)
            .env("HOME", &home)
            .env("SKILLCTL_LIFECYCLE_HOLD", &sentinel)
            // Hold after the install wrote its state but before it rolls back, which
            // is the window where an uncoordinated second writer would be erased.
            .env("SKILLCTL_LIFECYCLE_HOLD_AT", "install:after-state")
            // Pin the hold so the child always outwaits this test, whatever the
            // machine does, and never releases the lock on its own.
            .env(
                "SKILLCTL_LIFECYCLE_HOLD_TIMEOUT_MS",
                HELD_HOLD_TIMEOUT_MS.to_string(),
            )
            .env("SKILLCTL_FAILPOINT", "install:after-state")
            .args([
                "--no-input",
                "--name",
                "release-notes",
                "install",
                "shared-skills",
            ])
            // Keep the child's words: a failure under load must explain itself.
            .stdout(capture_stream(&held_output.stdout))
            .stderr(capture_stream(&held_output.stderr))
            .spawn()
            .unwrap_or_else(|error| panic!("the held process spawns: {error}")),
        sentinel,
    );

    // Wait until process B parks at the boundary where it holds the lock. The
    // holder line alone is not enough: process B runs several coordinated
    // transactions and releases the lock between them, so a second process can
    // finish a whole install inside that gap.
    let state_root = home.join(".skillctl");
    let lock_file = state_root.join("state.db.lock");
    wait_for_hold_boundary(
        &mut held.child,
        &state_root,
        &lock_file,
        &held_output,
        hold_boundary_wait_bound(),
    );

    // Process A cannot write while B holds the lock, so A cannot commit state
    // that B would then erase during its rollback.
    let blocked = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(&workspace_a)
        .env("HOME", &home)
        .env("SKILLCTL_LOCK_TIMEOUT_MS", "1000")
        .args([
            "--no-input",
            "--name",
            "bug-triage",
            "install",
            "shared-skills",
        ])
        .output()
        .expect("blocked command runs");
    let message = format!(
        "{}{}",
        String::from_utf8_lossy(&blocked.stdout),
        String::from_utf8_lossy(&blocked.stderr)
    );
    assert!(
        !blocked.status.success(),
        "process A must not write while the lock is held: {message}"
    );
    assert!(
        message.contains("timed out") && message.contains("shared state lock"),
        "the lock timeout must be actionable: {message}"
    );

    let rows_while_blocked = install_rows(&home);
    assert!(
        !rows_while_blocked.contains(&"bug-triage".to_string()),
        "process A wrote nothing while it was blocked: {rows_while_blocked:?}"
    );

    // Release B, which fails and rolls back its own work.
    let status = held.release_and_wait();
    assert!(
        !status.success(),
        "process B fails at its failpoint: {status}; {}",
        held_output.report()
    );

    assert_eq!(
        install_rows(&home),
        vec!["release-notes".to_string()],
        "process B rolled its own install back and A's install survived"
    );
    assert!(
        history_summaries(&home, &workspace_a)
            .iter()
            .any(|summary| summary.contains("release-notes")),
        "the committed history is intact after the rollback"
    );

    // Once the lock is free, A can retry and succeed.
    install_skill(&home, &workspace_a, "shared-skills", "bug-triage");
    assert_eq!(
        installed_skill_ids(&home, &workspace_a),
        vec!["bug-triage".to_string(), "release-notes".to_string()],
        "process A completes its retried install"
    );
}

/// Marker summary an uncoordinated writer commits while a read command runs.
const WRITER_MARKER: &str = "uncoordinated writer committed this entry";
/// How long the uncoordinated writer holds its transaction open.
///
/// The hold must outlast the five-second busy timeout `rusqlite` sets by
/// default, so the read below fails if the store falls back to that default.
const WRITER_HOLD: std::time::Duration = std::time::Duration::from_secs(6);

/// Hold one exclusive write transaction on the shared database.
///
/// This is the state a starved writer leaves a reader in: the commit has not
/// ended, so SQLite keeps every other connection out. The connection lives in
/// this process, so the `skillctl` child it competes with is a separate
/// process and meets the lock. The coordination lock is not taken, because read
/// commands do not take it either.
fn hold_exclusive_write(database: &Path) -> Connection {
    let connection = Connection::open(database).expect("database opens");
    connection
        .busy_timeout(READ_BUSY_TIMEOUT)
        .expect("writer busy timeout is set");
    connection
        .execute_batch("BEGIN EXCLUSIVE;")
        .expect("writer takes the exclusive lock");
    connection
        .execute(
            "INSERT INTO history_events \
                 (workspace_key, kind, scope, skill_id, target, occurred_at, summary, \
                 details_json) \
             VALUES ('__global__', 'install', 'user', NULL, NULL, \
                 '2026-11-06T00:00:00Z', ?1, '{}')",
            params![WRITER_MARKER],
        )
        .expect("marker row writes");
    connection
}

/// A read command must wait for a committing writer instead of failing.
///
/// `list` and `history` open the store without the coordination lock, so only
/// the connection busy timeout decides whether they wait or fail with
/// `database is locked`. See notes/state-lock-test-timing.md.
#[test]
fn read_commands_wait_for_a_committing_writer_instead_of_failing() {
    let workspace = TestWorkspace::new();
    let home = workspace.home_path();
    workspace.write_skill_source("shared-skills", "release-notes");
    install_skill(&home, workspace.path(), "shared-skills", "release-notes");
    let database = home.join(".skillctl").join("state.db");

    let writer = hold_exclusive_write(&database);
    let started = std::time::Instant::now();
    let reader = std::process::Command::new(env!("CARGO_BIN_EXE_skillctl"))
        .current_dir(workspace.path())
        .env("HOME", &home)
        .args(["--json", "history"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("read command launches");

    // Keep the writer in flight for longer than the previous default, then let
    // it commit. A read that only tolerated five seconds fails before this.
    std::thread::sleep(WRITER_HOLD);
    writer.execute_batch("COMMIT;").expect("writer commits");
    drop(writer);

    let output = reader.wait_with_output().expect("read command finishes");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "history waits for the committing writer: {stdout}{stderr}"
    );
    assert!(
        started.elapsed() >= WRITER_HOLD,
        "the read stayed blocked for the whole hold, so it met the writer"
    );
    assert!(
        stdout.contains(WRITER_MARKER),
        "the read observes the row the writer committed: {stdout}"
    );
}

/// The read busy timeout is configurable, so a caller that prefers to fail fast
/// can shorten the wait.
#[test]
fn the_read_busy_timeout_is_configurable() {
    let workspace = TestWorkspace::new();
    let home = workspace.home_path();
    workspace.write_skill_source("shared-skills", "release-notes");
    install_skill(&home, workspace.path(), "shared-skills", "release-notes");
    let database = home.join(".skillctl").join("state.db");

    let writer = hold_exclusive_write(&database);
    let started = std::time::Instant::now();
    let mut reader = std::process::Command::new(env!("CARGO_BIN_EXE_skillctl"))
        .current_dir(workspace.path())
        .env("HOME", &home)
        .env("SKILLCTL_STATE_BUSY_TIMEOUT_MS", "250")
        .args(["--json", "history"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("read command launches");

    // The read must end on its own while the writer still holds the lock. Poll
    // instead of waiting, so a regression cannot hold the test hostage.
    let mut exited = None;
    let bound = std::time::Duration::from_secs(30);
    while started.elapsed() < bound {
        match reader.try_wait().expect("read status is readable") {
            Some(status) => {
                exited = Some(status);
                break;
            }
            None => std::thread::sleep(std::time::Duration::from_millis(20)),
        }
    }

    // Release the writer before asserting, so the child can never outlive it.
    writer.execute_batch("COMMIT;").expect("writer commits");
    drop(writer);

    let output = reader.wait_with_output().expect("read command drains");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let status = exited.unwrap_or_else(|| {
        panic!("the read ends within {bound:?} against a held writer: {stdout}{stderr}")
    });
    assert!(
        !status.success(),
        "the shortened timeout fails the read instead of waiting it out: {stdout}{stderr}"
    );
    let message = format!("{stdout}{stderr}");
    assert!(
        message.contains("database is locked"),
        "the failure names the lock it waited for: {message}"
    );
    assert!(
        message.contains("SKILLCTL_STATE_BUSY_TIMEOUT_MS"),
        "the failure names the wait control instead of raw SQLite text: {message}"
    );
    assert!(
        message.contains("SKILLCTL_LOCK_TIMEOUT_MS"),
        "the failure separates the coordination lock wait: {message}"
    );
}

/// A child that dies before it parks must be reported at once, with its own
/// output, instead of failing an opaque timing assertion later.
///
/// This pins the diagnostics that make a rare load-related failure of the
/// two-process test explainable. See notes/state-lock-test-timing.md.
#[test]
#[should_panic(expected = "before it reached the install:after-state boundary")]
fn a_held_process_that_exits_early_is_reported_with_its_output() {
    let workspace = TestWorkspace::new();
    let home = workspace.home_path();
    let output = HeldOutput::new(workspace.path());

    // `help` never opens the state store, so it never parks and exits on its
    // own.
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_skillctl"))
        .current_dir(workspace.path())
        .env("HOME", &home)
        .args(["help"])
        .stdout(capture_stream(&output.stdout))
        .stderr(capture_stream(&output.stderr))
        .spawn()
        .expect("short-lived command spawns");
    let status = child.wait().expect("short-lived command exits");
    assert!(status.success(), "help succeeds: {status}");

    // The captured streams must carry the child's words for the report to
    // quote them.
    assert!(
        output.report().contains("skillctl"),
        "the child output is captured: {}",
        output.report()
    );

    let state_root = home.join(".skillctl");
    let lock_file = state_root.join("state.db.lock");
    wait_for_hold_boundary(
        &mut child,
        &state_root,
        &lock_file,
        &output,
        std::time::Duration::from_secs(60),
    );
}

#[test]
fn stranded_state_is_reported_and_repaired_without_guessing() {
    let workspace = TestWorkspace::new();
    let home = workspace.home_path();
    workspace.write_skill_source("shared-skills", "release-notes");
    install_skill(&home, workspace.path(), "shared-skills", "release-notes");

    let database = home.join(".skillctl").join("state.db");

    // Ambiguous ownership: another workspace already claimed the same rows.
    {
        let connection = Connection::open(&database).expect("database opens");
        connection
            .execute_batch(
                "CREATE TABLE install_records_v1 (
                    scope TEXT NOT NULL, skill_id TEXT NOT NULL, source_kind TEXT NOT NULL,
                    source_url TEXT NOT NULL, source_subpath TEXT NOT NULL,
                    resolved_revision TEXT NOT NULL, upstream_revision TEXT, content_hash TEXT NOT NULL,
                    overlay_hash TEXT NOT NULL, effective_version_hash TEXT NOT NULL,
                    installed_at TEXT NOT NULL, updated_at TEXT NOT NULL, detached INTEGER NOT NULL,
                    forked INTEGER NOT NULL
                 );
                 INSERT INTO install_records_v1 VALUES
                    ('workspace', 'stranded', 'git', 'https://example.com/stranded.git',
                     'skills/stranded', 'dddddddddddddddd', NULL, 'sha256:content',
                     'sha256:none', 'sha256:effective-d', '2026-03-19T12:00:00Z',
                     '2026-03-19T12:00:00Z', 0, 0);
                 INSERT INTO install_records VALUES
                    ('workspace', 'another-workspace', 'claimed', 'git',
                     'https://example.com/claimed.git', 'skills/claimed',
                     'dddddddddddddddd', NULL, 'sha256:content', 'sha256:none',
                     'sha256:effective-d', '2026-03-19T12:00:00Z', '2026-03-19T12:00:00Z', 0, 0);",
            )
            .expect("ambiguous stranded state installs");
    }

    let blocked = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home)
        .args(["--json", "list"])
        .output()
        .expect("list launches");
    assert_eq!(
        blocked.status.code(),
        Some(3),
        "ambiguous stranded state must fail validation, not be guessed away"
    );
    let message = String::from_utf8_lossy(&blocked.stdout).into_owned();
    assert!(
        message.contains("owning workspace cannot be determined"),
        "the failure must be actionable: {message}"
    );
    assert!(
        message.contains("doctor --repair-state"),
        "the failure must name the recovery entry point: {message}"
    );

    // The preview reports the ambiguity without changing anything.
    let (ok, preview) = run_json(&home, workspace.path(), &["doctor", "--repair-state"]);
    assert!(ok, "preview succeeds: {preview}");
    let legacy = preview["data"]["legacy_tables"].as_array();
    assert!(
        legacy.is_some_and(|tables| {
            tables.iter().any(|table| {
                table["legacy"] == "install_records_v1" && table["action"] == "ambiguous"
            })
        }),
        "the ambiguous table is reported: {preview}"
    );
    assert!(
        !preview["data"]["unresolved"]
            .as_array()
            .is_some_and(Vec::is_empty),
        "the ambiguity is listed as unresolved: {preview}"
    );

    let (ok, applied) = run_json(
        &home,
        workspace.path(),
        &["doctor", "--repair-state", "--apply"],
    );
    assert!(ok, "apply succeeds without guessing: {applied}");
    assert!(
        !applied["data"]["unresolved"]
            .as_array()
            .is_some_and(Vec::is_empty),
        "the ambiguous table stays unresolved: {applied}"
    );
    let connection = Connection::open(&database).expect("database opens");
    let retained: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'install_records_v1'",
            [],
            |row| row.get(0),
        )
        .expect("table check runs");
    assert_eq!(retained, 1, "ambiguous rows are never deleted");
    drop(connection);

    // Unambiguous stranded rows are recovered by the same entry point.
    {
        let connection = Connection::open(&database).expect("database opens");
        connection
            .execute_batch(
                "DROP TABLE install_records_v1;
                 DELETE FROM install_records WHERE workspace_key = 'another-workspace';
                 CREATE TABLE install_records_v1 (
                    scope TEXT NOT NULL, skill_id TEXT NOT NULL, source_kind TEXT NOT NULL,
                    source_url TEXT NOT NULL, source_subpath TEXT NOT NULL,
                    resolved_revision TEXT NOT NULL, upstream_revision TEXT, content_hash TEXT NOT NULL,
                    overlay_hash TEXT NOT NULL, effective_version_hash TEXT NOT NULL,
                    installed_at TEXT NOT NULL, updated_at TEXT NOT NULL, detached INTEGER NOT NULL,
                    forked INTEGER NOT NULL
                 );
                 INSERT INTO install_records_v1 VALUES
                    ('workspace', 'stranded', 'git', 'https://example.com/stranded.git',
                     'skills/stranded', 'dddddddddddddddd', NULL, 'sha256:content',
                     'sha256:none', 'sha256:effective-d', '2026-03-19T12:00:00Z',
                     '2026-03-19T12:00:00Z', 0, 0);",
            )
            .expect("unambiguous stranded state installs");
    }

    let (ok, applied) = run_json(
        &home,
        workspace.path(),
        &["doctor", "--repair-state", "--apply"],
    );
    assert!(ok, "apply succeeds: {applied}");
    let skills = installed_skill_ids(&home, workspace.path());
    assert!(
        skills.contains(&"stranded".to_string()),
        "unambiguous rows are recovered: {skills:?}"
    );
    assert!(
        skills.contains(&"release-notes".to_string()),
        "existing rows survive the repair: {skills:?}"
    );
}

#[test]
fn doctor_reports_pending_recoverable_operations() {
    let workspace = TestWorkspace::new();
    let home = workspace.home_path();
    workspace.write_skill_source("shared-skills", "release-notes");

    let terminated = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home)
        .env("SKILLCTL_TERMINATE_AT", "install:after-state")
        .args([
            "--no-input",
            "--name",
            "release-notes",
            "install",
            "shared-skills",
        ])
        .output()
        .expect("terminated command launches");
    assert_eq!(terminated.status.code(), Some(70));

    let (ok, doctor) = run_json(&home, workspace.path(), &["doctor"]);
    assert!(ok, "doctor succeeds while recovery is pending: {doctor}");
    let issues = doctor["data"]["issues"].as_array();
    assert!(
        issues.is_some_and(|issues| issues
            .iter()
            .any(|issue| issue["code"] == "state-recovery-pending")),
        "doctor points at the pending recovery: {doctor}"
    );
}

/// Spawn one child that parks mid-install while it holds the coordination
/// lock, on the pattern of
/// `a_second_process_waits_for_the_state_lock_and_cannot_erase_committed_history`.
fn hold_install_mid_transaction(
    shared: &TestWorkspace,
    home: &Path,
    workspace: &Path,
) -> (HeldChild, PathBuf, PathBuf, HeldOutput) {
    let sentinel = shared.path().join("release-holder");
    let held_output = HeldOutput::new(shared.path());
    let binary = env!("CARGO_BIN_EXE_skillctl");
    let held = HeldChild::new(
        std::process::Command::new(binary)
            .current_dir(workspace)
            .env("HOME", home)
            .env("SKILLCTL_LIFECYCLE_HOLD", &sentinel)
            .env("SKILLCTL_LIFECYCLE_HOLD_AT", "install:after-state")
            .env(
                "SKILLCTL_LIFECYCLE_HOLD_TIMEOUT_MS",
                HELD_HOLD_TIMEOUT_MS.to_string(),
            )
            .env("SKILLCTL_FAILPOINT", "install:after-state")
            .args([
                "--no-input",
                "--name",
                "release-notes",
                "install",
                "shared-skills",
            ])
            .stdout(capture_stream(&held_output.stdout))
            .stderr(capture_stream(&held_output.stderr))
            .spawn()
            .unwrap_or_else(|error| panic!("the held process spawns: {error}")),
        sentinel,
    );

    let state_root = home.join(".skillctl");
    let lock_file = state_root.join("state.db.lock");
    (held, state_root, lock_file, held_output)
}

/// Run one `skillctl` command and return its combined output text.
fn run_text(home: &Path, workspace: &Path, envs: &[(&str, &str)], args: &[&str]) -> (bool, String) {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_skillctl"));
    command.current_dir(workspace).env("HOME", home);
    for (name, value) in envs {
        command.env(name, value);
    }
    let output = command.args(args).output().expect("command launches");
    let message = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (output.status.success(), message)
}

/// A live lifecycle transaction holds the coordination lock and keeps an
/// active journal on disk for its whole duration.
///
/// While one runs, a read command stays available instead of timing out on the
/// lock, `doctor` must not report the healthy operation as interrupted, and a
/// check-mode update must wait for the lock like every other writer instead of
/// committing beside the holder.
#[test]
fn a_live_lifecycle_writer_keeps_readers_available_and_is_not_reported_interrupted() {
    let shared = TestWorkspace::new();
    let home = shared.home_path();
    let workspace_a = shared.path().join("ws-a");
    let workspace_b = shared.path().join("ws-b");
    for workspace in [&workspace_a, &workspace_b] {
        fs::create_dir_all(workspace).expect("workspace exists");
    }
    write_workspace_skill_at(
        &workspace_b,
        "shared-skills/.agents/skills/release-notes",
        "release-notes",
    );

    // Workspace A tracks a git import, so a check-mode update has a row to
    // persist behind the lock.
    shared.write_skill_source("git-source", "release-notes");
    shared.init_git_repo("git-source");
    install_skill(
        &home,
        &workspace_a,
        &shared.git_repo_url("git-source"),
        "release-notes",
    );

    let (mut held, state_root, lock_file, held_output) =
        hold_install_mid_transaction(&shared, &home, &workspace_b);
    wait_for_hold_boundary(
        &mut held.child,
        &state_root,
        &lock_file,
        &held_output,
        hold_boundary_wait_bound(),
    );

    // A reader opens the store without the coordination lock, so even a one
    // second lock timeout cannot fail it while the writer is healthy.
    let (ok, message) = run_text(
        &home,
        &workspace_a,
        &[("SKILLCTL_LOCK_TIMEOUT_MS", "1000")],
        &["--json", "list"],
    );
    assert!(ok, "list stays available while the lock is held: {message}");

    // Doctor reads the journal directory without the lock, so it must tell the
    // live operation from an interrupted one instead of advising a repair.
    let (ok, doctor) = run_json(&home, &workspace_a, &["doctor"]);
    assert!(
        ok,
        "doctor succeeds while a lifecycle writer runs: {doctor}"
    );
    assert!(
        !doctor["data"]["issues"]
            .as_array()
            .is_some_and(|issues| issues
                .iter()
                .any(|issue| issue["code"] == "state-recovery-pending")),
        "a live operation is not an interrupted one: {doctor}"
    );
    let (ok, preview) = run_json(&home, &workspace_a, &["doctor", "--repair-state"]);
    assert!(ok, "repair preview succeeds: {preview}");
    assert_eq!(
        preview["data"]["pending_operations"]
            .as_array()
            .map(Vec::len),
        Some(0),
        "the live operation is not offered for repair: {preview}"
    );

    // A check-mode update persists its rows through the coordination lock, so
    // it waits for the live writer instead of committing beside it.
    let (ok, message) = run_text(
        &home,
        &workspace_a,
        &[("SKILLCTL_LOCK_TIMEOUT_MS", "1000")],
        &["--no-input", "update"],
    );
    assert!(
        !ok,
        "the check-mode update must not commit beside the lock holder: {message}"
    );
    assert!(
        message.contains("timed out") && message.contains("record update check"),
        "the refusal names the coordinated write it waited for: {message}"
    );

    // Release the holder, which fails at its failpoint and rolls back its own
    // work, so no journal survives for a repair to find.
    let status = held.release_and_wait();
    assert!(
        !status.success(),
        "the held process fails at its failpoint: {status}; {}",
        held_output.report()
    );
    let (ok, preview) = run_json(&home, &workspace_a, &["doctor", "--repair-state"]);
    assert!(ok, "a follow-up preview succeeds: {preview}");
    assert_eq!(
        preview["data"]["pending_operations"]
            .as_array()
            .map(Vec::len),
        Some(0),
        "the rolled-back operation left no journal: {preview}"
    );
}

/// `pin` clones its git source before the guarded transaction opens, so the
/// clone failure is reported even while another process holds the
/// coordination lock, instead of the command timing out on a lock it never
/// reached.
#[test]
fn pin_clones_the_pinned_source_before_it_waits_for_the_state_lock() {
    let shared = TestWorkspace::new();
    let home = shared.home_path();
    let workspace_a = shared.path().join("ws-a");
    let workspace_b = shared.path().join("ws-b");
    for workspace in [&workspace_a, &workspace_b] {
        fs::create_dir_all(workspace).expect("workspace exists");
    }
    write_workspace_skill_at(
        &workspace_b,
        "shared-skills/.agents/skills/release-notes",
        "release-notes",
    );

    shared.write_skill_source("git-source", "release-notes");
    shared.init_git_repo("git-source");
    install_skill(
        &home,
        &workspace_a,
        &shared.git_repo_url("git-source"),
        "release-notes",
    );
    // A second commit gives the pin a revision to move to.
    shared.write_skill_source_at(
        "git-source",
        ".agents/skills/release-notes",
        "release-notes",
        "Pinned release notes helper.",
    );
    shared.commit_all("git-source", "pin release notes");
    let pinned_commit = shared.git_head("git-source");

    let (mut held, state_root, lock_file, held_output) =
        hold_install_mid_transaction(&shared, &home, &workspace_b);
    wait_for_hold_boundary(
        &mut held.child,
        &state_root,
        &lock_file,
        &held_output,
        hold_boundary_wait_bound(),
    );

    // The pinned source disappears, so the pin's clone fails fast no matter
    // where it runs. A clone inside the guarded transaction would instead
    // report the lock timeout, because the holder never releases.
    let source = shared.path().join("git-source");
    let hidden = shared.path().join("git-source-hidden");
    fs::rename(&source, &hidden).expect("git source hides");
    let (ok, message) = run_text(
        &home,
        &workspace_a,
        &[("SKILLCTL_LOCK_TIMEOUT_MS", "1000")],
        &["--no-input", "pin", "release-notes", pinned_commit.as_str()],
    );
    assert!(!ok, "the pin fails without its source: {message}");
    assert!(
        message.contains("clone pinned git source"),
        "the pin reports the clone it ran before the lock: {message}"
    );
    assert!(
        !message.contains("timed out"),
        "the pin never spent its lock timeout waiting to clone: {message}"
    );

    // Restore the source, release the holder, and the same pin succeeds.
    fs::rename(&hidden, &source).expect("git source returns");
    let status = held.release_and_wait();
    assert!(
        !status.success(),
        "the held process fails at its failpoint: {status}; {}",
        held_output.report()
    );
    let (ok, payload) = run_json(
        &home,
        &workspace_a,
        &["--no-input", "pin", "release-notes", pinned_commit.as_str()],
    );
    assert!(ok, "the pin succeeds once the source is back: {payload}");
    assert_eq!(
        payload["data"]["resolved_revision"], pinned_commit,
        "the pin moved to the second commit: {payload}"
    );
}

/// Write one skill into a directory that lives outside the test workspace root.
fn write_workspace_skill_at(root: &Path, relative: &str, skill_name: &str) {
    let skill_root = root.join(relative);
    fs::create_dir_all(&skill_root).expect("skill root exists");
    fs::write(
        skill_root.join("SKILL.md"),
        format!(
            "---\nname: {skill_name}\ndescription: Summarize release notes.\n---\n\n# {skill_name}\n"
        ),
    )
    .expect("skill manifest exists");
}

#[test]
fn interrupted_migrations_keep_the_previous_schema_and_rows() {
    let boundary = "after-data-copies";
    let workspace = TestWorkspace::new();
    let home = workspace.home_path();
    let database = home.join(".skillctl").join("state.db");
    let legacy_root = home.join(".skillctl");
    fs::create_dir_all(&legacy_root).expect("state root exists");

    {
        let connection = Connection::open(&database).expect("legacy database opens");
        connection
            .execute_batch(
                "CREATE TABLE install_records (
                    scope TEXT NOT NULL, skill_id TEXT NOT NULL, source_kind TEXT NOT NULL,
                    source_url TEXT NOT NULL, source_subpath TEXT NOT NULL,
                    resolved_revision TEXT NOT NULL, upstream_revision TEXT, content_hash TEXT NOT NULL,
                    overlay_hash TEXT NOT NULL, effective_version_hash TEXT NOT NULL,
                    installed_at TEXT NOT NULL, updated_at TEXT NOT NULL, detached INTEGER NOT NULL,
                    forked INTEGER NOT NULL
                 );
                 INSERT INTO install_records VALUES
                    ('workspace', 'release-notes', 'git', 'https://example.com/release-notes.git',
                     'skills/release-notes', 'aaaaaaaaaaaaaaaa', NULL, 'sha256:content',
                     'sha256:none', 'sha256:effective-a', '2026-03-19T12:00:00Z',
                     '2026-03-19T12:00:00Z', 0, 0);
                 PRAGMA user_version = 1;",
            )
            .expect("legacy schema installs");
    }

    let interrupted = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home)
        .env("SKILLCTL_MIGRATION_FAILPOINT", boundary)
        .args(["--json", "list"])
        .output()
        .expect("interrupted migration launches");
    assert!(
        !interrupted.status.success(),
        "an interrupted migration must fail visibly"
    );
    let message = format!(
        "{}{}",
        String::from_utf8_lossy(&interrupted.stdout),
        String::from_utf8_lossy(&interrupted.stderr)
    );
    assert!(
        message.contains("injected migration failure"),
        "the injected failure surfaces: {message}"
    );

    // The interrupted migration must leave the previous schema and its rows.
    {
        let connection = Connection::open(&database).expect("database opens");
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("version reads");
        assert_eq!(version, 1, "the version marker must not advance on failure");
        let rows: i64 = connection
            .query_row("SELECT COUNT(*) FROM install_records", [], |row| row.get(0))
            .expect("rows count");
        assert_eq!(rows, 1, "legacy rows survive the interruption");
        let legacy_named: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'install_records_v1'",
                [],
                |row| row.get(0),
            )
            .expect("renamed tables check");
        assert_eq!(
            legacy_named, 0,
            "an uncommitted rename must not persist as a stranded table"
        );
    }

    // The next ordinary command completes the migration and keeps the rows.
    let (ok, payload) = run_json(&home, workspace.path(), &["list"]);
    assert!(ok, "the retry succeeds: {payload}");
    assert!(
        installed_skill_ids(&home, workspace.path()).contains(&"release-notes".to_string()),
        "migrated rows are attributed to this workspace: {payload}"
    );
}

/// Run one `skillctl` command and return its exit code with its full output.
fn run_command(home: &Path, workspace: &Path, args: &[&str]) -> (Option<i32>, String) {
    let output = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace)
        .env("HOME", home)
        .args(args)
        .output()
        .expect("command launches");

    (
        output.status.code(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    )
}

/// Paths of the quarantined copies a repair left beside the state database.
fn quarantined_databases(state_root: &Path) -> Vec<PathBuf> {
    let mut moved: Vec<PathBuf> = fs::read_dir(state_root)
        .expect("state root lists")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("state.db.corrupt."))
        })
        .collect();
    moved.sort();
    moved
}

#[test]
fn a_corrupt_state_database_is_reported_and_repaired_by_repair_state() {
    let workspace = TestWorkspace::new();
    let home = workspace.home_path();
    workspace.write_skill_source("shared-skills", "release-notes");
    install_skill(&home, workspace.path(), "shared-skills", "release-notes");

    let state_root = home.join(".skillctl");
    let database = state_root.join("state.db");
    fs::write(&database, "garbage\n").expect("corrupt database writes");

    // Every state-backed command that cannot answer without local state fails
    // with the same actionable message instead of the raw SQLite text. Plain
    // `doctor` is not in this list: it keeps answering, as its own test below
    // records.
    for command in ["list", "history"] {
        let (code, output) = run_command(&home, workspace.path(), &[command]);
        assert_eq!(code, Some(1), "{command} fails: {output}");
        assert!(
            output.contains("is not a readable SQLite database"),
            "{command} names the corrupt state database: {output}"
        );
        assert!(
            output.contains("doctor --repair-state --apply"),
            "{command} names the recovery entry point: {output}"
        );
        assert!(
            output.contains(".agents/skillctl.yaml"),
            "{command} names what local state is rebuilt from: {output}"
        );
    }

    // `restore --frozen` journals a database snapshot first. The backup API
    // answers a foreign file with a meaningless `not an error`, so that text
    // must never reach the user.
    let (code, output) = run_command(&home, workspace.path(), &["restore", "--frozen"]);
    assert_eq!(code, Some(1), "restore fails: {output}");
    assert!(
        !output.contains("not an error"),
        "the misleading backup failure must be gone: {output}"
    );
    assert!(
        output.contains("is not a readable SQLite database"),
        "restore names the corrupt state database: {output}"
    );

    // Dependency state stays intact: `verify` reads the manifest, the lockfile,
    // and the import cache, not the local state store.
    let (code, output) = run_command(&home, workspace.path(), &["verify"]);
    assert_ne!(
        code,
        Some(1),
        "verify keeps working while local state is corrupt: {output}"
    );
    assert!(
        !output.contains("state store"),
        "verify does not touch the state store: {output}"
    );

    // The preview reports the finding and changes nothing.
    let (ok, preview) = run_json(&home, workspace.path(), &["doctor", "--repair-state"]);
    assert!(ok, "the recovery entry point still answers: {preview}");
    assert_eq!(
        preview["data"]["state_database_unreadable"], "file is not a database",
        "the preview names the reason: {preview}"
    );
    assert!(
        preview["data"]["repairs"]
            .as_array()
            .is_some_and(Vec::is_empty),
        "the preview repairs nothing: {preview}"
    );
    assert_eq!(
        fs::read_to_string(&database).expect("the corrupt file survives the preview"),
        "garbage\n"
    );

    // Apply moves the corrupt file aside and rebuilds an empty store.
    let (ok, applied) = run_json(
        &home,
        workspace.path(),
        &["doctor", "--repair-state", "--apply"],
    );
    assert!(ok, "apply succeeds: {applied}");
    assert!(
        applied["data"]["repairs"]
            .as_array()
            .is_some_and(|repairs| repairs.iter().any(|repair| repair
                .as_str()
                .is_some_and(|repair| repair.contains("moved the unreadable state database")))),
        "the repair names what it did: {applied}"
    );
    let moved = quarantined_databases(&state_root);
    assert_eq!(moved.len(), 1, "exactly one quarantine is kept: {moved:?}");
    assert_eq!(
        fs::read_to_string(&moved[0]).expect("the corrupt file is kept"),
        "garbage\n",
        "the quarantine preserves the original bytes"
    );
    assert!(
        is_sqlite_database(&database),
        "a fresh state database is rebuilt"
    );

    // The next command works, and the lockfile still describes the import.
    let (ok, payload) = run_json(&home, workspace.path(), &["list"]);
    assert!(ok, "state-backed commands work after the repair: {payload}");
    let (ok, payload) = run_json(&home, workspace.path(), &["doctor"]);
    assert!(ok, "doctor works after the repair: {payload}");

    let (ok, payload) = run_json(&home, workspace.path(), &["restore", "--frozen"]);
    assert!(ok, "the documented rebuild of imports succeeds: {payload}");
    assert!(
        installed_skill_ids(&home, workspace.path()).contains(&"release-notes".to_string()),
        "restore --frozen rebuilds the import records from the lockfile"
    );
}

/// Plain `doctor` is the command a user reaches for when something is broken,
/// so an unreadable state database must not end the report. It degrades to a
/// partial report whose own issue names the recovery.
#[test]
fn plain_doctor_reports_an_unreadable_state_database_as_an_issue() {
    let workspace = TestWorkspace::new();
    let home = workspace.home_path();
    workspace.write_skill_source("shared-skills", "release-notes");
    install_skill(&home, workspace.path(), "shared-skills", "release-notes");

    let database = home.join(".skillctl").join("state.db");
    fs::write(&database, "garbage\n").expect("corrupt database writes");

    // Human output still renders a report, not the raw open failure.
    let (code, output) = run_command(&home, workspace.path(), &["doctor"]);
    assert_eq!(
        code,
        Some(3),
        "doctor exits 3 with a rendered report, not 1 with an open failure: {output}"
    );
    assert!(
        output.contains("is not a readable SQLite database"),
        "doctor names the corrupt state database: {output}"
    );
    assert!(
        output.contains("doctor --repair-state --apply"),
        "doctor names the recovery entry point: {output}"
    );
    assert!(
        output.contains(".agents/skillctl.yaml"),
        "doctor names what local state is rebuilt from: {output}"
    );

    // The report is partial, not empty: the workspace skill is still checked
    // and the unreadable database is one error issue.
    let (_, payload) = run_json(&home, workspace.path(), &["doctor"]);
    let issues = payload["data"]["issues"].as_array();
    let unreadable = issues
        .into_iter()
        .flatten()
        .find(|issue| issue["code"] == "state-database-unreadable" && issue["severity"] == "error");
    assert!(
        unreadable.is_some(),
        "doctor reports the unreadable database as an issue: {payload}"
    );
    assert_eq!(
        payload["data"]["summary"]["checked_skill_count"],
        serde_json::json!(1),
        "the workspace skill is still checked: {payload}"
    );
    assert_eq!(
        payload["data"]["summary"]["error_count"],
        serde_json::json!(1),
        "only the unreadable database is an error: {payload}"
    );

    // The repair entry point keeps answering, and doctor works after it.
    let (ok, preview) = run_json(&home, workspace.path(), &["doctor", "--repair-state"]);
    assert!(ok, "the recovery preview still answers: {preview}");
    assert_eq!(
        preview["data"]["state_database_unreadable"], "file is not a database",
        "the preview names the reason: {preview}"
    );
    let (ok, applied) = run_json(
        &home,
        workspace.path(),
        &["doctor", "--repair-state", "--apply"],
    );
    assert!(ok, "apply repairs the database: {applied}");
    let (ok, payload) = run_json(&home, workspace.path(), &["doctor"]);
    assert!(
        ok,
        "doctor answers without issues after the repair: {payload}"
    );
    assert!(
        payload["data"]["issues"]
            .as_array()
            .is_some_and(|issues| !issues
                .iter()
                .any(|issue| issue["code"] == "state-database-unreadable")),
        "the unreadable-database issue is gone after the repair: {payload}"
    );
}

#[test]
fn moving_a_corrupt_state_database_aside_rebuilds_local_state() {
    let workspace = TestWorkspace::new();
    let home = workspace.home_path();
    workspace.write_skill_source("shared-skills", "release-notes");
    install_skill(&home, workspace.path(), "shared-skills", "release-notes");

    let state_root = home.join(".skillctl");
    let database = state_root.join("state.db");
    fs::write(&database, "garbage\n").expect("corrupt database writes");

    // The documented manual recovery: move the file aside yourself.
    let aside = state_root.join("state.db.corrupt");
    fs::rename(&database, &aside).expect("the corrupt file moves aside");

    let (ok, payload) = run_json(&home, workspace.path(), &["list"]);
    assert!(ok, "the next command rebuilds local state: {payload}");
    assert!(
        is_sqlite_database(&database),
        "a fresh state database is created"
    );
    assert!(
        quarantined_databases(&state_root).is_empty(),
        "a manual recovery leaves no automatic quarantine"
    );

    let (ok, payload) = run_json(&home, workspace.path(), &["restore", "--frozen"]);
    assert!(
        ok,
        "the import records rebuild from the lockfile: {payload}"
    );
    assert!(
        installed_skill_ids(&home, workspace.path()).contains(&"release-notes".to_string()),
        "the lockfile still describes the managed import"
    );
}

#[test]
fn a_corrupt_state_database_does_not_strand_a_pending_journal() {
    let workspace = TestWorkspace::new();
    let home = workspace.home_path();
    workspace.write_skill_source("shared-skills", "release-notes");
    install_skill(&home, workspace.path(), "shared-skills", "release-notes");
    workspace.write_skill_source("other-skills", "bug-triage");

    let state_root = home.join(".skillctl");
    let database = state_root.join("state.db");

    // Leave one interrupted install behind, then corrupt the database it
    // snapshotted.
    let terminated = Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", &home)
        .env("SKILLCTL_TERMINATE_AT", "install:after-state")
        .args([
            "--no-input",
            "--name",
            "bug-triage",
            "install",
            "other-skills",
        ])
        .output()
        .expect("terminated command launches");
    assert_eq!(terminated.status.code(), Some(70));
    fs::write(&database, "garbage\n").expect("corrupt database writes");

    // A lifecycle command must fail on the corrupt file without touching the
    // journal, so the snapshot it holds stays usable.
    let (code, output) = run_command(&home, workspace.path(), &["restore", "--frozen"]);
    assert_eq!(code, Some(1), "restore fails: {output}");
    assert!(
        !output.contains("not an error"),
        "the misleading backup failure must be gone: {output}"
    );
    let journal_states = pending_journal_states(&state_root);
    assert_eq!(
        journal_states,
        vec!["active".to_string()],
        "the journal must not be stranded in needs-review: {journal_states:?}"
    );

    // The repair quarantines the file first, so the journal restores its
    // snapshot into the rebuilt store instead of being stranded.
    let (ok, applied) = run_json(
        &home,
        workspace.path(),
        &["doctor", "--repair-state", "--apply"],
    );
    assert!(ok, "apply succeeds: {applied}");
    assert!(
        applied["data"]["unresolved"]
            .as_array()
            .is_some_and(Vec::is_empty),
        "nothing stays unresolved: {applied}"
    );
    assert!(
        applied["data"]["repairs"]
            .as_array()
            .is_some_and(|repairs| repairs.iter().any(|repair| {
                repair
                    .as_str()
                    .is_some_and(|repair| repair.contains("restored the pre-operation state"))
            })),
        "the journal is recovered after the quarantine: {applied}"
    );
    assert!(
        pending_journal_states(&state_root).is_empty(),
        "the recovered journal is closed"
    );
    assert_eq!(
        quarantined_databases(&state_root).len(),
        1,
        "the corrupt file is kept"
    );

    // The snapshot carried the committed install, so it survived the repair.
    assert_eq!(
        installed_skill_ids(&home, workspace.path()),
        vec!["release-notes".to_string()],
        "the interrupted install is undone and the committed install survives"
    );
}

/// States of the journals still waiting under one state root.
fn pending_journal_states(state_root: &Path) -> Vec<String> {
    let mut states: Vec<String> = Vec::new();
    let journal_root = state_root.join("journal");
    let workspaces = match fs::read_dir(&journal_root) {
        Ok(entries) => entries,
        Err(_) => return states,
    }
    .filter_map(Result::ok)
    .map(|entry| entry.path())
    .filter(|path| path.is_dir());

    for workspace in workspaces {
        let journals = match fs::read_dir(&workspace) {
            Ok(entries) => entries,
            Err(_) => continue,
        }
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.join("journal.json").is_file());

        for journal in journals {
            let contents =
                fs::read_to_string(journal.join("journal.json")).expect("journal record reads");
            let record: Value = serde_json::from_str(&contents).expect("journal record parses");
            if let Some(state) = record.get("state").and_then(|state| state.as_str()) {
                states.push(state.to_string());
            }
        }
    }

    states.sort();
    states
}
