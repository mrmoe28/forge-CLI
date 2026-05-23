# forge

Interactive harness for external coding agents (opencode, claude, codex, …).
Wraps the underlying agent subprocess with:

- a streaming TUI built on ratatui
- persistent sessions you can `/new`, `/resume`, `/sessions`, `/fork`, `/export`
- a multi-line composer with bracketed paste, prompt history, full cursor &
  word/line edits
- a fuzzy slash-command palette with categorized commands and detail panel
- guarded-mode approval cards that gate a run before forge actually invokes
  the agent
- first-class skills with YAML frontmatter (`description`, `triggers`) that
  auto-inject for one turn when a trigger phrase appears in the prompt
- provider session continuity via per-profile `continue_args` and
  `session_id_capture_prefix`

## Install

```sh
cargo install --path .
```

The binary is named `forge`.

## Quick start

```sh
forge                       # launches the TUI, /help for commands
forge run "say hello"       # one-shot CLI mode
forge jobs jobs.json        # batch from a JSON or CSV file
forge --plain               # line-oriented REPL (skips the TUI)
```

## Configuration

Pass a TOML config with `--config`. Minimal example:

```toml
[profiles.default]
command = ["opencode", "run"]
prompt_arg = true
timeout_secs = 600
bypass_args = ["--dangerously-skip-permissions"]
continue_args = ["--session", "{session_id}"]
session_id_capture_prefix = "session: "
```

Without `--config`, forge uses a default profile that runs `opencode run`.

State lives under `.codex/external-agent-harness/{runs,sessions}/` in the
current working directory by default; override with `--runs-dir` /
`--sessions-dir`.

## Status

Built and tested on Linux with Rust stable. The TUI uses bracketed paste,
which most modern terminals support; pasting falls back to per-keystroke
events if the terminal doesn't.

## License

Apache-2.0. Derived from the external-agent-harness crate written for
[openai/codex](https://github.com/openai/codex).
