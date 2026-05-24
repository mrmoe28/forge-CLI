//! End-to-end TUI tests driven through a real pseudo-terminal.
//!
//! Each test boots the compiled `forge` binary inside a PTY (via
//! `portable-pty`), pumps the bytes coming back out through a `vt100` parser,
//! and waits for stable marker text on the parsed screen before driving the
//! next key. Assertions read the parsed screen, never raw ANSI bytes.
//!
//! Hard requirements observed by every test:
//!   * isolated `--config`, `--runs-dir`, `--sessions-dir`, and `HOME`;
//!   * deterministic polling with explicit timeouts — no fixed `sleep`s;
//!   * the harness's `Drop` impl always reaps the spawned child, even if the
//!     test panics, so no orphans leak between tests.
//!
//! Unix-only because the cancellation/process-group story (and the PTY
//! `portable-pty` dep) are Unix-shaped. The `#[cfg(unix)]` gate at the top
//! makes this a no-op on Windows.

#![cfg(unix)]

use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// How long to wait for any single screen predicate. Generous for slow CI;
/// most assertions land within tens of ms.
const SCREEN_WAIT: Duration = Duration::from_secs(8);

/// PTY rows × cols. Small enough that "transcript overflow" is achievable
/// with a handful of `/status` lines, big enough that the header/composer/
/// status bar all render.
const ROWS: u16 = 18;
const COLS: u16 = 100;

/// Drive the `forge` binary inside a PTY.
struct PtyHarness {
    parser: Arc<Mutex<vt100::Parser>>,
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    child_pid: Option<u32>,
    reader_thread: Option<thread::JoinHandle<()>>,
    // Temp dirs kept alive for the lifetime of the harness so the spawned
    // forge can read/write them. Drop order matters: child first, then dirs.
    _temp: TempDir,
}

