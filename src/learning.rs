//! Minimal, explicit learning storage for the harness.
//!
//! The learning layer is deliberately small: it lets the user explicitly
//! record short notes (`/learn save <note>`), review them, accept the ones
//! worth keeping, and forget anything that turns out to be wrong. Nothing is
//! captured automatically — no transcript scraping, no embedding, no silent
//! prompt injection. Notes live as Markdown files the user can read with any
//! editor.
//!
//! Layout under `default_learning_dir()` (or the dir passed in):
//!
//! ```text
//! learning/
//!   pending/   <id>.md   — saved but not yet reviewed
//!   accepted/  <id>.md   — reviewed and kept
//!   rejected/  <id>.md   — pending notes the user discarded (audit trail)
//!   events.jsonl         — append-only log of learn_* events
//!   .disabled            — marker file; when present, save refuses
//! ```
//!
//! IDs are uuid v4 prefixes (8 chars) so filenames are stable and safe by
//! construction. User-supplied note bodies never appear in a path.

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use chrono::DateTime;
use chrono::Utc;
use serde::Serialize;
use std::path::Path;
use std::path::PathBuf;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

pub const DEFAULT_LEARNING_DIR: &str = ".codex/external-agent-harness/learning";

const PENDING: &str = "pending";
const ACCEPTED: &str = "accepted";
const REJECTED: &str = "rejected";
const EVENTS_LOG: &str = "events.jsonl";
const DISABLED_MARKER: &str = ".disabled";

/// Default storage dir, parallel to `default_runs_dir`/`default_sessions_dir`.
pub fn default_learning_dir() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    Ok(cwd.join(DEFAULT_LEARNING_DIR))
}

/// State of a stored note.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LearnState {
    Pending,
    Accepted,
    Rejected,
}

impl LearnState {
    fn subdir(self) -> &'static str {
        match self {
            LearnState::Pending => PENDING,
            LearnState::Accepted => ACCEPTED,
            LearnState::Rejected => REJECTED,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            LearnState::Pending => "pending",
            LearnState::Accepted => "accepted",
            LearnState::Rejected => "rejected",
        }
    }
}

/// A single learned note.
#[derive(Debug, Clone)]
pub struct LearnNote {
    pub id: String,
    pub state: LearnState,
    pub created_at: DateTime<Utc>,
    pub body: String,
    pub path: PathBuf,
}

impl LearnNote {
    /// First non-empty body line, trimmed and length-capped, for short list
    /// previews.
    pub fn preview(&self, max_chars: usize) -> String {
        let line = self
            .body
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("");
        if line.chars().count() <= max_chars {
            line.to_string()
        } else {
            let truncated: String = line.chars().take(max_chars).collect();
            format!("{truncated}…")
        }
    }
}

/// Counts and storage path for `/learn status`.
#[derive(Debug, Clone)]
pub struct LearningStatus {
    pub dir: PathBuf,
    pub pending: usize,
    pub accepted: usize,
    pub rejected: usize,
    pub disabled: bool,
}

/// Whether `/learn save` is currently disabled via the `.disabled` marker.
pub async fn is_disabled(dir: &Path) -> bool {
    fs::try_exists(dir.join(DISABLED_MARKER))
        .await
        .unwrap_or(false)
}

/// Toggle the disabled marker. Creates the dir if needed.
pub async fn set_disabled(dir: &Path, disabled: bool) -> Result<()> {
    fs::create_dir_all(dir)
        .await
        .with_context(|| format!("failed to create {}", dir.display()))?;
    let marker = dir.join(DISABLED_MARKER);
    if disabled {
        fs::write(&marker, b"")
            .await
            .with_context(|| format!("failed to write {}", marker.display()))?;
    } else if fs::try_exists(&marker).await.unwrap_or(false) {
        fs::remove_file(&marker)
            .await
            .with_context(|| format!("failed to remove {}", marker.display()))?;
    }
    Ok(())
}

