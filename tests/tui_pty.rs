#![cfg(unix)]

mod support;

use std::time::Duration;
use support::tui_pty::{HarnessConfig, Key, ProfileSpec, PtyHarness};

#[test]
fn harness_boots_real_tui_then_exits_cleanly() -> anyhow::Result<()> {
    let mut harness = PtyHarness::launch_default()?;
    let pid = harness.pid();

    harness.wait_for_contains("forge")?;
    harness.wait_for_contains("/help")?;

    harness.send_text("PTYMARK")?;
    harness.wait_for_contains("PTYMARK")?;
    for _ in 0..7 {
        harness.send_key(Key::Backspace)?;
    }
    harness.wait_for_not_contains("PTYMARK")?;

    let status = harness.exit_cleanly()?;
    assert!(status.success(), "expected clean /exit, got {status:?}");

    drop(harness);
    if let Some(pid) = pid {
        assert!(
            !PtyHarness::pid_alive(pid),
            "forge pid {pid} is still alive after clean exit"
        );
    }

    Ok(())
}

#[test]
fn enter_on_partial_slash_applies_suggestion_not_submits() -> anyhow::Result<()> {
    let mut harness = PtyHarness::launch_default()?;
    harness.wait_for_contains("forge")?;

    harness.send_text("/comp")?;
    harness.wait_for_contains("/compact")?;
    harness.send_key(Key::Enter)?;

    let snap = harness.wait_for_contains("nothing to compact")?;
    assert!(
        !snap.contains("Unknown command"),
        "Enter on /comp should not produce an unknown-command hint:\n{snap}"
    );
    Ok(())
}

#[test]
fn enter_on_complete_known_command_dispatches_verbatim() -> anyhow::Result<()> {
    let mut harness = PtyHarness::launch_default()?;
    harness.wait_for_contains("forge")?;

    harness.send_line("/skill clear")?;
    harness.wait_for_contains("Skills cleared")?;
    Ok(())
}

#[test]
fn run_output_uses_structured_activity_sections() -> anyhow::Result<()> {
    let mut harness = PtyHarness::launch_default()?;
    harness.wait_for_contains("forge")?;

    harness.send_line("/bypass on")?;
    harness.wait_for_contains("Bypass mode on")?;

    harness.send_line("show structured output")?;
    harness.wait_for_contains("thinking: started agent run")?;
    harness.wait_for_contains("output: agent response")?;
    harness.wait_for_contains("FORGE_PTY_FAST_MARKER")?;
    harness.wait_for_contains("done: run succeeded")?;
    Ok(())
}

#[test]
fn spawn_fans_prompt_out_to_multiple_agents() -> anyhow::Result<()> {
    let mut harness = PtyHarness::launch_default()?;
    harness.wait_for_contains("forge")?;

    harness.send_line("/spawn 2 compare approaches")?;
    harness.wait_for_contains("spawn: launching 2 agent")?;
    harness.wait_for_contains("prompt: compare approaches")?;
    harness.wait_for_contains("spawn done: 2/2 succeeded, 0 failed")?;
    harness.wait_for_contains("spawn result 1:")?;
    harness.wait_for_contains("spawn result 2:")?;
    Ok(())
}

#[test]
fn transcript_overflows_and_can_be_scrolled() -> anyhow::Result<()> {
    let mut harness = PtyHarness::launch(HarnessConfig::default_fast().size(18, 100))?;
    harness.wait_for_contains("forge")?;

    for _ in 0..8 {
        harness.send_line("/status")?;
        harness.wait_for_contains("last run:")?;
    }

    let before = harness.screen();
    assert!(
        before.contains("last run:"),
        "expected status output after repeated /status:\n{before}"
    );

    harness.send_key(Key::PageUp)?;
    harness.wait_for_screen_change(&before, Duration::from_secs(2))?;

    harness.send_key(Key::PageDown)?;
    harness.wait_for_contains("last run:")?;
    Ok(())
}

#[test]
fn cancel_shows_cancel_text_and_does_not_hang() -> anyhow::Result<()> {
    let mut harness =
        PtyHarness::launch_with_profile(ProfileSpec::slow_shell_wrapper("default", "SLOW_MARKER"))?;
    let pid = harness.pid();
    harness.wait_for_contains("forge")?;

    harness.send_line("/bypass on")?;
    harness.wait_for_contains("Bypass mode on")?;

    harness.send_line("anything")?;
    harness.wait_for_contains("Running")?;

    harness.send_key(Key::Esc)?;
    harness.wait_for("cancel landed", Duration::from_secs(8), |screen| {
        screen.contains("Run cancelled") || screen.contains("Cancelled")
    })?;

    harness.send_line("/status")?;
    harness.wait_for_contains("last run:")?;

    let status = harness.exit_cleanly()?;
    assert!(status.success(), "expected clean /exit, got {status:?}");

    drop(harness);
    if let Some(pid) = pid {
        assert!(
            !PtyHarness::pid_alive(pid),
            "forge pid {pid} is still alive after cancel scenario"
        );
    }
    Ok(())
}

#[test]
fn learn_save_accept_accepted_and_show_work_in_tui() -> anyhow::Result<()> {
    let mut harness = PtyHarness::launch_default()?;
    harness.wait_for_contains("forge")?;

    harness.send_line("/learn save prefer rg for repo searches")?;
    let saved = harness.wait_for_contains("saved pending note")?;
    let id = extract_saved_note_id(&saved)?;

    let accept_command = format!("/learn accept {id}");
    harness.send_line(&accept_command)?;
    harness.wait_for_contains(&format!("accepted {id}"))?;

    harness.send_line("/learn accepted")?;
    harness.wait_for_contains("1 accepted note")?;
    harness.wait_for_contains("prefer rg for repo searches")?;

    let show_command = format!("/learn show {id}");
    harness.send_line(&show_command)?;
    harness.wait_for_contains("status: accepted")?;
    harness.wait_for_contains("prefer rg for repo searches")?;
    Ok(())
}

fn extract_saved_note_id(screen: &str) -> anyhow::Result<String> {
    let Some(index) = screen.find("saved pending note") else {
        anyhow::bail!("saved note marker missing from screen:\n{screen}");
    };
    let after = &screen[index + "saved pending note".len()..];
    let Some(id) = after.split_whitespace().next() else {
        anyhow::bail!("saved note id missing from screen:\n{screen}");
    };
    Ok(id.to_string())
}
