//! Slash command registry.
//!
//! The TUI and the plain REPL both look up commands here so that adding a new
//! `/foo` happens in exactly one place: registering a [`SlashCommand`] in
//! [`COMMANDS`]. The registry also drives the fuzzy command palette
//! (autocomplete + selected-item details panel) by exposing match results
//! with stable categories and usage strings.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Category {
    Session,
    Mode,
    Skills,
    Runs,
    Learning,
    System,
}

impl Category {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Category::Session => "session",
            Category::Mode => "mode",
            Category::Skills => "skills",
            Category::Runs => "runs",
            Category::Learning => "learning",
            Category::System => "system",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct SlashCommand {
    pub name: &'static str,
    pub category: Category,
    pub summary: &'static str,
    pub usage: &'static str,
    pub help: &'static str,
    /// Whether this command is dispatched by the full-screen TUI. All
    /// currently registered commands are TUI-supported; the flag is here so
    /// parity tests can fail when a new command is added without TUI wiring.
    /// Read only by tests in [`tests::every_command_is_dispatched_by_the_tui`].
    #[allow(dead_code)]
    pub tui: bool,
    /// Whether this command is dispatched by the plain line-oriented REPL.
    /// Commands that are intentionally TUI-only (because they depend on an
    /// in-process active run or interactive controls) set this to `false` and
    /// the plain REPL rejects them with a "TUI-only" message. Read only by
    /// the registry parity tests.
    #[allow(dead_code)]
    pub plain: bool,
}

impl SlashCommand {
    /// True when the command is supported in the TUI but intentionally not in
    /// the plain REPL. Used by the registry parity tests.
    #[allow(dead_code)]
    pub(crate) fn is_tui_only(&self) -> bool {
        self.tui && !self.plain
    }
}

pub(crate) const COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "help",
        category: Category::System,
        summary: "show this command list",
        usage: "/help",
        help: "Lists all available slash commands with usage and a one-line summary.",
        tui: true,
        plain: true,
    },
    SlashCommand {
        name: "status",
        category: Category::System,
        summary: "show current state",
        usage: "/status",
        help: "Shows the current session id, profile, mode flags, active skills, working directory, and the most recent run id.",
        tui: true,
        plain: true,
    },
    SlashCommand {
        name: "clear",
        category: Category::System,
        summary: "clear visible transcript",
        usage: "/clear",
        help: "Clear the visible terminal transcript. The persisted session transcript on disk is unaffected.",
        tui: true,
        plain: true,
    },
    SlashCommand {
        name: "exit",
        category: Category::System,
        summary: "quit forge",
        usage: "/exit",
        help: "Save the current session and exit the TUI. /quit and /q are aliases.",
        tui: true,
        plain: true,
    },
    SlashCommand {
        name: "profile",
        category: Category::Mode,
        summary: "switch agent profile",
        usage: "/profile <name>",
        help: "Switch the active agent profile. Without an argument, prints the current profile. Use /profiles to see configured options.",
        tui: true,
        plain: true,
    },
    SlashCommand {
        name: "profiles",
        category: Category::Mode,
        summary: "list configured profiles",
        usage: "/profiles",
        help: "Print every profile name configured in the harness config TOML, in declared order.",
        tui: true,
        plain: true,
    },
    SlashCommand {
        name: "model",
        category: Category::Mode,
        summary: "show the active profile command",
        usage: "/model",
        help: "Show the command line forge runs for each new prompt. Models are baked into the profile command, so this is informational; use /profile to switch to a profile with a different model.",
        tui: true,
        plain: true,
    },
    SlashCommand {
        name: "permissions",
        category: Category::Mode,
        summary: "show or set permission mode",
        usage: "/permissions [guarded|bypass|desktop]",
        help: "Without an argument, prints the current permission mode. With one, sets the mode: guarded (no bypass), bypass (append the bypass flag), or desktop (bypass + inject the desktop control prompt prefix).",
        tui: true,
        plain: true,
    },
    SlashCommand {
        name: "bypass",
        category: Category::Mode,
        summary: "toggle dangerous permission bypass",
        usage: "/bypass [on|off]",
        help: "Toggle the per-session bypass flag. When on, forge appends the profile's bypass_args to the agent command.",
        tui: true,
        plain: true,
    },
    SlashCommand {
        name: "desktop",
        category: Category::Mode,
        summary: "toggle desktop control prompting",
        usage: "/desktop [on|off]",
        help: "Toggle the desktop control prompt prefix. Enabling desktop also enables bypass automatically.",
        tui: true,
        plain: true,
    },
    SlashCommand {
        name: "skills",
        category: Category::Skills,
        summary: "list discovered skills",
        usage: "/skills",
        help: "Walk the standard skill directories and list every SKILL.md found. Active skills are marked with `*`.",
        tui: true,
        plain: true,
    },
    SlashCommand {
        name: "skill",
        category: Category::Skills,
        summary: "activate, clear, or create a skill",
        usage: "/skill <name>|clear|create <name>",
        help: "Add a skill body to the prompt prefix for the next turn. `/skill clear` removes all active skills. `/skill create <name>` scaffolds a new skill under <session-cwd>/skills/<name>/SKILL.md.",
        tui: true,
        plain: true,
    },
    SlashCommand {
        name: "runs",
        category: Category::Runs,
        summary: "list recent runs",
        usage: "/runs",
        help: "Show the most recent agent runs in the runs directory, newest first.",
        tui: true,
        plain: true,
    },
    SlashCommand {
        name: "last",
        category: Category::Runs,
        summary: "show last response",
        usage: "/last",
        help: "Show the transcript of the most recent run in this session.",
        tui: true,
        plain: true,
    },
    SlashCommand {
        name: "cancel",
        category: Category::Runs,
        summary: "cancel the active run",
        usage: "/cancel",
        help: "Cancel the currently running agent invocation. This is the slash-command equivalent of pressing Esc while a run is active.",
        tui: true,
        plain: false,
    },
    SlashCommand {
        name: "retry",
        category: Category::Runs,
        summary: "retry a run",
        usage: "/retry [id]",
        help: "Resubmit the prompt from a prior run. Without an id, retries the last run from this session.",
        tui: true,
        plain: true,
    },
    SlashCommand {
        name: "new",
        category: Category::Session,
        summary: "start a fresh session",
        usage: "/new",
        help: "Save the current session and start a new one, keeping the current profile, cwd, and mode.",
        tui: true,
        plain: true,
    },
    SlashCommand {
        name: "resume",
        category: Category::Session,
        summary: "resume a saved session",
        usage: "/resume [id]",
        help: "Load a saved session. Without an id, loads the most recently updated session other than the current one.",
        tui: true,
        plain: true,
    },
    SlashCommand {
        name: "sessions",
        category: Category::Session,
        summary: "list saved sessions",
        usage: "/sessions",
        help: "Show every persisted session, newest first, with id prefix, profile, turn count, and last-updated timestamp. The current session is marked with `*`.",
        tui: true,
        plain: true,
    },
    SlashCommand {
        name: "fork",
        category: Category::Session,
        summary: "fork a session into a branch",
        usage: "/fork [id]",
        help: "Create a new session that inherits the transcript and configuration of the current (or named) session.",
        tui: true,
        plain: true,
    },
    SlashCommand {
        name: "compact",
        category: Category::Session,
        summary: "drop early turns to shrink session",
        usage: "/compact [keep]",
        help: "Keep only the most recent `keep` (default 20) turns in the session transcript. Run records on disk are not deleted.",
        tui: true,
        plain: true,
    },
    SlashCommand {
        name: "smoke",
        category: Category::Runs,
        summary: "send a quick health-check prompt",
        usage: "/smoke [prompt]",
        help: "Run a minimal prompt against the current profile to verify the agent is wired up. Defaults to `Reply exactly: ok`.",
        tui: true,
        plain: false,
    },
    SlashCommand {
        name: "inspect",
        category: Category::Runs,
        summary: "show full record for a run",
        usage: "/inspect [id]",
        help: "Print the persisted JSON record for a run, including command line, cwd, exit code, durations, and log paths. Defaults to the most recent run.",
        tui: true,
        plain: false,
    },
    SlashCommand {
        name: "open-run",
        category: Category::Runs,
        summary: "show transcript for a specific run",
        usage: "/open-run <id>",
        help: "Show the captured stdout/stderr for a specific run id (prefix matches).",
        tui: true,
        plain: false,
    },
    SlashCommand {
        name: "logs",
        category: Category::Runs,
        summary: "show log file paths for a run",
        usage: "/logs [id]",
        help: "Print the stdout.log and stderr.log paths for a run so you can tail or open them externally.",
        tui: true,
        plain: false,
    },
    SlashCommand {
        name: "export",
        category: Category::Session,
        summary: "export the current session as markdown",
        usage: "/export <path>",
        help: "Write the active session transcript to <path> as Markdown. Errors if the file already exists.",
        tui: true,
        plain: false,
    },
    SlashCommand {
        name: "jobs",
        category: Category::Runs,
        summary: "run batch jobs from a JSON or CSV file",
        usage: "/jobs <file> [concurrency]",
        help: "Run every job in <file> against the current profile. The TUI is unresponsive until the batch completes; cancel with Ctrl+C.",
        tui: true,
        plain: false,
    },
    SlashCommand {
        name: "provider",
        category: Category::Session,
        summary: "inspect or set the provider session id",
        usage: "/provider [show|clear|set <id>]",
        help: "Show, set, or clear the provider session id that forge sends via the profile's continue_args. Without an argument, prints the current id.",
        tui: true,
        plain: true,
    },
    SlashCommand {
        name: "doctor",
        category: Category::Session,
        summary: "validate the harness environment",
        usage: "/doctor",
        help: "Run a battery of OK/WARN/FAIL checks against the active profile, command binary, cwd, runs/sessions directories, and skills. Use it after editing config or moving directories.",
        tui: true,
        plain: true,
    },
    SlashCommand {
        name: "learn",
        category: Category::Learning,
        summary: "explicitly save, review, accept, or forget short notes",
        usage: "/learn <status|save <note>|review|accepted|show <id>|accept <id>|reject <id>|forget <id>|on|off>",
        help: "Minimal explicit-learning layer. Nothing is captured automatically. `/learn save <note>` writes a pending Markdown note under the learning dir; `/learn review` lists pending notes with ids; `/learn accepted` lists kept notes; `/learn show <id>` prints a note body; `/learn accept <id>` keeps a note, `/learn reject <id>` discards it, `/learn forget <id>` removes an accepted note. `/learn off` and `/learn on` toggle saving; `/learn status` prints counts and the storage path.",
        tui: true,
        plain: true,
    },
];