/// Snapshot counts. Missing subdirs count as zero.
pub async fn status(dir: &Path) -> Result<LearningStatus> {
    let pending = count_notes(&dir.join(PENDING)).await?;
    let accepted = count_notes(&dir.join(ACCEPTED)).await?;
    let rejected = count_notes(&dir.join(REJECTED)).await?;
    Ok(LearningStatus {
        dir: dir.to_path_buf(),
        pending,
        accepted,
        rejected,
        disabled: is_disabled(dir).await,
    })
}

/// Create a new pending note from `body`. Returns the stored note.
/// Refuses with an error if learning is disabled or `body` is empty.
pub async fn save_pending(dir: &Path, body: &str) -> Result<LearnNote> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("note body is empty"));
    }
    if is_disabled(dir).await {
        return Err(anyhow!(
            "learning is disabled; run /learn on to re-enable saving"
        ));
    }
    let pending_dir = dir.join(PENDING);
    fs::create_dir_all(&pending_dir)
        .await
        .with_context(|| format!("failed to create {}", pending_dir.display()))?;

    // uuid-based id keeps the filename stable and free of user input.
    let id = short_id_from_uuid(Uuid::new_v4());
    let path = pending_dir.join(format!("{id}.md"));
    if fs::try_exists(&path).await.unwrap_or(false) {
        // Astronomically unlikely with 8 hex chars but guard anyway.
        return Err(anyhow!("id collision for `{id}`; retry"));
    }
    let created_at = Utc::now();
    let serialized = serialize_note(&id, LearnState::Pending, created_at, trimmed);
    fs::write(&path, serialized.as_bytes())
        .await
        .with_context(|| format!("failed to write {}", path.display()))?;

    let note = LearnNote {
        id: id.clone(),
        state: LearnState::Pending,
        created_at,
        body: trimmed.to_string(),
        path,
    };
    append_event(dir, "learn_saved", &note).await?;
    Ok(note)
}

/// List pending notes, newest first by creation time.
pub async fn list_pending(dir: &Path) -> Result<Vec<LearnNote>> {
    list_state(dir, LearnState::Pending).await
}

/// List accepted notes, newest first.
pub async fn list_accepted(dir: &Path) -> Result<Vec<LearnNote>> {
    list_state(dir, LearnState::Accepted).await
}

/// Find a note by prefix across pending, accepted, then rejected notes.
pub async fn show(dir: &Path, id_prefix: &str) -> Result<LearnNote> {
    if !is_safe_id(id_prefix) {
        return Err(anyhow!(
            "invalid id `{id_prefix}`: only [a-z0-9-] characters allowed"
        ));
    }

    let mut matches = Vec::new();
    for state in [
        LearnState::Pending,
        LearnState::Accepted,
        LearnState::Rejected,
    ] {
        matches.extend(
            list_state(dir, state)
                .await?
                .into_iter()
                .filter(|note| note.id.starts_with(id_prefix)),
        );
    }

    match matches.len() {
        0 => Err(anyhow!("no learned note matches `{id_prefix}`")),
        1 => Ok(matches.remove(0)),
        n => Err(anyhow!(
            "id prefix `{id_prefix}` is ambiguous; matched {n} notes"
        )),
    }
}

/// Move a pending note (`id_prefix` matches the start of an id) to accepted.
pub async fn accept(dir: &Path, id_prefix: &str) -> Result<LearnNote> {
    move_note(dir, id_prefix, LearnState::Pending, LearnState::Accepted).await
}

/// Move a pending note to the rejected area.
pub async fn reject(dir: &Path, id_prefix: &str) -> Result<LearnNote> {
    move_note(dir, id_prefix, LearnState::Pending, LearnState::Rejected).await
}

/// Permanently delete an accepted note.
pub async fn forget(dir: &Path, id_prefix: &str) -> Result<LearnNote> {
    let note = find_note(dir, id_prefix, LearnState::Accepted).await?;
    fs::remove_file(&note.path)
        .await
        .with_context(|| format!("failed to delete {}", note.path.display()))?;
    append_event(dir, "learn_forgotten", &note).await?;
    Ok(note)
}

