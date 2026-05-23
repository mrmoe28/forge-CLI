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

## Architecture

Forge is a **harness**, not an agent. It does not call an LLM, does not
interpret tool calls, and does not enforce sandboxing. Its job is to drive
some other coding agent as a subprocess and provide a richer surface around
it: durable sessions, an approval gate, a structured composer, log capture,
and prompt-time skill injection.

### Layers

```
                    ┌──────────────────────────────────────────┐
                    │            User interactions             │
                    └──────────────────────────────────────────┘
                                       │
            ┌──────────────────────────┴──────────────────────────┐
            │                                                     │
   ┌────────▼──────────┐                                ┌─────────▼────────┐
   │  terminal_ui.rs   │                                │     main.rs      │
   │  (ratatui TUI)    │                                │  (plain REPL +   │
   │                   │                                │   CLI commands)  │
   └────────┬──────────┘                                └─────────┬────────┘
            │ composer.rs (text editing)                          │
            │ commands.rs (slash registry, fuzzy search)          │
            └──────────────────────────┬──────────────────────────┘
                                       │
                                       ▼
                    ┌──────────────────────────────────────────┐
                    │   lib.rs — the harness core              │
                    │                                          │
                    │  • HarnessConfig, AgentProfile loader    │
                    │  • Session / SessionTurn persistence     │
                    │  • Skill discovery and frontmatter parse │
                    │  • run_agent_streaming (the executor)    │
                    │  • RunRecord + run directory layout      │
                    └──────────────────┬───────────────────────┘
                                       │
                                       ▼
                    ┌──────────────────────────────────────────┐
                    │      tokio::process::Command(agent)      │
                    │   stdout / stderr → mpsc<RunEvent>       │
                    └──────────────────────────────────────────┘
```

### A turn, end to end

1. **Compose**. Keystrokes flow through `Composer`, which owns the buffer,
   the cursor, and the prompt history.
2. **Submit**. Pressing Enter calls `composer.submit()`, returning the text.
   If the input is a slash command, it routes through the `commands`
   registry; otherwise it becomes a prompt.
3. **Approval gate**. In guarded mode (no `--bypass` and no `--desktop`),
   the prompt is stashed in `state.pending_approval` and a modal renders
   over the transcript. The agent is **not** spawned until the user
   presses `y`, `a` (also flips bypass for the session), `Enter`, or
   denies with `n` / `Esc`.
4. **Resolve skills**. `active_skill_prompt_for(prompt)` collects:
   - skills the user activated with `/skill <name>` (sticky), plus
   - skills whose `triggers` frontmatter matches the prompt as a
     case-insensitive substring (one-shot).

   Each chosen skill's body is appended to a `prompt_prefix` string. The
   default rendering is:

   ```
   Use the following active skills for this request:

   --- skill: <name> ---
   <body>
   ```
5. **Build the request**. `start_run` constructs a `RunRequest` from the
   active session (`profile`, `cwd`, `timeout_secs`, `bypass`, `desktop`,
   `provider_session_id`) plus the prompt and prefix.
6. **Resolve the command line**. `resolved_command(profile, prompt, request)`
   builds the argv:
   - start with `profile.command`
   - append `profile.bypass_args` if `bypass_permissions`
   - append `profile.desktop_args` if `desktop_control`
   - append `profile.continue_args` (with `{session_id}` substituted) if
     the session has a captured provider session id
   - push the prompt itself if `prompt_arg = true`
7. **Spawn and stream**. `run_agent_streaming` spawns the child with piped
   stdout/stderr, creates an `mpsc::unbounded_channel<RunEvent>`, and
   launches one tokio task per pipe. Each pipe task uses
   `BufReader::read_until('\n')` to read one line, writes the raw bytes to
   the run's `stdout.log` / `stderr.log`, and sends
   `RunEvent::Stdout(line)` or `RunEvent::Stderr(line)`. The TUI loop
   drains the channel between frames via `try_recv`, so the user sees
   lines as they arrive.
8. **Wait and finalize**. The main task awaits `child.wait()` with the
   profile's timeout. On success or failure, the pipe tasks are awaited,
   the captured session id (if any) is extracted by scanning `stdout.log`
   for `session_id_capture_prefix`, and a `RunRecord` is persisted to the
   run directory.
9. **Update the session**. The TUI's `pump_active_run` records the
   assistant turn (the buffered stdout) and the new `run_id` on the
   `Session`, captures the provider session id if one was extracted, and
   writes the session JSON.

### Storage layout

By default forge writes under the current working directory:

```
.codex/external-agent-harness/
├── runs/
│   └── 20260523T141204Z_<uuid>/
│       ├── prompt.txt        ← exact prompt sent to the agent
│       ├── stdout.log        ← raw stdout, newline-preserved
│       ├── stderr.log
│       └── record.json       ← RunRecord, see below
└── sessions/
    └── <uuid>.json           ← Session, see below
```

A **`RunRecord`** is the immutable summary of one agent invocation:

```jsonc
{
  "id": "…",
  "profile": "default",
  "label": null,
  "prompt": "…",
  "command": ["opencode", "run", "…"],   // fully resolved argv
  "cwd": "/path/to/project",
  "started_at": "…",
  "finished_at": "…",
  "duration_ms": 1234,
  "timeout_secs": 600,
  "status": "succeeded",                 // | "failed" | "timed_out"
  "exit_code": 0,
  "stdout_log": "…/stdout.log",
  "stderr_log": "…/stderr.log",
  "captured_session_id": "abc123"        // only if capture_prefix matched
}
```