/// Aliases handled by `lookup` so that `/q`, `/quit`, `/h` all resolve.
pub(crate) const ALIASES: &[(&str, &str)] = &[("h", "help"), ("quit", "exit"), ("q", "exit")];

/// Classification of free-form user input at submit time.
///
/// Both the TUI Enter handler and the plain REPL route on this so behaviour
/// stays consistent: known slashes dispatch, pasted paths fall through to the
/// agent, and unknown slashes can be flagged with a "did-you-mean" hint
/// instead of being sent verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InputClass<'a> {
    /// First token is a known slash command. The captured slice is the
    /// command body (everything after the leading `/`).
    Command(&'a str),
    /// Input looks like a filesystem path; pass to the agent as a prompt.
    Path,
    /// Input starts with `/` but is neither a known command nor a path. The
    /// captured slice is the first whitespace-delimited token (without the
    /// leading slash) so callers can build a hint.
    UnknownSlash(&'a str),
    /// Plain prompt text (no leading `/`).
    Prompt,
}

/// What the Enter key should do for the current composer/REPL input, derived
/// purely from the input text. The TUI and the plain REPL both Enter-route on
/// this so the contract is testable without standing up a terminal.
///
/// Suggestion-pane behaviour is intentionally NOT modelled here. The TUI lets
/// the suggestion palette steal Enter only while the input does *not* yet
/// classify as a known command — i.e. while `classify_input` returns
/// `UnknownSlash`/`Prompt`/`Path`. For complete commands (with or without
/// arguments) the TUI bypasses the palette and dispatches verbatim. The test
/// `route_enter_returns_command_for_known_slash_with_args` pins that contract.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EnterRoute {
    /// Empty / whitespace-only input. Do nothing.
    Noop,
    /// Dispatch as command. The string is the command body (no leading `/`).
    Command(String),
    /// Submit to the agent as a prompt (also the case for path-looking input).
    Submit(String),
    /// Starts with `/` but is neither a known command nor a path. The string
    /// is the first whitespace-delimited token so callers can build a
    /// "did-you-mean" hint.
    UnknownSlash(String),
}

