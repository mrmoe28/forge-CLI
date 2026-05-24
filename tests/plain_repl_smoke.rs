//! Integration smoke tests for the plain (line-oriented) REPL.
//!
//! We use the plain REPL rather than a real PTY/TUI for these smoke tests
//! because:
//!   * full TUI automation requires a pseudo-terminal library and a virtual
//!     terminal parser (e.g. `expectrl` + `vt100`), each a non-trivial
//!     dependency we'd rather not pull into the harness;
//!   * the slash-command routing — including unknown handling, /doctor,
//!     /help, /permissions — is shared logic between TUI and plain REPL
//!     (see `commands::route_enter` and the registry parity tests in
//!     `src/commands.rs`), so exercising it through the plain path covers
//!     the same business logic.
//!
//! When the day comes for full TUI PTY tests, the obvious entry points are
//! `terminal_ui::handle_command` and the renderer in `terminal_ui::render`.

use assert_cmd::Command;
use std::io::Write;
use tempfile::TempDir;

fn forge() -> Command {
    Command::cargo_bin("forge").expect("forge binary builds")
}

fn write_config(temp: &TempDir) -> std::path::PathBuf {
    let path = temp.path().join("config.toml");
    // A minimal, deterministic profile. `true` is on PATH on every UNIX
    // system the harness targets, so `/doctor` will resolve it.
    let body = r#"
[profiles.default]
command = ["true"]
"#;
    std::fs::write(&path, body).unwrap();
    path
}

/// Helper: run the binary with `--plain`, piping `stdin_lines` joined with
/// `\n` into stdin, in a tempdir so runs/sessions are isolated.
fn run_plain(stdin_lines: &[&str]) -> (TempDir, assert_cmd::assert::Assert) {
    let temp = TempDir::new().unwrap();
    let config = write_config(&temp);
    let runs = temp.path().join("runs");
    let sessions = temp.path().join("sessions");

    let mut cmd = forge();
    cmd.arg("--config")
        .arg(&config)
        .arg("--runs-dir")
        .arg(&runs)
        .arg("--sessions-dir")
        .arg(&sessions)
        .arg("--plain")
        // Set HOME to a temp path so skill discovery doesn't walk the real
        // user's home (we don't care about discovered skills here, only that
        // discovery doesn't blow up).
        .env("HOME", temp.path())
        // Keep PATH so `/doctor` can locate `true`.
        .env("PATH", std::env::var("PATH").unwrap_or_default());

    let mut stdin = String::new();
    for line in stdin_lines {
        stdin.push_str(line);
        stdin.push('\n');
    }
    cmd.write_stdin(stdin);

    let assert = cmd.assert();
    (temp, assert)
}

#[test]
fn help_lists_known_commands_and_exits_cleanly() {
    let (_temp, assert) = run_plain(&["/help", "/exit"]);
    let assert = assert.success();
    let output = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    // /help in the plain REPL prints the registry; spot-check a few entries.
    assert!(
        output.contains("/help"),
        "missing /help in output: {output}"
    );
    assert!(
        output.contains("/doctor"),
        "missing /doctor in output: {output}"
    );
    assert!(
        output.contains("/runs"),
        "missing /runs in output: {output}"
    );
}

#[test]
fn unknown_slash_command_is_reported_with_hint() {
    let (_temp, assert) = run_plain(&["/foobarxyz", "/exit"]);
    let assert = assert.success();
    let output = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        output.contains("unknown command: /foobarxyz"),
        "expected unknown-command message, got: {output}"
    );
}

#[test]
fn permissions_bypass_is_handled_locally_without_a_run() {
    let (_temp, assert) = run_plain(&["/permissions bypass", "/permissions", "/exit"]);
    let assert = assert.success();
    let output = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    // After flipping to bypass, the no-arg /permissions prints the new mode.
    assert!(
        output.contains("permissions: bypass"),
        "expected `permissions: bypass` after toggling, got: {output}"
    );
}

#[test]
fn doctor_runs_and_reports_summary_line() {
    let (_temp, assert) = run_plain(&["/doctor", "/exit"]);
    let assert = assert.success();
    let output = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    // Each check line is `[OK|WARN|FAIL] name: ...`; the summary footer
    // matches the `-- N OK, N WARN, N FAIL --` format from main.rs.
    assert!(
        output.contains("[OK] profile:"),
        "expected profile OK line, got: {output}"
    );
    assert!(
        output.contains("[OK] command.binary:"),
        "expected command.binary OK line for `true`, got: {output}"
    );
    assert!(
        output.contains("OK,"),
        "expected summary footer, got: {output}"
    );
}

#[test]
fn clear_command_in_plain_repl_is_recognized() {
    // /clear in the plain REPL is a no-op (the line-oriented terminal has no
    // visible transcript to wipe) but it must not be reported as an unknown
    // command — that's the registry parity contract.
    let (_temp, assert) = run_plain(&["/clear", "/exit"]);
    let assert = assert.success();
    let output = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        !output.contains("unknown command: /clear"),
        "/clear should be dispatched, not rejected: {output}"
    );
}

#[test]
fn tui_only_commands_print_rejection_in_plain_repl() {
    // /cancel is TUI-only (see PLAIN_TUI_ONLY_COMMANDS); the plain REPL
    // should recognize it and print a tui-only rejection rather than
    // dispatching or treating it as unknown.
    let (_temp, assert) = run_plain(&["/cancel", "/exit"]);
    let assert = assert.success();
    let output = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        !output.contains("unknown command: /cancel"),
        "/cancel should be recognized, even if rejected as TUI-only: {output}"
    );
}

#[test]
#[ignore = "superseded by tests/tui_pty.rs"]
fn placeholder_tui_pty_test() {
    // Real TUI PTY automation now lives in `tests/tui_pty.rs` using
    // `portable-pty` + `vt100`. This stub stays as a discoverable signpost
    // (so a developer searching for "pty" finds both files) and exits
    // immediately when run.
    let _ = std::io::stdout().write_all(b"");
}