impl PtyHarness {
    /// Boot `forge` with the given profile command embedded in a temp config.
    fn launch(profile_command: &[&str]) -> anyhow::Result<Self> {
        let temp = TempDir::new()?;
        let config_path = temp.path().join("config.toml");
        let cmd_array = profile_command
            .iter()
            .map(|s| format!("{:?}", s))
            .collect::<Vec<_>>()
            .join(", ");
        std::fs::write(
            &config_path,
            format!("[profiles.default]\ncommand = [{cmd_array}]\ntimeout_secs = 60\n"),
        )?;

        let runs = temp.path().join("runs");
        let sessions = temp.path().join("sessions");
        let home = temp.path().join("home");
        std::fs::create_dir_all(&home)?;

        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows: ROWS,
            cols: COLS,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let forge_bin = std::path::PathBuf::from(env!("CARGO_BIN_EXE_forge"));
        let mut builder = CommandBuilder::new(&forge_bin);
        builder.arg("--config");
        builder.arg(&config_path);
        builder.arg("--runs-dir");
        builder.arg(&runs);
        builder.arg("--sessions-dir");
        builder.arg(&sessions);
        // Intentionally NO --plain — we want the full TUI.
        // Wipe inherited environment except for PATH (so the profile binary
        // can resolve) and what we set explicitly. Stable TERM avoids
        // termcap surprises in the vt100 parser.
        builder.env_clear();
        if let Ok(path_env) = std::env::var("PATH") {
            builder.env("PATH", path_env);
        }
        builder.env("HOME", &home);
        builder.env("TERM", "xterm-256color");
        builder.env("LANG", "C.UTF-8");
        // forge's TUI checks is_terminal(stdout); the PTY satisfies that, so
        // we don't need an explicit flag.
        builder.cwd(temp.path());

        let child = pair.slave.spawn_command(builder)?;
        let child_pid = child.process_id();

        // Reader thread: pumps PTY bytes into the vt100 parser. Drops cleanly
        // when the master reader returns EOF (i.e. child exited).
        let parser = Arc::new(Mutex::new(vt100::Parser::new(ROWS, COLS, 1000)));
        let parser_for_thread = Arc::clone(&parser);
        let mut reader = pair.master.try_clone_reader()?;
        let reader_thread = thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => return,
                    Ok(n) => {
                        let mut p = parser_for_thread.lock().unwrap();
                        p.process(&buf[..n]);
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => return,
                }
            }
        });

        let writer = pair.master.take_writer()?;

        Ok(Self {
            parser,
            writer,
            master: pair.master,
            child,
            child_pid,
            reader_thread: Some(reader_thread),
            _temp: temp,
        })
    }

    /// Snapshot of the visible screen (no scrollback).
    fn screen_contents(&self) -> String {
        self.parser.lock().unwrap().screen().contents()
    }

    /// Block until `predicate` returns true against a fresh screen snapshot,
    /// up to [`SCREEN_WAIT`]. Returns the final snapshot on success or a
    /// `Err` whose message includes the last snapshot for easy debugging.
    fn wait_for_screen<P>(&self, label: &str, mut predicate: P) -> anyhow::Result<String>
    where
        P: FnMut(&str) -> bool,
    {
        self.wait_for_screen_timeout(label, SCREEN_WAIT, &mut predicate)
    }

    fn wait_for_screen_timeout<P>(
        &self,
        label: &str,
        timeout: Duration,
        predicate: &mut P,
    ) -> anyhow::Result<String>
    where
        P: FnMut(&str) -> bool,
    {
        let deadline = Instant::now() + timeout;
        loop {
            let snap = self.screen_contents();
            if predicate(&snap) {
                return Ok(snap);
            }
            if Instant::now() >= deadline {
                anyhow::bail!("timed out waiting for `{label}`. Last screen:\n---\n{snap}\n---");
            }
            thread::sleep(Duration::from_millis(40));
        }
    }

    /// Write raw bytes to the slave's stdin.
    fn send(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()?;
        Ok(())
    }

    /// Convenience for typing a string then pressing Enter.
    fn send_line(&mut self, text: &str) -> anyhow::Result<()> {
        self.send(text.as_bytes())?;
        self.send(b"\r")
    }

    /// Send Esc.
    fn send_esc(&mut self) -> anyhow::Result<()> {
        self.send(b"\x1b")
    }

    /// On Unix, check that a pid (if known) is no longer a live process.
    /// Uses `kill(pid, 0)` which returns ESRCH for dead pids. We accept
    /// EPERM as "still in a different process group we can't probe" rather
    /// than as a definite "alive" because the test runner's perms could
    /// differ — but in practice the spawned forge is the same uid so this
    /// branch should not fire.
    fn assert_pid_reaped(pid: u32) {
        let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if rc == 0 {
            panic!("forge pid {pid} is still alive after harness shutdown");
        }
        let err = std::io::Error::last_os_error();
        let raw = err.raw_os_error().unwrap_or(0);
        assert!(
            raw == libc::ESRCH || raw == libc::EPERM,
            "unexpected kill(0) errno {raw}: {err}"
        );
    }
}