/// Decide what Enter should do for `input`. Pure; safe to call from tests.
#[allow(dead_code)]
pub(crate) fn route_enter(input: &str) -> EnterRoute {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return EnterRoute::Noop;
    }
    match classify_input(trimmed) {
        InputClass::Command(body) => EnterRoute::Command(body.to_string()),
        InputClass::UnknownSlash(token) => EnterRoute::UnknownSlash(token.to_string()),
        InputClass::Path | InputClass::Prompt => EnterRoute::Submit(trimmed.to_string()),
    }
}

pub(crate) fn classify_input(input: &str) -> InputClass<'_> {
    let trimmed = input.trim();
    let Some(rest) = trimmed.strip_prefix('/') else {
        return InputClass::Prompt;
    };
    let first_token = rest.split_whitespace().next().unwrap_or_default();
    if is_known(first_token) {
        return InputClass::Command(rest);
    }
    if looks_like_path(trimmed) {
        return InputClass::Path;
    }
    InputClass::UnknownSlash(first_token)
}

/// True when `trimmed` looks like a filesystem path the user pasted or typed.
/// Either the path actually exists on disk, or its first whitespace-delimited
/// segment contains an additional `/` (e.g. `/home/me/repo`). Single-segment
/// non-existent strings like `/foobar` are NOT classified as paths so they
/// can be flagged as unknown commands.
fn looks_like_path(trimmed: &str) -> bool {
    if !trimmed.starts_with('/') {
        return false;
    }
    let path = std::path::Path::new(trimmed);
    if path.is_absolute() && path.exists() {
        return true;
    }
    if let Some(rest) = trimmed.strip_prefix('/')
        && let Some(first) = rest.split_whitespace().next()
        && first.contains('/')
    {
        return true;
    }
    false
}

