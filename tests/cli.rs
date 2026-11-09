use assert_cmd::Command;
use predicates::prelude::*;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::Command as ProcessCommand,
    sync::Arc,
    thread,
};

// The shared harness lives in tests/support. It is re-exported here so every
// `tests/cli/*` submodule reaches it through `use super::*`.
#[allow(dead_code)]
mod support;

pub use support::{
    MINIMAL_LOCKFILE, PathSnapshot, TestWorkspace, file_url, is_sqlite_database, portable,
    snapshot_path,
};

#[cfg(unix)]
#[path = "cli/broken_pipe.rs"]
mod broken_pipe;
#[path = "cli/bundled.rs"]
mod bundled;
#[path = "cli/cleanup.rs"]
mod cleanup;
#[path = "cli/diagnostics.rs"]
mod diagnostics;
#[path = "cli/discover.rs"]
mod discover;
#[path = "cli/drift.rs"]
mod drift;
#[path = "cli/help.rs"]
mod help;
#[path = "cli/human_output.rs"]
mod human_output;
#[path = "cli/import_repair.rs"]
mod import_repair;
#[path = "cli/init.rs"]
mod init;
#[path = "cli/install.rs"]
mod install;
#[path = "cli/inventory.rs"]
mod inventory;
#[path = "cli/local_source_apply.rs"]
mod local_source_apply;
#[path = "cli/override_cmd.rs"]
mod override_cmd;
#[path = "cli/projection.rs"]
mod projection;
#[path = "cli/restore.rs"]
mod restore;
#[path = "cli/review.rs"]
mod review;
#[path = "cli/source_env.rs"]
mod source_env;
#[path = "cli/state_recovery.rs"]
mod state_recovery;
#[path = "cli/trust_gate.rs"]
mod trust_gate;
#[path = "cli/update.rs"]
mod update;
#[path = "cli/update_apply.rs"]
mod update_apply;

/// State-store timestamps follow the real clock; `SOURCE_DATE_EPOCH` pins
/// projection metadata only. Tests assert the RFC 3339 UTC shape instead of
/// one frozen value.
fn assert_rfc3339_utc(value: &str) {
    let shape = value.len() == 20
        && b"####-##-##T##:##:##Z".iter().zip(value.as_bytes()).all(
            |(pattern, byte)| match pattern {
                b'#' => byte.is_ascii_digit(),
                _ => pattern == byte,
            },
        );
    assert!(shape, "expected an RFC 3339 UTC timestamp, got {value:?}");
}

fn initialize_runtime_state(workspace: &TestWorkspace) {
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .args(["--json", "telemetry", "status"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
}

fn assert_transaction_rolled_back(
    workspace: &TestWorkspace,
    failpoint: &str,
    source_date_epoch: &str,
    args: &[&str],
    expected: &BTreeMap<String, PathSnapshot>,
) {
    Command::cargo_bin("skillctl")
        .expect("binary exists")
        .current_dir(workspace.path())
        .env("HOME", workspace.home_path())
        .env("SOURCE_DATE_EPOCH", source_date_epoch)
        .env("SKILLCTL_FAILPOINT", failpoint)
        .args(args)
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("injected lifecycle failure"));

    let actual = workspace.snapshot_paths(&expected.keys().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(&actual, expected);
}

/// One response a local test origin serves.
struct Route {
    path: &'static str,
    status: u16,
    body: Vec<u8>,
}

fn route(path: &'static str, status: u16, body: String) -> Route {
    Route {
        path,
        status,
        body: body.into_bytes(),
    }
}

/// Route one binary body, such as a downloaded archive.
fn route_bytes(path: &'static str, status: u16, body: Vec<u8>) -> Route {
    Route { path, status, body }
}

/// Serve `routes` on a free localhost port and return the origin URL.
///
/// The server lives in the test process, so it stops with the test binary and
/// needs no cleanup registration.
fn serve(routes: Vec<Route>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("test origin binds a free port");
    let address = listener
        .local_addr()
        .expect("test origin reports its address");
    let routes = Arc::new(routes);

    thread::spawn(move || {
        while let Ok((stream, _)) = listener.accept() {
            handle_connection(stream, &routes);
        }
    });

    format!("http://{address}")
}

fn handle_connection(mut stream: TcpStream, routes: &[Route]) {
    let mut buffer = [0_u8; 8192];
    let read = stream.read(&mut buffer).unwrap_or(0);
    let request = String::from_utf8_lossy(&buffer[..read]);
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or_default()
        .split('?')
        .next()
        .unwrap_or_default()
        .to_string();

    let (status, body) = match routes.iter().find(|route| route.path == path) {
        Some(route) => (route.status, route.body.clone()),
        None => (404, String::from("{\"error\":\"not found\"}").into_bytes()),
    };
    let response = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n",
        reason_phrase(status),
        body.len(),
    );

    let _ = stream.write_all(response.as_bytes());
    let _ = stream.write_all(&body);
    let _ = stream.flush();
}

const fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Status",
    }
}

/// Digest of one committed discovery fixture, computed at test time.
fn fixture_digest(relative_path: &str) -> String {
    let bytes = fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join(relative_path))
        .unwrap_or_else(|error| panic!("fixture {relative_path} is readable: {error}"));
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    format!("sha256:{:x}", hasher.finalize())
}

/// Bytes of one committed discovery fixture.
fn fixture_bytes(relative_path: &str) -> Vec<u8> {
    fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join(relative_path))
        .unwrap_or_else(|error| panic!("fixture {relative_path} is readable: {error}"))
}