/// Run a `/learn ...` subcommand against `dir` and return the lines that
/// should be shown to the user. Shared by the plain REPL and the TUI so both
/// surfaces print the same thing.
///
/// `args` is the whitespace-split tail after `learn` (e.g. for
/// `/learn save prefer rg`, `args == ["save", "prefer", "rg"]`).
///
/// On errors this returns a one-line message rather than propagating —
/// `/learn` is a UX command, not a hard failure point.
pub async fn handle_command(dir: &Path, args: &[&str]) -> Vec<String> {
    match args.first().copied() {
        None | Some("status") => match status(dir).await {
            Ok(s) => vec![
                format!("learning dir: {}", s.dir.display()),
                format!(
                    "pending: {}  accepted: {}  rejected: {}  disabled: {}",
                    s.pending, s.accepted, s.rejected, s.disabled
                ),
                format!(
                    "subcommands: save <note> | review | accepted | show <id> | accept <id> | reject <id> | forget <id> | on | off"
                ),
            ],
            Err(err) => vec![format!("/learn status failed: {err}")],
        },
        Some("save") => {
            let body = args[1..].join(" ");
            if body.trim().is_empty() {
                return vec!["Usage: /learn save <note>".to_string()];
            }
            match save_pending(dir, &body).await {
                Ok(note) => vec![
                    format!("saved pending note {}", note.id),
                    format!("path: {}", note.path.display()),
                    "review with /learn review; keep with /learn accept <id>".to_string(),
                ],
                Err(err) => vec![format!("/learn save failed: {err}")],
            }
        }
        Some("review") => match list_pending(dir).await {
            Ok(notes) if notes.is_empty() => vec!["no pending notes".to_string()],
            Ok(notes) => {
                let mut lines = format_note_list("pending", notes);
                lines.push(
                    "accept with /learn accept <id> or discard with /learn reject <id>".to_string(),
                );
                lines
            }
            Err(err) => vec![format!("/learn review failed: {err}")],
        },
        Some("accepted") => match list_accepted(dir).await {
            Ok(notes) if notes.is_empty() => vec!["no accepted notes".to_string()],
            Ok(notes) => {
                let mut lines = format_note_list("accepted", notes);
                lines.push(
                    "inspect with /learn show <id>; remove with /learn forget <id>".to_string(),
                );
                lines
            }
            Err(err) => vec![format!("/learn accepted failed: {err}")],
        },
        Some("show") => match args.get(1) {
            None => vec!["Usage: /learn show <id>".to_string()],
            Some(id) => match show(dir, id).await {
                Ok(note) => format_note_detail(&note),
                Err(err) => vec![format!("/learn show failed: {err}")],
            },
        },
        Some("accept") => match args.get(1) {
            None => vec!["Usage: /learn accept <id>".to_string()],
            Some(id) => match accept(dir, id).await {
                Ok(note) => vec![
                    format!("accepted {}", note.id),
                    format!("path: {}", note.path.display()),
                ],
                Err(err) => vec![format!("/learn accept failed: {err}")],
            },
        },
        Some("reject") => match args.get(1) {
            None => vec!["Usage: /learn reject <id>".to_string()],
            Some(id) => match reject(dir, id).await {
                Ok(note) => vec![format!("rejected {} (moved to rejected/)", note.id)],
                Err(err) => vec![format!("/learn reject failed: {err}")],
            },
        },
        Some("forget") => match args.get(1) {
            None => vec!["Usage: /learn forget <id>".to_string()],
            Some(id) => match forget(dir, id).await {
                Ok(note) => vec![format!("forgot {} (deleted)", note.id)],
                Err(err) => vec![format!("/learn forget failed: {err}")],
            },
        },
        Some("off") | Some("disable") => match set_disabled(dir, true).await {
            Ok(()) => vec!["learning saves disabled (toggle with /learn on)".to_string()],
            Err(err) => vec![format!("/learn off failed: {err}")],
        },
        Some("on") | Some("enable") => match set_disabled(dir, false).await {
            Ok(()) => vec!["learning saves enabled".to_string()],
            Err(err) => vec![format!("/learn on failed: {err}")],
        },
        Some(other) => vec![format!("unknown /learn subcommand `{other}`; see /help")],
    }
}