pub(crate) fn lookup(name: &str) -> Option<&'static SlashCommand> {
    let canonical = ALIASES
        .iter()
        .find_map(|(alias, target)| (*alias == name).then_some(*target))
        .unwrap_or(name);
    COMMANDS.iter().find(|cmd| cmd.name == canonical)
}

pub(crate) fn is_known(name: &str) -> bool {
    lookup(name).is_some()
}

/// Fuzzy-match against the command set. Empty query returns all commands in
/// declared order. Otherwise commands are scored by subsequence match and
/// returned highest-first.
pub(crate) fn fuzzy_search(query: &str) -> Vec<&'static SlashCommand> {
    if query.is_empty() {
        return COMMANDS.iter().collect();
    }
    let mut scored: Vec<(i32, &'static SlashCommand)> = COMMANDS
        .iter()
        .filter_map(|cmd| {
            let name_score = fuzzy_score(query, cmd.name).map(|s| s + 50);
            let summary_score = fuzzy_score(query, cmd.summary).map(|s| s / 2);
            let category_score = fuzzy_score(query, cmd.category.label());
            let best = [name_score, summary_score, category_score]
                .into_iter()
                .flatten()
                .max()?;
            Some((best, cmd))
        })
        .collect();
    scored.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    scored.into_iter().map(|(_, cmd)| cmd).collect()
}

