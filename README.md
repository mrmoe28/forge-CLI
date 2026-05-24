# forge

Interactive harness for external coding agents (opencode, claude, codex, …).
Wraps the underlying agent subprocess with:

- a streaming TUI built on ratatui, with a persistent status bar showing
  run state (`● running 12s` / `○ idle`), session id, cwd (with `$HOME`
  collapsed to `~`), and permission mode
- a single input classifier shared by the TUI and the plain REPL that
  routes between known slash commands, filesystem paths, and prompt text,
  so pasting an absolute path never gets treated as an unknown command
  and unknown slashes block with a "did you mean" hint
- a multi-line composer with bracketed paste, prompt history, full cursor
  & word/line edits, plus a small border chip (` path ` / ` unknown `)
  showing how Enter will route the current text
- a fuzzy slash-command palette with categorized commands and detail panel
- structured run transcript markers (`thinking:`, `output:`, `errors:`,
  `done:` / `failed:`) so streamed agent output is easier to scan
- `Esc` (or `/cancel`) aborts an in-flight run cleanly: the spawned task
  is dropped, the child agent is reaped via `kill_on_drop`, and a
  `Run cancelled` line lands in the transcript (with a partial-output
  note when streams had already opened)
- guarded-mode approval cards that gate a run before forge actually invokes
  the agent
- persistent sessions you can `/new`, `/resume`, `/sessions`, `/fork`, `/export`
- first-class skills with YAML frontmatter (`description`, `triggers`) that
  auto-inject for one turn when a trigger phrase appears in the prompt
- explicit `/learn` notes you can save, review, accept, inspect, and forget
  without automatic transcript capture or prompt injection
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

State lives under `.codex/external-agent-harness/{runs,sessions,learning}/`
in the current working directory by default; override with `--runs-dir`,
`--sessions-dir`, and `--learning-dir`.

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
2. **Submit**. Pressing Enter classifies the composer text via
   `commands::classify_input`, which has four outcomes:
   - **`Command`** — first token is in the slash registry. Dispatches the
     full body verbatim, so `/permissions bypass`, `/profile default`,
     `/skill clear`, `/provider set abc123`, and friends always reach
     their handler with their arguments intact. The suggestion palette
     never overrides a `Command` classification — it only steers Enter
     when the user is still mid-typing an incomplete or unknown slash.
   - **`Path`** — input starts with `/` and is either an existing path or
     has another `/` in the first whitespace-delimited segment. Submitted
     as prompt text (the agent still sees `/home/me/x`, not a command).
   - **`UnknownSlash`** — starts with `/` but is neither a known command
     nor a path. Submission is **blocked**; the status bar surfaces an
     `Unknown command /foobar. Try /<closest>` hint and the composer text
     is preserved so the user can fix it.
   - **`Prompt`** — everything else. Submitted as prompt text.

   The same classifier drives the plain REPL, so behaviour is identical
   between `forge` and `forge --plain`.
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

### Slash commands

Every `/<name>` is registered in `commands::COMMANDS` exactly once, with a
category, summary, usage string, and longer help string. Both the TUI
dispatcher (`terminal_ui::handle_command`) and the plain REPL
(`main::handle_interactive_command`) read from that registry. The Enter
handler in the TUI dispatches a typed command verbatim — the fuzzy
suggestion palette never overrides a complete `Command(_)`
classification, so `/skill <name>`, `/permissions bypass`, `/profile
default`, `/provider set abc`, and other arg-bearing forms always reach
their handler with their arguments intact.

Where the TUI and REPL diverge intentionally:

| Commands | TUI | Plain REPL |
|---|---|---|
| `help`, `status`, `clear`, `exit`, `profile`, `profiles`, `model`, `permissions`, `bypass`, `desktop`, `skills`, `skill`, `runs`, `last`, `retry`, `new`, `resume`, `sessions`, `fork`, `compact`, `provider`, `learn` | dispatched | dispatched |
| `cancel`, `smoke`, `inspect`, `open-run`, `logs`, `export`, `jobs` | dispatched | prints "only available in the interactive TUI" |

Registry-coverage and classifier tests (`every_registered_command_classifies_as_command`,
`commands_with_arguments_classify_with_full_body`,
`unknown_slash_never_classifies_as_command`,
`nested_path_input_classifies_as_path_not_command`, plus the existing
`every_command_resolves_via_lookup` / `every_alias_resolves_to_a_known_command`)
keep the invariants honest as new commands land.