fn format_note_list(label: &str, notes: Vec<LearnNote>) -> Vec<String> {
    let mut lines = vec![format!("{} {label} note(s):", notes.len())];
    for note in notes {
        lines.push(format!(
            "  {}  {}  {}",
            note.id,
            note.created_at.format("%Y-%m-%d %H:%M"),
            note.preview(72)
        ));
    }
    lines
}

fn format_note_detail(note: &LearnNote) -> Vec<String> {
    let mut lines = vec![
        format!("learned note {}", note.id),
        format!("status: {}", note.state.as_str()),
        format!(
            "created_at: {}",
            note.created_at
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        ),
        format!("path: {}", note.path.display()),
        String::new(),
    ];
    lines.extend(note.body.lines().map(str::to_string));
    lines
}

// ----------------------------------------------------------------------------
// internals
// ----------------------------------------------------------------------------

async fn count_notes(dir: &Path) -> Result<usize> {
    let mut entries = match fs::read_dir(dir).await {
        Ok(e) => e,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(err).with_context(|| format!("failed to read {}", dir.display())),
    };
    let mut count = 0usize;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("md") {
            count += 1;
        }
    }
    Ok(count)
}

async fn list_state(dir: &Path, state: LearnState) -> Result<Vec<LearnNote>> {
    let sub = dir.join(state.subdir());
    let mut entries = match fs::read_dir(&sub).await {
        Ok(e) => e,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("failed to read {}", sub.display())),
    };
    let mut notes = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if !is_safe_id(stem) {
            continue;
        }
        let Ok(body_raw) = fs::read_to_string(&path).await else {
            continue;
        };
        let parsed = parse_note(stem, state, &path, &body_raw);
        notes.push(parsed);
    }
    notes.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(notes)
}

async fn move_note(
    dir: &Path,
    id_prefix: &str,
    from: LearnState,
    to: LearnState,
) -> Result<LearnNote> {
    let existing = find_note(dir, id_prefix, from).await?;
    let dest_dir = dir.join(to.subdir());
    fs::create_dir_all(&dest_dir)
        .await
        .with_context(|| format!("failed to create {}", dest_dir.display()))?;
    let dest = dest_dir.join(format!("{}.md", existing.id));
    if fs::try_exists(&dest).await.unwrap_or(false) {
        return Err(anyhow!(
            "destination already exists: {} (refusing to overwrite)",
            dest.display()
        ));
    }
    // Rewrite with updated state header rather than a bare rename so the file
    // on disk reflects its new state.
    let serialized = serialize_note(&existing.id, to, existing.created_at, &existing.body);
    fs::write(&dest, serialized.as_bytes())
        .await
        .with_context(|| format!("failed to write {}", dest.display()))?;
    fs::remove_file(&existing.path)
        .await
        .with_context(|| format!("failed to remove {}", existing.path.display()))?;

    let moved = LearnNote {
        id: existing.id,
        state: to,
        created_at: existing.created_at,
        body: existing.body,
        path: dest,
    };
    let event = match to {
        LearnState::Accepted => "learn_accepted",
        LearnState::Rejected => "learn_rejected",
        LearnState::Pending => "learn_repended",
    };
    append_event(dir, event, &moved).await?;
    Ok(moved)
}

async fn find_note(dir: &Path, id_prefix: &str, state: LearnState) -> Result<LearnNote> {
    if !is_safe_id(id_prefix) {
        return Err(anyhow!(
            "invalid id `{id_prefix}`: only [a-z0-9-] characters allowed"
        ));
    }
    let notes = list_state(dir, state).await?;
    let mut matches: Vec<LearnNote> = notes
        .into_iter()
        .filter(|n| n.id.starts_with(id_prefix))
        .collect();
    match matches.len() {
        0 => Err(anyhow!("no {} note matches `{id_prefix}`", state.as_str())),
        1 => Ok(matches.remove(0)),
        n => Err(anyhow!(
            "id prefix `{id_prefix}` is ambiguous; matched {n} {} notes",
            state.as_str()
        )),
    }
}