/// Subsequence-based fuzzy score. `None` means the query characters do not
/// appear in `candidate` in order. Higher scores are better matches.
pub(crate) fn fuzzy_score(query: &str, candidate: &str) -> Option<i32> {
    let q: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
    if q.is_empty() {
        return Some(0);
    }
    let c: Vec<char> = candidate.chars().flat_map(char::to_lowercase).collect();
    let mut score = 0i32;
    let mut q_idx = 0usize;
    let mut consecutive = 0i32;
    let mut last_match_idx: Option<usize> = None;
    for (i, ch) in c.iter().enumerate() {
        if q_idx >= q.len() {
            break;
        }
        if *ch == q[q_idx] {
            score += 10;
            if i == 0 {
                score += 20;
            }
            if last_match_idx == Some(i.wrapping_sub(1)) {
                consecutive += 1;
                score += consecutive * 5;
            } else {
                consecutive = 0;
            }
            last_match_idx = Some(i);
            q_idx += 1;
        }
    }
    if q_idx < q.len() {
        return None;
    }
    Some(score)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_resolves_aliases() {
        assert_eq!(lookup("q").map(|c| c.name), Some("exit"));
        assert_eq!(lookup("quit").map(|c| c.name), Some("exit"));
        assert_eq!(lookup("h").map(|c| c.name), Some("help"));
        assert_eq!(lookup("retry").map(|c| c.name), Some("retry"));
        assert!(lookup("bogus").is_none());
    }

    #[test]
    fn fuzzy_search_prefers_prefix_match() {
        let results = fuzzy_search("ses");
        assert_eq!(results[0].name, "sessions");
    }

    #[test]
    fn fuzzy_search_matches_subsequence() {
        let results = fuzzy_search("ret");
        assert!(results.iter().any(|cmd| cmd.name == "retry"));
    }

    #[test]
    fn fuzzy_search_empty_returns_all() {
        let results = fuzzy_search("");
        assert_eq!(results.len(), COMMANDS.len());
    }

    #[test]
    fn fuzzy_search_returns_empty_on_no_match() {
        let results = fuzzy_search("zzzz");
        assert!(results.is_empty());
    }

    #[test]
    fn every_command_resolves_via_lookup() {
        // Guards against commands that drift out of sync with `is_known`/dispatch:
        // every entry in COMMANDS must round-trip through `lookup`.
        for cmd in COMMANDS {
            assert_eq!(
                lookup(cmd.name).map(|c| c.name),
                Some(cmd.name),
                "command `{}` does not round-trip through lookup",
                cmd.name
            );
        }
    }

    #[test]
    fn classify_known_command() {
        assert!(matches!(classify_input("/help"), InputClass::Command(_)));
        assert!(matches!(
            classify_input("/skill foo"),
            InputClass::Command(_)
        ));
        assert!(matches!(
            classify_input("/permissions bypass"),
            InputClass::Command("permissions bypass")
        ));
        // Aliases.
        assert!(matches!(classify_input("/q"), InputClass::Command(_)));
        assert!(matches!(classify_input("/h"), InputClass::Command(_)));
    }

    #[test]
    fn classify_nested_path_is_path_even_if_missing() {
        // Heuristic: first segment contains another '/'.
        assert_eq!(
            classify_input("/no/such/dir/here"),
            InputClass::Path,
            "multi-segment slash input should be classified as Path"
        );
        assert_eq!(classify_input("/home/mrmoe28/Project"), InputClass::Path);
    }

    #[test]
    fn classify_existing_absolute_path_is_path() {
        if std::path::Path::new("/tmp").exists() {
            assert_eq!(classify_input("/tmp"), InputClass::Path);
        }
    }

    #[test]
    fn classify_unknown_single_token_slash_is_unknown() {
        match classify_input("/foobar") {
            InputClass::UnknownSlash(name) => assert_eq!(name, "foobar"),
            other => panic!("expected UnknownSlash, got {other:?}"),
        }
    }

    #[test]
    fn classify_plain_prompt() {
        assert_eq!(classify_input("hello world"), InputClass::Prompt);
        assert_eq!(classify_input(""), InputClass::Prompt);
        // Relative paths don't start with '/', so they're Prompt.
        assert_eq!(classify_input("./relative/path"), InputClass::Prompt);
    }

    #[test]
    fn every_registered_command_classifies_as_command() {
        // Guards the contract that the Enter handler relies on: typing
        // `/<name>` for any registered command must classify as Command, so
        // the suggestion palette never hijacks dispatch.
        for cmd in COMMANDS {
            let input = format!("/{}", cmd.name);
            let classified = classify_input(&input);
            match classified {
                InputClass::Command(body) => {
                    assert_eq!(
                        body, cmd.name,
                        "Command body for `{}` should be its name verbatim",
                        cmd.name
                    );
                }
                other => panic!(
                    "command `/{}` should classify as Command, got {:?}",
                    cmd.name, other
                ),
            }
        }
    }

    #[test]
    fn every_alias_classifies_as_command() {
        for (alias, _) in ALIASES {
            let input = format!("/{alias}");
            assert!(
                matches!(classify_input(&input), InputClass::Command(_)),
                "alias `/{alias}` should classify as Command"
            );
        }
    }

    #[test]
    fn commands_with_arguments_classify_with_full_body() {
        // Each case mirrors how a user actually types these commands. The
        // Enter handler dispatches `body` verbatim, so the body must include
        // the arguments — not just the command name.
        for (input, expected_body) in [
            ("/permissions bypass", "permissions bypass"),
            ("/permissions guarded", "permissions guarded"),
            ("/profile default", "profile default"),
            ("/skill clear", "skill clear"),
            ("/skill create demo", "skill create demo"),
            ("/skill my-skill", "skill my-skill"),
            ("/provider set abc123", "provider set abc123"),
            ("/provider clear", "provider clear"),
            ("/bypass on", "bypass on"),
            ("/desktop off", "desktop off"),
            ("/compact 50", "compact 50"),
            ("/retry 4f3a8b", "retry 4f3a8b"),
            ("/open-run abcdef", "open-run abcdef"),
            ("/export out.md", "export out.md"),
            ("/jobs jobs.json 4", "jobs jobs.json 4"),
            ("/smoke Reply exactly: ok", "smoke Reply exactly: ok"),
            // /learn carries an arbitrary note body verbatim — the dispatcher
            // re-joins everything after `save` and writes it as the note.
            (
                "/learn save prefer rg over grep",
                "learn save prefer rg over grep",
            ),
            ("/learn review", "learn review"),
            ("/learn accepted", "learn accepted"),
            ("/learn show ab12", "learn show ab12"),
            ("/learn status", "learn status"),
            ("/learn accept ab12", "learn accept ab12"),
            // Leading/trailing whitespace is stripped before classification.
            ("   /profile default   ", "profile default"),
        ] {
            match classify_input(input) {
                InputClass::Command(body) => assert_eq!(
                    body, expected_body,
                    "input `{input}` should classify with body `{expected_body}`"
                ),
                other => panic!("input `{input}` should classify as Command, got {other:?}"),
            }
        }
    }

    #[test]
    fn unknown_slash_never_classifies_as_command() {
        // Names chosen so they are not registered commands AND extremely
        // unlikely to exist as filesystem paths (would otherwise resolve to
        // Path via the existence check).
        for input in ["/foobar", "/foobar arg1 arg2", "/zzzz_not_a_real_cmd_xyz"] {
            let classified = classify_input(input);
            assert!(
                !matches!(classified, InputClass::Command(_)),
                "input `{input}` must not classify as Command (got {classified:?})"
            );
            assert!(
                matches!(classified, InputClass::UnknownSlash(_)),
                "input `{input}` should classify as UnknownSlash (got {classified:?})"
            );
        }
    }

    #[test]
    fn nested_path_input_classifies_as_path_not_command() {
        for input in [
            "/no/such/path/here",
            "/home/me/project",
            "/var/log/syslog",
            "/etc/hosts",
        ] {
            assert_eq!(
                classify_input(input),
                InputClass::Path,
                "nested absolute path `{input}` should classify as Path"
            );
        }
    }

    #[test]
    fn every_alias_resolves_to_a_known_command() {
        for (alias, target) in ALIASES {
            assert!(
                COMMANDS.iter().any(|cmd| cmd.name == *target),
                "alias `{alias}` -> `{target}` has no matching command entry"
            );
            assert!(
                is_known(alias),
                "alias `{alias}` should be recognized by is_known"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Registry parity tests.
    //
    // These guard the cross-module contract: every entry in COMMANDS must be
    // accounted for in both dispatchers (TUI and plain REPL), and the registry
    // itself must not contain duplicates or aliases that drift out of sync.
    // -----------------------------------------------------------------------

    #[test]
    fn commands_have_no_duplicate_names() {
        let mut seen = std::collections::HashSet::new();
        for cmd in COMMANDS {
            assert!(
                seen.insert(cmd.name),
                "duplicate command entry `{}` in COMMANDS",
                cmd.name
            );
        }
    }

    #[test]
    fn aliases_have_no_duplicates() {
        let mut seen = std::collections::HashSet::new();
        for (alias, _) in ALIASES {
            assert!(seen.insert(*alias), "duplicate alias `{alias}` in ALIASES");
        }
    }

    #[test]
    fn aliases_do_not_collide_with_command_names() {
        // Otherwise `is_known` would be ambiguous about which entry wins.
        for (alias, _) in ALIASES {
            assert!(
                !COMMANDS.iter().any(|cmd| cmd.name == *alias),
                "alias `{alias}` collides with a registered command of the same name"
            );
        }
    }

    #[test]
    fn aliases_point_to_existing_commands() {
        for (alias, target) in ALIASES {
            assert!(
                COMMANDS.iter().any(|cmd| cmd.name == *target),
                "alias `{alias}` -> `{target}` points to a missing command"
            );
        }
    }

    #[test]
    fn every_command_is_dispatched_by_the_tui() {
        let dispatched: std::collections::HashSet<&str> =
            crate::terminal_ui::TUI_DISPATCHED_COMMANDS
                .iter()
                .copied()
                .collect();
        for cmd in COMMANDS {
            if !cmd.tui {
                continue;
            }
            assert!(
                dispatched.contains(cmd.name),
                "command `/{}` declares tui=true but is not in \
                 terminal_ui::TUI_DISPATCHED_COMMANDS — add the dispatch arm \
                 and the constant entry together",
                cmd.name
            );
        }
    }

    #[test]
    fn tui_dispatched_list_only_contains_registered_commands() {
        // Reverse direction: the dispatcher must not claim to handle a name
        // that isn't in COMMANDS (otherwise help/listing would never surface
        // it).
        let registered: std::collections::HashSet<&str> =
            COMMANDS.iter().map(|cmd| cmd.name).collect();
        for name in crate::terminal_ui::TUI_DISPATCHED_COMMANDS {
            assert!(
                registered.contains(*name),
                "TUI_DISPATCHED_COMMANDS includes `{name}` but no SlashCommand \
                 entry exists in COMMANDS"
            );
        }
    }

    #[test]
    fn every_command_is_dispatched_or_marked_tui_only_in_plain_repl() {
        let dispatched: std::collections::HashSet<&str> =
            crate::PLAIN_DISPATCHED_COMMANDS.iter().copied().collect();
        let rejected: std::collections::HashSet<&str> =
            crate::PLAIN_TUI_ONLY_COMMANDS.iter().copied().collect();
        for cmd in COMMANDS {
            if dispatched.contains(cmd.name) {
                assert!(
                    cmd.plain,
                    "command `/{}` is dispatched in plain REPL but its \
                     SlashCommand entry has plain=false",
                    cmd.name
                );
                continue;
            }
            assert!(
                cmd.is_tui_only(),
                "command `/{}` is not handled by the plain REPL and is not \
                 marked TUI-only in COMMANDS (tui={}, plain={})",
                cmd.name,
                cmd.tui,
                cmd.plain
            );
            assert!(
                rejected.contains(cmd.name),
                "command `/{}` is marked TUI-only but the plain REPL doesn't \
                 list it in PLAIN_TUI_ONLY_COMMANDS — its rejection branch \
                 will fall through to the unknown-command arm",
                cmd.name
            );
        }
    }

    #[test]
    fn plain_dispatched_and_tui_only_lists_only_contain_registered_commands() {
        let registered: std::collections::HashSet<&str> =
            COMMANDS.iter().map(|cmd| cmd.name).collect();
        for name in crate::PLAIN_DISPATCHED_COMMANDS {
            assert!(
                registered.contains(*name),
                "PLAIN_DISPATCHED_COMMANDS includes `{name}` but no \
                 SlashCommand entry exists"
            );
        }
        for name in crate::PLAIN_TUI_ONLY_COMMANDS {
            assert!(
                registered.contains(*name),
                "PLAIN_TUI_ONLY_COMMANDS includes `{name}` but no SlashCommand \
                 entry exists"
            );
        }
    }

    #[test]
    fn plain_dispatched_and_tui_only_are_disjoint() {
        // A name can't be both "plain dispatches it" and "plain rejects as
        // TUI-only" — those are mutually exclusive code paths.
        let dispatched: std::collections::HashSet<&str> =
            crate::PLAIN_DISPATCHED_COMMANDS.iter().copied().collect();
        for name in crate::PLAIN_TUI_ONLY_COMMANDS {
            assert!(
                !dispatched.contains(*name),
                "command `{name}` appears in both PLAIN_DISPATCHED_COMMANDS \
                 and PLAIN_TUI_ONLY_COMMANDS"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Enter-routing tests.
    //
    // Exercise the pure `route_enter` helper that both the TUI Enter handler
    // and the plain REPL boil down to. These cover the cases called out in
    // the harness hardening checklist: known commands (with and without args),
    // unknown slashes, paths, plain prompts, and empty input.
    // -----------------------------------------------------------------------

    #[test]
    fn route_enter_dispatches_known_command() {
        assert_eq!(
            route_enter("/help"),
            EnterRoute::Command("help".to_string())
        );
    }

    #[test]
    fn route_enter_dispatches_alias_as_canonical_body() {
        // The body is "q" (the typed token), not "exit" — dispatchers handle
        // both spellings. We just need to confirm Enter routes to Command.
        match route_enter("/q") {
            EnterRoute::Command(body) => assert_eq!(body, "q"),
            other => panic!("expected Command, got {other:?}"),
        }
    }

    #[test]
    fn route_enter_dispatches_permissions_bypass_with_full_body() {
        assert_eq!(
            route_enter("/permissions bypass"),
            EnterRoute::Command("permissions bypass".to_string())
        );
    }

    #[test]
    fn route_enter_dispatches_profile_default_with_full_body() {
        assert_eq!(
            route_enter("/profile default"),
            EnterRoute::Command("profile default".to_string())
        );
    }

    #[test]
    fn route_enter_dispatches_skill_clear_with_full_body() {
        // This case used to be vulnerable to suggestion-pane Enter stealing
        // (because `/skill clear` fuzzy-matches `/skills`). The classifier
        // path returns Command for the full body, so the TUI dispatches
        // verbatim without consulting the palette.
        assert_eq!(
            route_enter("/skill clear"),
            EnterRoute::Command("skill clear".to_string())
        );
    }

    #[test]
    fn route_enter_dispatches_provider_set_with_full_body() {
        assert_eq!(
            route_enter("/provider set abc"),
            EnterRoute::Command("provider set abc".to_string())
        );
    }

    #[test]
    fn route_enter_blocks_unknown_single_token_slash() {
        match route_enter("/foobar") {
            EnterRoute::UnknownSlash(tok) => assert_eq!(tok, "foobar"),
            other => panic!("expected UnknownSlash, got {other:?}"),
        }
    }

    #[test]
    fn route_enter_blocks_unknown_slash_with_args_using_first_token() {
        // Multi-token unknown slash: the hint key is the leading token.
        match route_enter("/foobar arg1 arg2") {
            EnterRoute::UnknownSlash(tok) => assert_eq!(tok, "foobar"),
            other => panic!("expected UnknownSlash, got {other:?}"),
        }
    }

    #[test]
    fn route_enter_submits_nested_path_as_prompt() {
        assert_eq!(
            route_enter("/no/such/path/here"),
            EnterRoute::Submit("/no/such/path/here".to_string())
        );
    }

    #[test]
    fn route_enter_submits_normal_text_as_prompt() {
        assert_eq!(
            route_enter("write me a haiku"),
            EnterRoute::Submit("write me a haiku".to_string())
        );
    }

    #[test]
    fn route_enter_noops_on_empty_input() {
        assert_eq!(route_enter(""), EnterRoute::Noop);
        assert_eq!(route_enter("   "), EnterRoute::Noop);
        assert_eq!(route_enter("\t\n"), EnterRoute::Noop);
    }

    #[test]
    fn route_enter_trims_whitespace_before_classifying() {
        assert_eq!(
            route_enter("   /help   "),
            EnterRoute::Command("help".to_string())
        );
        assert_eq!(
            route_enter("   /profile default   "),
            EnterRoute::Command("profile default".to_string())
        );
    }

    #[test]
    fn route_enter_returns_command_for_known_slash_with_args() {
        // Pins the contract documented on `EnterRoute`: complete slash
        // commands (with or without arguments) classify as Command, so
        // suggestion/autocomplete in the TUI cannot steal Enter from them.
        for input in [
            "/help",
            "/profile default",
            "/permissions bypass",
            "/skill clear",
            "/skill create demo",
            "/provider set abc123",
            "/bypass on",
            "/desktop off",
            "/compact 50",
            "/jobs jobs.json 4",
        ] {
            assert!(
                matches!(route_enter(input), EnterRoute::Command(_)),
                "input `{input}` should route as Command (got {:?})",
                route_enter(input)
            );
        }
    }
}