### TUI affordances

The visible chrome around the transcript is designed so each piece of
state has exactly one home and never has to compete for the same row.

- **Top header**: `forge` · profile · permission mode · skill summary.
  Stable info only; never blinks.
- **Composer border**: title says ` message ` on the left. On the right,
  a small chip appears when the current input is non-trivial:
  - ` path ` (magenta) — input classifies as a filesystem path; Enter
    sends it as prompt text.
  - ` unknown ` (red) — input starts with `/` and matches no known
    command; Enter is blocked.

  Plain prompts and known commands keep the default look, so the border
  stays quiet during normal typing.
- **Bottom status bar**: persistent, single line:

  ```
  ● running 12s  ·  session 4f3a8b  ·  cwd ~/forge-CLI  ·  guarded         Profile switched to dev
  ```

  The left half is always the same four segments (run state, session id,
  cwd with `$HOME` collapsed to `~`, mode). The right half carries the
  ephemeral status message when one is set (`Running…`, `Failed: …`,
  `Unknown command /…`, etc.), and falls back to `enter send · /help`
  otherwise. Status messages no longer compete with profile/mode/skills
  info in the top header.
- **Run timer**: while a run is in flight, the status bar's run-state
  segment shows the live elapsed time, scaling from `12s` to `2m05s`.
- **Run transcript sections**: each submitted prompt is followed by a
  `thinking:` marker while forge is waiting for the agent subprocess,
  `output:` before the first stdout line, `errors:` before the first stderr
  line, and a final `done:` / `failed:` summary with duration and run id.
  These are forge-observed activity labels, not hidden model reasoning.

### Cancellation

While a run is active, **`Esc`** aborts it: the spawned tokio task is
dropped, which drops the `tokio::process::Child` handle. The agent is
spawned with `kill_on_drop(true)` so the OS reaps the process. A system
line `Run cancelled` is pushed to the transcript (with
`(partial output preserved above)` when stdout or stderr had already
emitted lines), the status bar flips back to `○ idle`, and the composer
returns to its normal editable state.

`/cancel` is the slash-command equivalent of the same key, useful in IME
setups where `Esc` is intercepted. When no run is active, `Esc` falls
through to the previous behaviour of exiting the TUI; `Ctrl+C` always
exits regardless.

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

### Explicit learning

`/learn` is a small, manual note store for things the user explicitly wants
forge to remember. It does **not** scrape transcripts, generate summaries,
write prompts automatically, or inject accepted notes into future agent runs.
Everything starts with a user-typed command.

Default storage:

```text
.codex/external-agent-harness/learning/
  pending/<id>.md
  accepted/<id>.md
  rejected/<id>.md
  events.jsonl
  .disabled
```

Useful commands:

| Command | Effect |
|---|---|
| `/learn` or `/learn status` | Show storage path, note counts, and disabled flag |
| `/learn save <note>` | Write a pending Markdown note |
| `/learn review` | List pending notes with ids and previews |
| `/learn accept <id>` | Move a pending note to accepted |
| `/learn reject <id>` | Move a pending note to rejected |
| `/learn accepted` | List accepted notes |
| `/learn show <id>` | Print a note's status, path, and body |
| `/learn forget <id>` | Delete an accepted note |
| `/learn off` / `/learn on` | Disable or re-enable future saves |

Note ids are generated from uuid v4 prefixes and user input is never used in
filenames. `accept`, `reject`, `show`, and `forget` accept unique id prefixes
and reject path-like characters before touching the filesystem.

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
| `src/commands.rs` | Slash command registry, categories, fuzzy matcher, input classifier (`InputClass`, `classify_input`) shared by the TUI and the REPL |
| `src/learning.rs` | Explicit `/learn` storage: pending/accepted/rejected notes, review/show/forget, disabled marker, events log |

## Status

Built and tested on Linux with Rust stable. The TUI uses bracketed paste,
which most modern terminals support; pasting falls back to per-keystroke
events if the terminal doesn't.

## License

Apache-2.0. Derived from the external-agent-harness crate written for
[openai/codex](https://github.com/openai/codex).