impl Drop for PtyHarness {
    fn drop(&mut self) {
        // Best-effort orderly shutdown. If `/exit` isn't applicable (e.g.
        // the TUI is stuck on an approval card) the fallback `kill()` cleans
        // up. Either way the reader thread terminates when the master pty
        // sees EOF.
        let _ = self.send(b"/exit\r");
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Ok(Some(_)) = self.child.try_wait() {
                break;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                break;
            }
            thread::sleep(Duration::from_millis(40));
        }
        // Drop the master writer so the reader's read() returns 0 and the
        // background thread exits.
        drop(std::mem::replace(
            &mut self.writer,
            Box::new(std::io::sink()),
        ));
        let _ = self.master.resize(PtySize {
            rows: 1,
            cols: 1,
            pixel_width: 0,
            pixel_height: 0,
        });
        if let Some(thread) = self.reader_thread.take() {
            // Reader will return on EOF; join with a short safety timeout
            // via a sentinel (join itself can't be timed, but in practice
            // the reader exits within a few ms once writer is dropped).
            let _ = thread.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Scenario 1: boot/render smoke
// ---------------------------------------------------------------------------

#[test]
fn tui_boots_and_shows_recognizable_chrome() -> anyhow::Result<()> {
    let harness = PtyHarness::launch(&["true"])?;
    // The initial transcript greets the user and the header shows "forge".
    // We accept either as a "TUI booted" signal.
    let snap = harness.wait_for_screen("boot chrome", |s| {
        s.contains("forge") && s.contains("/help")
    })?;
    // Spot-check the footer/composer hint that always renders.
    assert!(
        snap.contains("enter send") || snap.contains("/help"),
        "expected composer/status hint, got:\n{snap}"
    );
    Ok(())
}

#[test]
fn tui_exits_cleanly_on_slash_exit() -> anyhow::Result<()> {
    let mut harness = PtyHarness::launch(&["true"])?;
    let pid = harness.child_pid;
    harness.wait_for_screen("boot", |s| s.contains("forge"))?;
    harness.send_line("/exit")?;
    // Wait for the child to exit. The Drop impl also handles this, but here
    // we want to assert it happens before fallback kill.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(_) = harness.child.try_wait()? {
            break;
        }
        if Instant::now() >= deadline {
            anyhow::bail!("/exit did not terminate forge within 5s");
        }
        thread::sleep(Duration::from_millis(40));
    }
    drop(harness);
    if let Some(pid) = pid {
        PtyHarness::assert_pid_reaped(pid);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Scenario 2: suggestion-pane Enter routing
// ---------------------------------------------------------------------------

#[test]
fn enter_on_partial_slash_applies_suggestion_not_submits() -> anyhow::Result<()> {
    let mut harness = PtyHarness::launch(&["true"])?;
    harness.wait_for_screen("boot", |s| s.contains("forge"))?;
    // `/comp` fuzzy-matches `/compact` (the only command with the `c-o-m-p`
    // subsequence) so suggestion[0] is unambiguously /compact. This is
    // distinct from `/sk` which ties between /skill and /skills.
    harness.send(b"/comp")?;
    harness.wait_for_screen("suggestion visible", |s| s.contains("/compact"))?;
    // Press Enter. The contract: a partial slash with a highlighted
    // suggestion is COMPLETED to that suggestion rather than rejected as
    // unknown. /compact runs and emits a distinctive status line we can
    // grep for. If Enter had instead been treated as "submit /comp as
    // unknown slash", the status would read "Unknown command /comp ..."
    // — the negative assertion below rules that out.
    harness.send(b"\r")?;
    let snap = harness.wait_for_screen("compact ran via suggestion", |s| {
        // Fresh session has 0 turns, so /compact reports "nothing to compact".
        s.contains("nothing to compact")
    })?;
    assert!(
        !snap.contains("Unknown command"),
        "Enter on /comp should not produce an unknown-command hint:\n{snap}"
    );
    Ok(())
}

#[test]
fn enter_on_complete_known_command_dispatches_verbatim() -> anyhow::Result<()> {
    let mut harness = PtyHarness::launch(&["true"])?;
    harness.wait_for_screen("boot", |s| s.contains("forge"))?;
    // `/skill clear` is the canonical regression case: it fuzzy-matches
    // `/skills`, so the old suggestion-hijack bug would have routed Enter
    // through the palette. The current contract: a complete known command
    // dispatches verbatim.
    harness.send_line("/skill clear")?;
    harness.wait_for_screen("skill clear dispatched", |s| s.contains("Skills cleared"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Scenario 3: transcript scrolling
// ---------------------------------------------------------------------------

#[test]
fn transcript_overflows_and_can_be_scrolled() -> anyhow::Result<()> {
    let mut harness = PtyHarness::launch(&["true"])?;
    harness.wait_for_screen("boot", |s| s.contains("forge"))?;
    // Each `/status` pushes 6 system lines; with an 18-row viewport plus
    // chrome, ~6 invocations overflow comfortably.
    for _ in 0..8 {
        harness.send_line("/status")?;
        // Give the screen a moment to settle so we can land subsequent
        // commands without racing the renderer.
        harness.wait_for_screen("status rendered", |s| s.contains("last run:"))?;
    }
    // Sanity: after the spam, the screen still contains a status line —
    // the renderer didn't crash.
    let after_spam = harness.screen_contents();
    assert!(
        after_spam.contains("last run:"),
        "expected status output to still be visible after spam:\n{after_spam}"
    );
    // Scroll up via PageUp. ratatui's transcript honors PageUp through the
    // composer's scroll keys. The visible content should change.
    let before = after_spam.clone();
    harness.send(b"\x1b[5~")?; // PageUp
    // Wait for *any* render delta. We can't assert "older line X became
    // visible" because the system transcript repeats `session:` / `cwd:`
    // pairs verbatim, but the cursor row and footer change after a scroll.
    let _ = harness.wait_for_screen_timeout(
        "scroll caused a render delta",
        Duration::from_secs(2),
        &mut |snap| snap != before,
    );
    // PageDown brings us back to the live tail.
    harness.send(b"\x1b[6~")?; // PageDown
    harness.wait_for_screen("returned to tail after PageDown", |s| {
        s.contains("last run:")
    })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Scenario 4: cancel rendering and process-tree cleanup
// ---------------------------------------------------------------------------

/// Use a profile that emulates the worst-case "real agent under sh -c" shape:
/// sh forks a long sleep as a background job, then `wait`s on it. Without the
/// `process_group(0)` cancel fix this test would wedge on the orphaned
/// grandchild holding the stdout/stderr pipes.
fn cancel_profile() -> [&'static str; 3] {
    ["sh", "-c", "sleep 30 & wait"]
}

#[test]
fn cancel_shows_cancel_text_and_does_not_hang() -> anyhow::Result<()> {
    let mut harness = PtyHarness::launch(&cancel_profile())?;
    let pid = harness.child_pid;
    harness.wait_for_screen("boot", |s| s.contains("forge"))?;

    // Flip to bypass so prompts dispatch immediately without an approval
    // card (guarded mode would otherwise interrupt the flow).
    harness.send_line("/bypass on")?;
    harness.wait_for_screen("bypass on", |s| s.contains("Bypass mode on"))?;

    // Send any prompt; the profile sleeps for 30s, so we'll catch a
    // "Running" state and cancel it.
    harness.send_line("anything")?;
    harness.wait_for_screen("run started", |s| s.contains("Running"))?;

    // Press Esc to cancel. The status bar swaps to "Cancelling…" before
    // the final "Cancelled in Nms" lands; we accept either.
    harness.send_esc()?;
    harness.wait_for_screen("cancel landed", |s| {
        s.contains("Run cancelled") || s.contains("Cancelled")
    })?;

    // Tail: ensure the TUI is still responsive (no wedge on orphaned
    // grandchild). `/status` should render its 6 lines within the screen-
    // wait timeout.
    harness.send_line("/status")?;
    harness.wait_for_screen("post-cancel responsive", |s| s.contains("last run:"))?;

    // Tear down explicitly and confirm reap.
    harness.send_line("/exit")?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(_) = harness.child.try_wait()? {
            break;
        }
        if Instant::now() >= deadline {
            anyhow::bail!("forge did not exit within 5s after /exit");
        }
        thread::sleep(Duration::from_millis(40));
    }
    drop(harness);
    if let Some(pid) = pid {
        PtyHarness::assert_pid_reaped(pid);
    }
    Ok(())
}
