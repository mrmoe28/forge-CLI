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
    System,
}

impl Category {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Category::Session => "session",
            Category::Mode => "mode",
            Category::Skills => "skills",
            Category::Runs => "runs",
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
}

pub(crate) const COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "help",
        category: Category::System,
        summary: "show this command list",
        usage: "/help",
        help: "Lists all available slash commands with usage and a one-line summary.",
    },
    SlashCommand {
        name: "status",
        category: Category::System,
        summary: "show current state",
        usage: "/status",
        help: "Shows the current session id, profile, mode flags, active skills, working directory, and the most recent run id.",
    },
    SlashCommand {
        name: "clear",
        category: Category::System,
        summary: "clear visible transcript",
        usage: "/clear",
        help: "Clear the visible terminal transcript. The persisted session transcript on disk is unaffected.",
    },
    SlashCommand {
        name: "exit",
        category: Category::System,
        summary: "quit forge",
        usage: "/exit",
        help: "Save the current session and exit the TUI. /quit and /q are aliases.",
    },
    SlashCommand {
        name: "profile",
        category: Category::Mode,
        summary: "switch agent profile",
        usage: "/profile <name>",
        help: "Switch the active agent profile. Without an argument, prints the current profile. Use /profiles to see configured options.",
    },
    SlashCommand {
        name: "profiles",
        category: Category::Mode,
        summary: "list configured profiles",
        usage: "/profiles",
        help: "Print every profile name configured in the harness config TOML, in declared order.",
    },
    SlashCommand {
        name: "model",
        category: Category::Mode,
        summary: "show the active profile command",
        usage: "/model",
        help: "Show the command line forge runs for each new prompt. Models are baked into the profile command, so this is informational; use /profile to switch to a profile with a different model.",
    },
    SlashCommand {
        name: "permissions",
        category: Category::Mode,
        summary: "show or set permission mode",
        usage: "/permissions [guarded|bypass|desktop]",
        help: "Without an argument, prints the current permission mode. With one, sets the mode: guarded (no bypass), bypass (append the bypass flag), or desktop (bypass + inject the desktop control prompt prefix).",
    },
    SlashCommand {
        name: "bypass",
        category: Category::Mode,
        summary: "toggle dangerous permission bypass",
        usage: "/bypass [on|off]",
        help: "Toggle the per-session bypass flag. When on, forge appends the profile's bypass_args to the agent command.",
    },
    SlashCommand {
        name: "desktop",
        category: Category::Mode,
        summary: "toggle desktop control prompting",
        usage: "/desktop [on|off]",
        help: "Toggle the desktop control prompt prefix. Enabling desktop also enables bypass automatically.",
    },
    SlashCommand {
        name: "skills",
        category: Category::Skills,
        summary: "list discovered skills",
        usage: "/skills",
        help: "Walk the standard skill directories and list every SKILL.md found. Active skills are marked with `*`.",
    },
    SlashCommand {
        name: "skill",
        category: Category::Skills,
        summary: "activate, clear, or create a skill",
        usage: "/skill <name>|clear|create <name>",
        help: "Add a skill body to the prompt prefix for the next turn. `/skill clear` removes all active skills. `/skill create <name>` scaffolds a new skill under <session-cwd>/skills/<name>/SKILL.md.",
    },
    SlashCommand {
        name: "runs",
        category: Category::Runs,
        summary: "list recent runs",
        usage: "/runs",
        help: "Show the most recent agent runs in the runs directory, newest first.",
    },
    SlashCommand {
        name: "last",
        category: Category::Runs,
        summary: "show last response",
        usage: "/last",
        help: "Show the transcript of the most recent run in this session.",
    },
    SlashCommand {
        name: "cancel",
        category: Category::Runs,
        summary: "cancel the active run",
        usage: "/cancel",
        help: "Cancel the currently running agent invocation. This is the slash-command equivalent of pressing Esc while a run is active.",
    },
    SlashCommand {
        name: "retry",
        category: Category::Runs,
        summary: "retry a run",
        usage: "/retry [id]",
        help: "Resubmit the prompt from a prior run. Without an id, retries the last run from this session.",
    },
    SlashCommand {
        name: "new",
        category: Category::Session,
        summary: "start a fresh session",
        usage: "/new",
        help: "Save the current session and start a new one, keeping the current profile, cwd, and mode.",
    },
    SlashCommand {
        name: "resume",
        category: Category::Session,
        summary: "resume a saved session",
        usage: "/resume [id]",
        help: "Load a saved session. Without an id, loads the most recently updated session other than the current one.",
    },
    SlashCommand {
        name: "sessions",
        category: Category::Session,
        summary: "list saved sessions",
        usage: "/sessions",
        help: "Show every persisted session, newest first, with id prefix, profile, turn count, and last-updated timestamp. The current session is marked with `*`.",
    },
    SlashCommand {
        name: "fork",
        category: Category::Session,
        summary: "fork a session into a branch",
        usage: "/fork [id]",
        help: "Create a new session that inherits the transcript and configuration of the current (or named) session.",
    },
    SlashCommand {
        name: "compact",
        category: Category::Session,
        summary: "drop early turns to shrink session",
        usage: "/compact [keep]",
        help: "Keep only the most recent `keep` (default 20) turns in the session transcript. Run records on disk are not deleted.",
    },
    SlashCommand {
        name: "smoke",
        category: Category::Runs,
        summary: "send a quick health-check prompt",
        usage: "/smoke [prompt]",
        help: "Run a minimal prompt against the current profile to verify the agent is wired up. Defaults to `Reply exactly: ok`.",
    },
    SlashCommand {
        name: "inspect",
        category: Category::Runs,
        summary: "show full record for a run",
        usage: "/inspect [id]",
        help: "Print the persisted JSON record for a run, including command line, cwd, exit code, durations, and log paths. Defaults to the most recent run.",
    },
    SlashCommand {
        name: "open-run",
        category: Category::Runs,
        summary: "show transcript for a specific run",
        usage: "/open-run <id>",
        help: "Show the captured stdout/stderr for a specific run id (prefix matches).",
    },
    SlashCommand {
        name: "logs",
        category: Category::Runs,
        summary: "show log file paths for a run",
        usage: "/logs [id]",
        help: "Print the stdout.log and stderr.log paths for a run so you can tail or open them externally.",
    },
    SlashCommand {
        name: "export",
        category: Category::Session,
        summary: "export the current session as markdown",
        usage: "/export <path>",
        help: "Write the active session transcript to <path> as Markdown. Errors if the file already exists.",
    },
    SlashCommand {
        name: "jobs",
        category: Category::Runs,
        summary: "run batch jobs from a JSON or CSV file",
        usage: "/jobs <file> [concurrency]",
        help: "Run every job in <file> against the current profile. The TUI is unresponsive until the batch completes; cancel with Ctrl+C.",
    },
    SlashCommand {
        name: "provider",
        category: Category::Session,
        summary: "inspect or set the provider session id",
        usage: "/provider [show|clear|set <id>]",
        help: "Show, set, or clear the provider session id that forge sends via the profile's continue_args. Without an argument, prints the current id.",
    },
];

/// Aliases handled by `lookup` so that `/q`, `/quit`, `/h` all resolve.
const ALIASES: &[(&str, &str)] = &[("h", "help"), ("quit", "exit"), ("q", "exit")];

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
}