fn serialize_note(id: &str, state: LearnState, created_at: DateTime<Utc>, body: &str) -> String {
    let mut out = String::new();
    out.push_str("---\n");
    out.push_str(&format!("id: {id}\n"));
    out.push_str(&format!(
        "created_at: {}\n",
        created_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    ));
    out.push_str(&format!("status: {}\n", state.as_str()));
    out.push_str("---\n\n");
    out.push_str(body.trim_end());
    out.push('\n');
    out
}

fn parse_note(id: &str, fallback_state: LearnState, path: &Path, raw: &str) -> LearnNote {
    let (created_at, body) = match strip_frontmatter(raw) {
        Some((meta, body)) => {
            let created_at = meta
                .iter()
                .find_map(|(k, v)| (k == "created_at").then_some(v.as_str()))
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(Utc::now);
            (created_at, body.to_string())
        }
        None => (Utc::now(), raw.to_string()),
    };
    LearnNote {
        id: id.to_string(),
        state: fallback_state,
        created_at,
        body: body.trim().to_string(),
        path: path.to_path_buf(),
    }
}

fn strip_frontmatter(raw: &str) -> Option<(Vec<(String, String)>, &str)> {
    let rest = raw.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    let header = &rest[..end];
    let body_start = end + "\n---".len();
    let body = &rest[body_start..];
    let body = body.strip_prefix('\n').unwrap_or(body);
    let body = body.strip_prefix('\n').unwrap_or(body);
    let mut meta = Vec::new();
    for line in header.lines() {
        if let Some((k, v)) = line.split_once(':') {
            meta.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    Some((meta, body))
}

fn short_id_from_uuid(uuid: Uuid) -> String {
    uuid.simple().to_string().chars().take(8).collect()
}

/// Restrict ids to lowercase hex/dash so prefix matching can't traverse paths
/// or pull in unexpected files.
fn is_safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

#[derive(Serialize)]
struct EventRecord<'a> {
    event: &'a str,
    id: &'a str,
    state: &'a str,
    timestamp: String,
}

async fn append_event(dir: &Path, event: &str, note: &LearnNote) -> Result<()> {
    fs::create_dir_all(dir)
        .await
        .with_context(|| format!("failed to create {}", dir.display()))?;
    let path = dir.join(EVENTS_LOG);
    let record = EventRecord {
        event,
        id: &note.id,
        state: note.state.as_str(),
        timestamp: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    };
    let mut line = serde_json::to_string(&record)?;
    line.push('\n');
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await
        .with_context(|| format!("failed to open {}", path.display()))?;
    file.write_all(line.as_bytes())
        .await
        .with_context(|| format!("failed to append to {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn save_pending_creates_markdown_file_and_event() -> Result<()> {
        let temp = TempDir::new()?;
        let dir = temp.path().to_path_buf();
        let note = save_pending(&dir, "  prefer rg over grep for repo scans  ").await?;
        assert_eq!(note.state, LearnState::Pending);
        assert!(note.path.starts_with(dir.join(PENDING)));
        let body = fs::read_to_string(&note.path).await?;
        assert!(body.starts_with("---\n"), "expected frontmatter:\n{body}");
        assert!(body.contains("status: pending"));
        assert!(body.contains("prefer rg over grep"));
        // Event log should contain learn_saved.
        let log = fs::read_to_string(dir.join(EVENTS_LOG)).await?;
        assert!(log.contains("learn_saved"), "events.jsonl: {log}");
        assert!(log.contains(&note.id));
        Ok(())
    }

    #[tokio::test]
    async fn empty_body_is_rejected() {
        let temp = TempDir::new().unwrap();
        let err = save_pending(temp.path(), "   ").await.unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[tokio::test]
    async fn list_pending_sorts_newest_first_and_skips_unsafe_ids() -> Result<()> {
        let temp = TempDir::new()?;
        let dir = temp.path().to_path_buf();
        let a = save_pending(&dir, "first note").await?;
        // Force the second note's created_at to be later than the first.
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        let b = save_pending(&dir, "second note").await?;
        // Drop a junk file with a stem that fails is_safe_id (uppercase);
        // list_state should silently skip it.
        let bad = dir.join(PENDING).join("BADSTEM.md");
        fs::write(&bad, "---\nid: BADSTEM\n---\nignored").await?;
        let listed = list_pending(&dir).await?;
        assert_eq!(listed.len(), 2, "junk file should be skipped");
        assert_eq!(listed[0].id, b.id, "newest should be first");
        assert_eq!(listed[1].id, a.id);
        Ok(())
    }

    #[tokio::test]
    async fn accept_moves_pending_to_accepted_with_new_status() -> Result<()> {
        let temp = TempDir::new()?;
        let dir = temp.path().to_path_buf();
        let note = save_pending(&dir, "kept body").await?;
        let accepted = accept(&dir, &note.id).await?;
        assert_eq!(accepted.state, LearnState::Accepted);
        assert!(accepted.path.starts_with(dir.join(ACCEPTED)));
        assert!(!fs::try_exists(&note.path).await?);
        let stored = fs::read_to_string(&accepted.path).await?;
        assert!(stored.contains("status: accepted"));
        assert!(stored.contains("kept body"));
        assert!(list_pending(&dir).await?.is_empty());
        assert_eq!(list_accepted(&dir).await?.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn accept_by_prefix_matches_uniquely() -> Result<()> {
        let temp = TempDir::new()?;
        let dir = temp.path().to_path_buf();
        let note = save_pending(&dir, "by prefix").await?;
        // Use just the first 4 chars; should match uniquely.
        let prefix: String = note.id.chars().take(4).collect();
        let accepted = accept(&dir, &prefix).await?;
        assert_eq!(accepted.id, note.id);
        Ok(())
    }

    #[tokio::test]
    async fn reject_moves_pending_to_rejected() -> Result<()> {
        let temp = TempDir::new()?;
        let dir = temp.path().to_path_buf();
        let note = save_pending(&dir, "discard me").await?;
        let rejected = reject(&dir, &note.id).await?;
        assert_eq!(rejected.state, LearnState::Rejected);
        assert!(!fs::try_exists(&note.path).await?);
        assert!(rejected.path.starts_with(dir.join(REJECTED)));
        assert!(list_pending(&dir).await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn forget_deletes_accepted_note() -> Result<()> {
        let temp = TempDir::new()?;
        let dir = temp.path().to_path_buf();
        let note = save_pending(&dir, "fleeting").await?;
        let accepted = accept(&dir, &note.id).await?;
        let forgotten = forget(&dir, &accepted.id).await?;
        assert_eq!(forgotten.id, accepted.id);
        assert!(!fs::try_exists(&accepted.path).await?);
        assert!(list_accepted(&dir).await?.is_empty());
        // Event log records the forget.
        let log = fs::read_to_string(dir.join(EVENTS_LOG)).await?;
        assert!(log.contains("learn_forgotten"));
        Ok(())
    }

    #[tokio::test]
    async fn show_finds_note_across_states_and_returns_detail_lines() -> Result<()> {
        let temp = TempDir::new()?;
        let dir = temp.path().to_path_buf();
        let note = save_pending(&dir, "remember this\nsecond line").await?;
        let accepted = accept(&dir, &note.id).await?;

        let shown = show(&dir, &accepted.id[..4]).await?;
        assert_eq!(shown.id, accepted.id);
        assert_eq!(shown.state, LearnState::Accepted);

        let lines = handle_command(&dir, &["show", &accepted.id[..4]]).await;
        assert!(lines.iter().any(|line| line == "status: accepted"));
        assert!(lines.iter().any(|line| line == "remember this"));
        assert!(lines.iter().any(|line| line == "second line"));
        Ok(())
    }

    #[tokio::test]
    async fn accepted_subcommand_lists_accepted_notes() -> Result<()> {
        let temp = TempDir::new()?;
        let dir = temp.path().to_path_buf();
        let note = save_pending(&dir, "accepted preview body").await?;
        let accepted = accept(&dir, &note.id).await?;

        let lines = handle_command(&dir, &["accepted"]).await;
        assert!(lines[0].contains("1 accepted note"));
        assert!(
            lines
                .iter()
                .any(|line| line.contains(&accepted.id) && line.contains("accepted preview body")),
            "accepted output: {lines:?}"
        );
        assert!(lines.iter().any(|line| line.contains("/learn show")));
        Ok(())
    }

    #[tokio::test]
    async fn unsafe_id_is_rejected_at_lookup() {
        let temp = TempDir::new().unwrap();
        let err = accept(temp.path(), "../etc").await.unwrap_err();
        assert!(err.to_string().contains("invalid id"));
        let err = forget(temp.path(), "a/b").await.unwrap_err();
        assert!(err.to_string().contains("invalid id"));
        let lines = handle_command(temp.path(), &["show", "../etc"]).await;
        assert!(lines[0].contains("invalid id"));
    }

    #[tokio::test]
    async fn ambiguous_prefix_errors_instead_of_matching() -> Result<()> {
        let temp = TempDir::new()?;
        let dir = temp.path().to_path_buf();
        // Hand-craft two pending notes with overlapping prefixes to drive the
        // ambiguity branch deterministically; uuid prefixes are
        // probabilistically unique so we can't rely on natural collisions.
        let pending_dir = dir.join(PENDING);
        fs::create_dir_all(&pending_dir).await?;
        for id in ["aa111111", "aa222222"] {
            let body = serialize_note(id, LearnState::Pending, Utc::now(), "body");
            fs::write(pending_dir.join(format!("{id}.md")), body).await?;
        }
        let err = accept(&dir, "aa").await.unwrap_err();
        assert!(err.to_string().contains("ambiguous"), "{err}");
        Ok(())
    }

    #[tokio::test]
    async fn status_counts_and_reports_dir() -> Result<()> {
        let temp = TempDir::new()?;
        let dir = temp.path().to_path_buf();
        let s0 = status(&dir).await?;
        assert_eq!(s0.pending, 0);
        assert_eq!(s0.accepted, 0);
        assert_eq!(s0.rejected, 0);
        assert!(!s0.disabled);

        let a = save_pending(&dir, "a").await?;
        let b = save_pending(&dir, "b").await?;
        accept(&dir, &a.id).await?;
        reject(&dir, &b.id).await?;
        let s1 = status(&dir).await?;
        assert_eq!(s1.pending, 0);
        assert_eq!(s1.accepted, 1);
        assert_eq!(s1.rejected, 1);
        assert_eq!(s1.dir, dir);
        Ok(())
    }

    #[tokio::test]
    async fn disable_blocks_new_saves_until_re_enabled() -> Result<()> {
        let temp = TempDir::new()?;
        let dir = temp.path().to_path_buf();
        set_disabled(&dir, true).await?;
        assert!(is_disabled(&dir).await);
        let err = save_pending(&dir, "blocked").await.unwrap_err();
        assert!(err.to_string().contains("disabled"));
        set_disabled(&dir, false).await?;
        assert!(!is_disabled(&dir).await);
        save_pending(&dir, "now allowed").await?;
        Ok(())
    }

    #[test]
    fn is_safe_id_rejects_path_traversal_and_special_chars() {
        assert!(is_safe_id("a1b2c3d4"));
        assert!(is_safe_id("0000ffff"));
        assert!(!is_safe_id(""));
        assert!(!is_safe_id("../etc"));
        assert!(!is_safe_id("a/b"));
        assert!(!is_safe_id("A1B2C3D4")); // uppercase rejected for stable matching
        assert!(!is_safe_id("a b"));
        assert!(!is_safe_id(&"a".repeat(65)));
    }

    #[test]
    fn preview_truncates_long_lines() {
        let note = LearnNote {
            id: "abc".to_string(),
            state: LearnState::Pending,
            created_at: Utc::now(),
            body: "first line that is fairly long\nsecond".to_string(),
            path: PathBuf::from("/tmp/x"),
        };
        let short = note.preview(10);
        assert!(short.ends_with('…'));
        assert_eq!(short.chars().count(), 11); // 10 + ellipsis
        let long = note.preview(200);
        assert_eq!(long, "first line that is fairly long");
    }
}