A **`Session`** is the durable conversation state:

```jsonc
{
  "id": "…",
  "name": null,
  "profile": "default",
  "cwd": "/path/to/project",
  "bypass": false,
  "desktop": false,
  "timeout_secs": null,
  "active_skills": [],
  "transcript": [
    { "role": "user",      "text": "…", "at": "…" },
    { "role": "assistant", "text": "…", "run_id": "…", "at": "…" }
  ],
  "run_ids": ["…"],
  "provider_session_id": "abc123",       // for continue_args templating
  "created_at": "…",
  "updated_at": "…"
}
```

### Profiles

A profile is a recipe for invoking one agent CLI. All fields are optional
except `command`:

| Field | Purpose |
|---|---|
| `command` | Base argv. e.g. `["opencode", "run"]` |
| `prompt_arg` | If true, append the user prompt as a positional arg |
| `bypass_args` | Args appended when `/bypass on` |
| `desktop_args` | Args appended when `/desktop on` |
| `desktop_prompt_prefix` | Text injected into the prompt under `/desktop on` |
| `env` | Extra env vars for the spawned process |
| `cwd` | Override working directory (defaults to the session cwd) |
| `timeout_secs` | Hard limit; expires kill the child |
| `continue_args` | Appended when the session has a captured id. `{session_id}` is substituted. Example: `["--session", "{session_id}"]` |
| `session_id_capture_prefix` | Forge scans the run's stdout for a line starting with this prefix and stores the remainder as the provider session id |

### Sessions vs run records

Sessions and run records are **independent**. A `RunRecord` is an
append-only audit trail of one subprocess invocation. A `Session` is the
user-facing conversation: it accumulates user/assistant turns and the
sequence of run ids that produced them. `/compact` trims the session
without touching run records on disk. `/fork` clones the session under a
new id; both branches keep referring to the same run records.

### Provider session continuity

Most agent CLIs run as one-shot processes that forget the previous turn.
Two profile fields let forge stitch turns back together:

1. After every run, forge scans `stdout.log` for the first line beginning
   with `session_id_capture_prefix` and stores the remainder of the line
   on the active session as `provider_session_id`.
2. On subsequent runs in that session, if `provider_session_id` is set,
   forge appends `continue_args` to the command, substituting
   `{session_id}` with the captured id. The agent now resumes its own
   internal conversation state across forge turns.

If your agent doesn't print a session id, you can set the id manually
with `/provider set <id>` or clear it with `/provider clear`.

### Skills

Skills are markdown files at `skills/<name>/SKILL.md` (or any of the
fallback locations: `.agents/skills/`, `.opencode/skills/`,
`.claude/skills/`, `.cursor/skills/`, plus the same dirs under `$HOME`).

A skill may have YAML frontmatter:

```markdown
---
name: refactor-helper
description: Activate when the user is restructuring existing code
triggers:
  - refactor
  - rename
  - "extract function"
---

# Refactor helper

...skill body...
```

A skill is injected into the prompt if **either**:

- it is in the session's `active_skills` (sticky, set with `/skill <name>`)
- one of its `triggers` is a case-insensitive substring of the user prompt
  (one-shot for that turn only)

This is the key change from naïve harnesses that dump every discovered
skill on every turn; forge keeps the agent's context small unless the
skill is actually relevant.

### Approval cards

In guarded mode, Enter on a non-slash prompt does **not** spawn the
agent. It opens a modal showing:

- the active profile
- the fully-resolved command line that would be invoked
- the working directory
- the prompt (up to 10 lines)

Keys: `y` / Enter approve once · `a` approve and flip bypass on for the
session · `n` / `Esc` / `e` deny (composer text is preserved so the user
can edit and retry) · `Ctrl+C` exit forge.

Slash commands bypass the gate (they are internal to forge) and so do
runs where the user has already opted into `/bypass on` or `/desktop on`.

### Streaming model

The streaming path is what makes the TUI feel live. Two design choices
matter:

- **Line-buffered**, not byte-buffered. `BufReader::read_until('\n')`
  preserves line boundaries when echoing to the transcript and to
  `stdout.log`. Partial trailing output on EOF is still delivered.
- **Unbounded channel**. `mpsc::unbounded_channel<RunEvent>` means the
  agent never blocks on a slow TUI redraw. The TUI drains with
  `try_recv` once per frame.

If the consumer of `run_agent_streaming` drops its receiver mid-run, the
streaming pipe falls back to a `drain_pipe` path that keeps writing to
the log file but stops allocating strings nobody will read.

## File map

| File | Responsibility |
|---|---|
| `src/lib.rs` | Public API: `HarnessConfig`, `AgentProfile`, `Session`, `Skill`, `RunRequest`, `RunRecord`, `RunEvent`, `run_agent`, `run_agent_streaming`, `discover_skills`, session persistence |
| `src/main.rs` | Clap CLI; plain-mode REPL with full slash-command parity |
| `src/terminal_ui.rs` | Ratatui TUI: state, render loop, key handling, approval card, suggestion picker |
| `src/composer.rs` | Multi-line composer (cursor, history, paste, word-level edits) |
| `src/commands.rs` | Slash command registry, categories, fuzzy matcher |

## Status

Built and tested on Linux with Rust stable. The TUI uses bracketed paste,
which most modern terminals support; pasting falls back to per-keystroke
events if the terminal doesn't.

## License

Apache-2.0. Derived from the external-agent-harness crate written for
[openai/codex](https://github.com/openai/codex).
