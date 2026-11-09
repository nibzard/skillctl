#!/usr/bin/env python3
"""Measure how long a spawned `skillctl install` holds the state lock.

`tests/cli/state_recovery.rs::a_second_process_waits_for_the_state_lock_and_
cannot_erase_committed_history` needs a child that parks on the coordination
lock. Two signals look equivalent but are not:

- the holder line in `~/.skillctl/state.db.lock.holder` appears at the child's
  first coordinated transaction, and
- the `after-state` step in the child's install journal appears when the child
  parks.

Between the two, the child releases the lock, so a second process can complete
a whole install in that gap. That gap is the root cause recorded in
notes/state-lock-test-timing.md. This probe measures it:

    python3 scripts/state-lock-timing-probe.py
    python3 scripts/state-lock-timing-probe.py --runs 20 --load

Unix only: the probe uses `fcntl.flock` to ask whether the lock is currently
held. It writes nothing outside a temporary directory it removes itself.
"""

from __future__ import annotations

import argparse
import fcntl
import json
import os
import pathlib
import shutil
import subprocess
import tempfile
import time

DEFAULT_RUNS = 5
REPOSITORY_ROOT = pathlib.Path(__file__).resolve().parent.parent
DEFAULT_BINARY = REPOSITORY_ROOT / "target" / "debug" / "skillctl"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--runs",
        type=int,
        default=DEFAULT_RUNS,
        help="number of installs to measure (default: %(default)s)",
    )
    parser.add_argument(
        "--load",
        action="store_true",
        help="saturate the CPU with two spinners per core while measuring",
    )
    parser.add_argument(
        "--binary",
        default=str(DEFAULT_BINARY),
        help="skillctl binary to spawn (default: %(default)s)",
    )
    return parser.parse_args()


def write_skill(source_root: pathlib.Path, name: str) -> None:
    skill_root = source_root / ".agents" / "skills" / name
    skill_root.mkdir(parents=True, exist_ok=True)
    (skill_root / "SKILL.md").write_text(
        f"---\nname: {name}\ndescription: Summarize release notes.\n---\n\n# {name}\n"
    )


def install_journals(state_root: pathlib.Path, pid: int) -> list[dict]:
    """Journal records of one process. Journals are written atomically."""
    records = []
    journal_root = state_root / "journal"
    if not journal_root.is_dir():
        return records
    for workspace in journal_root.iterdir():
        if not workspace.is_dir():
            continue
        for operation in workspace.iterdir():
            record_path = operation / "journal.json"
            if not record_path.is_file():
                continue
            try:
                record = json.loads(record_path.read_text(encoding="utf-8"))
            except (OSError, json.JSONDecodeError):
                continue
            if record.get("pid") == pid:
                records.append(record)
    return records


def parked_at_boundary(state_root: pathlib.Path, pid: int) -> bool:
    return any(
        record.get("operation") == "install" and "after-state" in record.get("steps", [])
        for record in install_journals(state_root, pid)
    )


def lock_is_free(lock_file: pathlib.Path) -> bool:
    """Whether another process could take the coordination lock right now."""
    try:
        handle = os.open(lock_file, os.O_RDWR | os.O_CREAT, 0o644)
    except OSError:
        return False
    try:
        fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
        fcntl.flock(handle, fcntl.LOCK_UN)
        return True
    except OSError:
        return False
    finally:
        os.close(handle)


def measure_once(binary: str) -> dict[str, object]:
    base = pathlib.Path(tempfile.mkdtemp(prefix="skillctl-lock-probe-"))
    try:
        home = base / "home"
        home.mkdir()
        workspace = base / "ws"
        workspace.mkdir()
        write_skill(workspace / "shared-skills", "release-notes")

        state_root = home / ".skillctl"
        lock_file = state_root / "state.db.lock"
        holder_file = state_root / "state.db.lock.holder"
        sentinel = base / "release-b"
        env = dict(os.environ)
        env.update(
            {
                "HOME": str(home),
                "SKILLCTL_LIFECYCLE_HOLD": str(sentinel),
                "SKILLCTL_LIFECYCLE_HOLD_AT": "install:after-state",
                "SKILLCTL_LIFECYCLE_HOLD_TIMEOUT_MS": "120000",
                "SKILLCTL_FAILPOINT": "install:after-state",
            }
        )

        started = time.perf_counter()
        child = subprocess.Popen(
            [str(pathlib.Path(binary).resolve()), "--no-input", "--name", "release-notes",
             "install", "shared-skills"],
            cwd=workspace,
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )

        holder_ms = None
        boundary_ms = None
        free_windows: list[tuple[float, float]] = []
        free_since = None

        def elapsed_ms() -> float:
            return (time.perf_counter() - started) * 1000

        while elapsed_ms() < 120_000:
            if holder_ms is None:
                try:
                    if f"pid={child.pid}" in holder_file.read_text(encoding="utf-8"):
                        holder_ms = elapsed_ms()
                except (FileNotFoundError, NotADirectoryError):
                    pass

            free = lock_is_free(lock_file)
            if free and free_since is None:
                free_since = elapsed_ms()
            elif not free and free_since is not None:
                free_windows.append((free_since, elapsed_ms()))
                free_since = None

            if boundary_ms is None and parked_at_boundary(state_root, child.pid):
                boundary_ms = elapsed_ms()

            if (holder_ms is not None and boundary_ms is not None) or child.poll() is not None:
                break
            time.sleep(0.002)

        if free_since is not None:
            free_windows.append((free_since, elapsed_ms()))
        sentinel.write_bytes(b"go")
        child.wait()

        # Only the windows between the two signals are the race the test had.
        gap_ms = sum(
            end - start
            for start, end in free_windows
            if holder_ms is not None and start >= holder_ms and boundary_ms is not None
        )
        return {
            "holder_ms": holder_ms,
            "boundary_ms": boundary_ms,
            "gap_ms": gap_ms,
            "exit_code": child.returncode,
        }
    finally:
        shutil.rmtree(base, ignore_errors=True)


def start_spinners(count: int) -> list[subprocess.Popen[bytes]]:
    return [
        subprocess.Popen(
            ["bash", "-c", "while :; do :; done"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        for _ in range(count)
    ]


def main() -> int:
    args = parse_args()
    if not pathlib.Path(args.binary).exists():
        print(f"binary not found: {args.binary} (build it first)", file=os.stderr)
        return 2

    spinners: list[subprocess.Popen[bytes]] = []
    if args.load:
        cores = os.cpu_count() or 1
        spinners = start_spinners(2 * cores)
        print(f"load: {len(spinners)} CPU spinners on {cores} cores")

    try:
        results = [measure_once(args.binary) for _ in range(args.runs)]
    finally:
        for spinner in spinners:
            spinner.terminate()
        for spinner in spinners:
            spinner.wait()

    for index, result in enumerate(results, start=1):
        holder = result["holder_ms"]
        boundary = result["boundary_ms"]
        holder_text = "never" if holder is None else f"{holder:7.1f} ms"
        boundary_text = "never" if boundary is None else f"{boundary:7.1f} ms"
        print(
            f"  run {index:>2}: holder line {holder_text}, boundary {boundary_text}, "
            f"lock free in the gap {result['gap_ms']:6.1f} ms, exit {result['exit_code']}"
        )

    gaps = [result["gap_ms"] for result in results]
    print(
        f"summary: {len(results)} runs; lock free between the two signals for "
        f"min {min(gaps):.1f} ms, mean {sum(gaps) / len(gaps):.1f} ms, max {max(gaps):.1f} ms"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
