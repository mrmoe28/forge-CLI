use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use chrono::DateTime;
use chrono::Utc;
use futures::StreamExt;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::fs;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::process::Command;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio::sync::mpsc::UnboundedSender;
use uuid::Uuid;

pub const DEFAULT_RUNS_DIR: &str = ".codex/external-agent-harness/runs";
pub const DEFAULT_SESSIONS_DIR: &str = ".codex/external-agent-harness/sessions";
const DEFAULT_PROFILE: &str = "default";

#[derive(Debug, Clone, Deserialize)]
pub struct HarnessConfig {
    #[serde(default)]
    pub profiles: BTreeMap<String, AgentProfile>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentProfile {
    pub command: Vec<String>,
    #[serde(default)]
    pub bypass_args: Vec<String>,
    #[serde(default)]
    pub desktop_args: Vec<String>,
    #[serde(default)]
    pub desktop_prompt_prefix: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[serde(default = "default_prompt_arg")]
    pub prompt_arg: bool,
    /// Args appended when the session has a known provider session id. Each
    /// occurrence of `{session_id}` is replaced with the provider session id.
    /// Example for `opencode`: `["--session", "{session_id}"]`.
    #[serde(default)]
    pub continue_args: Vec<String>,
    /// When set, forge scans each captured stdout line for `prefix` and, on a
    /// match, treats the remainder of the line as the agent's session id and
    /// stores it on the [`Session`] so subsequent turns can use
    /// `continue_args`.
    #[serde(default)]
    pub session_id_capture_prefix: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RunRequest {
    pub profile: String,
    pub prompt: String,
    pub label: Option<String>,
    pub cwd: Option<PathBuf>,
    pub timeout_secs: Option<u64>,
    pub bypass_permissions: bool,
    pub desktop_control: bool,
    pub prompt_prefix: Option<String>,
    /// When set, the profile's `continue_args` are appended so the agent
    /// resumes the existing conversation rather than starting a new one.
    pub provider_session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    pub id: String,
    pub profile: String,
    pub label: Option<String>,
    pub prompt: String,
    pub command: Vec<String>,
    pub cwd: PathBuf,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub duration_ms: u128,
    pub timeout_secs: Option<u64>,
    pub status: RunStatus,
    pub exit_code: Option<i32>,
    pub stdout_log: PathBuf,
    pub stderr_log: PathBuf,
    /// Captured from a stdout line matching the profile's
    /// `session_id_capture_prefix`, if any.
    #[serde(default)]
    pub captured_session_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Succeeded,
    Failed,
    TimedOut,
}

/// Live event emitted by [`run_agent_streaming`].
///
/// `Started` is fired immediately after the subprocess is spawned. `Stdout` /
/// `Stderr` are line-buffered text chunks (newline-stripped). `Completed`
/// fires exactly once, after both pipes have closed and the [`RunRecord`] has
/// been persisted to disk.
#[derive(Debug, Clone)]
pub enum RunEvent {
    Started(RunStarted),
    Stdout(String),
    Stderr(String),
    Completed(Box<RunRecord>),
}

#[derive(Debug, Clone)]
pub struct RunStarted {
    pub id: String,
    pub command: Vec<String>,
    pub cwd: PathBuf,
    pub started_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy)]
enum StreamKind {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Deserialize)]
pub struct JobSpec {
    pub prompt: String,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub bypass_permissions: bool,
    #[serde(default)]
    pub desktop_control: bool,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub skills: Vec<String>,
}

/// A discovered skill. `body` is the markdown after any YAML frontmatter, and
/// is what is injected into the agent prompt when the skill is active or its
/// triggers match the current prompt.
#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub path: PathBuf,
    pub title: Option<String>,
    pub description: Option<String>,
    pub triggers: Vec<String>,
    pub body: String,
}

impl Skill {
    /// Returns true when any of the skill's trigger phrases appears as a
    /// case-insensitive substring of `prompt`. A skill without triggers never
    /// auto-matches; it must be manually activated.
    pub fn matches_prompt(&self, prompt: &str) -> bool {
        if self.triggers.is_empty() {
            return false;
        }
        let lower = prompt.to_lowercase();
        self.triggers
            .iter()
            .any(|trigger| lower.contains(&trigger.to_lowercase()))
    }

    /// One-line label for UI listings. Prefers the YAML `description`, then
    /// the first markdown `# ...` title, then falls back to the bare name.
    pub fn summary(&self) -> &str {
        self.description
            .as_deref()
            .or(self.title.as_deref())
            .unwrap_or(self.name.as_str())
    }
}

/// A persistent interactive session. Carries the configuration the user has
/// dialed in (profile, cwd, bypass/desktop, active skills) plus the
/// conversation transcript. Sessions are written to
/// `<sessions_dir>/<id>.json` after each turn so they can be resumed across
/// restarts and forked into branches.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    pub profile: String,
    pub cwd: PathBuf,
    #[serde(default)]
    pub bypass: bool,
    #[serde(default)]
    pub desktop: bool,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub active_skills: Vec<String>,
    #[serde(default)]
    pub transcript: Vec<SessionTurn>,
    #[serde(default)]
    pub run_ids: Vec<String>,
    #[serde(default)]
    pub provider_session_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum SessionTurn {
    User {
        text: String,
        at: DateTime<Utc>,
    },
    Assistant {
        text: String,
        run_id: String,
        at: DateTime<Utc>,
    },
    System {
        text: String,
        at: DateTime<Utc>,
    },
}

impl Session {
    pub fn new(profile: String, cwd: PathBuf) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4().to_string(),
            name: None,
            profile,
            cwd,
            bypass: false,
            desktop: false,
            timeout_secs: None,
            active_skills: Vec::new(),
            transcript: Vec::new(),
            run_ids: Vec::new(),
            provider_session_id: None,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn record_user(&mut self, text: String) {
        let now = Utc::now();
        self.transcript.push(SessionTurn::User { text, at: now });
        self.updated_at = now;
    }

    pub fn record_assistant(&mut self, text: String, run_id: String) {
        let now = Utc::now();
        if !self.run_ids.contains(&run_id) {
            self.run_ids.push(run_id.clone());
        }
        self.transcript.push(SessionTurn::Assistant {
            text,
            run_id,
            at: now,
        });
        self.updated_at = now;
    }

    pub fn record_system(&mut self, text: String) {
        let now = Utc::now();
        self.transcript.push(SessionTurn::System { text, at: now });
        self.updated_at = now;
    }

    pub fn fork(&self) -> Self {
        let now = Utc::now();
        let mut next = self.clone();
        next.id = Uuid::new_v4().to_string();
        next.name = match &self.name {
            Some(name) => Some(format!("{name} (fork)")),
            None => Some(format!("fork of {}", short_id(&self.id))),
        };
        next.created_at = now;
        next.updated_at = now;
        next
    }

    /// Short, ergonomic prefix used in UI listings.
    pub fn short_id(&self) -> String {
        short_id(&self.id)
    }

    pub fn turn_count(&self) -> usize {
        self.transcript.len()
    }
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

pub fn default_sessions_dir() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    Ok(cwd.join(DEFAULT_SESSIONS_DIR))
}

pub async fn save_session(sessions_dir: &Path, session: &Session) -> Result<()> {
    fs::create_dir_all(sessions_dir)
        .await
        .with_context(|| format!("failed to create {}", sessions_dir.display()))?;
    let path = session_path(sessions_dir, &session.id);
    let body = serde_json::to_vec_pretty(session)?;
    fs::write(&path, body)
        .await
        .with_context(|| format!("failed to write {}", path.display()))
}

pub async fn load_session(sessions_dir: &Path, id_or_prefix: &str) -> Result<Session> {
    let path = resolve_session_path(sessions_dir, id_or_prefix).await?;
    let body = fs::read_to_string(&path)
        .await
        .with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&body).with_context(|| format!("failed to parse {}", path.display()))
}

pub async fn list_sessions(sessions_dir: &Path) -> Result<Vec<Session>> {
    let mut entries = match fs::read_dir(sessions_dir).await {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(err).with_context(|| format!("failed to read {}", sessions_dir.display()));
        }
    };
    let mut sessions = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Ok(body) = fs::read_to_string(&path).await else {
            continue;
        };
        let Ok(session) = serde_json::from_str::<Session>(&body) else {
            continue;
        };
        sessions.push(session);
    }
    sessions.sort_by_key(|session| session.updated_at);
    sessions.reverse();
    Ok(sessions)
}

pub async fn delete_session(sessions_dir: &Path, id_or_prefix: &str) -> Result<()> {
    let path = resolve_session_path(sessions_dir, id_or_prefix).await?;
    fs::remove_file(&path)
        .await
        .with_context(|| format!("failed to delete {}", path.display()))
}

fn session_path(sessions_dir: &Path, id: &str) -> PathBuf {
    sessions_dir.join(format!("{id}.json"))
}

async fn resolve_session_path(sessions_dir: &Path, id_or_prefix: &str) -> Result<PathBuf> {
    let exact = session_path(sessions_dir, id_or_prefix);
    if fs::try_exists(&exact).await.unwrap_or(false) {
        return Ok(exact);
    }
    let mut matches: Vec<PathBuf> = Vec::new();
    let mut entries = match fs::read_dir(sessions_dir).await {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(anyhow!("no sessions found in {}", sessions_dir.display()));
        }
        Err(err) => {
            return Err(err)
                .with_context(|| format!("failed to read {}", sessions_dir.display()));
        }
    };
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        if stem.starts_with(id_or_prefix) {
            matches.push(path);
        }
    }
    match matches.len() {
        0 => Err(anyhow!("no session matches `{id_or_prefix}`")),
        1 => Ok(matches.remove(0)),
        _ => Err(anyhow!(
            "session prefix `{id_or_prefix}` is ambiguous; matched {} sessions",
            matches.len()
        )),
    }
}

pub async fn load_config(path: Option<&Path>) -> Result<HarnessConfig> {
    let Some(path) = path else {
        return Ok(default_config());
    };
    let body = fs::read_to_string(path)
        .await
        .with_context(|| format!("failed to read config {}", path.display()))?;
    let config = toml::from_str::<HarnessConfig>(&body)
        .with_context(|| format!("failed to parse config {}", path.display()))?;
    Ok(config.with_default_profile())
}

pub fn default_runs_dir() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    Ok(cwd.join(DEFAULT_RUNS_DIR))
}

pub async fn discover_skills(cwd: &Path) -> Result<Vec<Skill>> {
    let mut roots = vec![
        cwd.join(".agents/skills"),
        cwd.join("skills"),
        cwd.join(".opencode/skills"),
        cwd.join(".claude/skills"),
        cwd.join(".cursor/skills"),
    ];
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        roots.extend([
            home.join(".agents/skills"),
            home.join(".codex/skills"),
            home.join(".opencode/skills"),
            home.join(".claude/skills"),
            home.join(".cursor/skills"),
        ]);
    }

    let mut seen = HashSet::new();
    let mut skills = Vec::new();
    for root in roots {
        let mut entries = match fs::read_dir(&root).await {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => {
                return Err(err).with_context(|| format!("failed to read {}", root.display()));
            }
        };
        while let Some(entry) = entries.next_entry().await? {
            if !entry.file_type().await?.is_dir() {
                continue;
            }
            let skill_path = entry.path().join("SKILL.md");
            if !seen.insert(skill_path.clone()) {
                continue;
            }
            let Ok(raw) = fs::read_to_string(&skill_path).await else {
                continue;
            };
            let name = entry.file_name().to_string_lossy().to_string();
            let parsed = parse_skill_file(&raw);
            skills.push(Skill {
                name,
                path: skill_path,
                title: parsed.title,
                description: parsed.description,
                triggers: parsed.triggers,
                body: parsed.body,
            });
        }
    }
    skills.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(skills)
}

pub async fn create_skill(cwd: &Path, name: &str) -> Result<Skill> {
    let name = sanitize_skill_name(name)?;
    let skill_dir = cwd.join("skills").join(&name);
    let skill_path = skill_dir.join("SKILL.md");
    fs::create_dir_all(&skill_dir)
        .await
        .with_context(|| format!("failed to create {}", skill_dir.display()))?;
    if fs::try_exists(&skill_path).await? {
        return Err(anyhow!("skill `{name}` already exists"));
    }
    let title = name
        .split('-')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    let body = format!(
        "---\nname: {name}\ndescription: One-line summary of when this skill applies\ntriggers:\n  - {name}\n---\n\n# {title}\n\nUse this skill when the task requires {name}.\n\n## Instructions\n\n- Add the operating procedure for this skill here.\n"
    );
    fs::write(&skill_path, body.as_bytes())
        .await
        .with_context(|| format!("failed to write {}", skill_path.display()))?;
    let parsed = parse_skill_file(&body);
    Ok(Skill {
        name,
        path: skill_path,
        title: parsed.title,
        description: parsed.description,
        triggers: parsed.triggers,
        body: parsed.body,
    })
}

fn sanitize_skill_name(name: &str) -> Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("skill name cannot be empty"));
    }
    let normalized = trimmed
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    let normalized = normalized
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if normalized.is_empty() {
        return Err(anyhow!("skill name must contain a letter or number"));
    }
    Ok(normalized)
}

/// Execute one prompt against the configured agent process. Output is collected
/// into log files and the final [`RunRecord`] is persisted to disk. Callers
/// that want live token-by-line output should use [`run_agent_streaming`]
/// instead — this is a thin convenience wrapper that discards events.
pub async fn run_agent(
    config: &HarnessConfig,
    runs_dir: &Path,
    request: RunRequest,
) -> Result<RunRecord> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = run_agent_streaming(config, runs_dir, request, tx).await;
    drain.await.context("event drain task failed")?;
    result
}

/// Execute one prompt against the configured agent process, streaming stdout
/// and stderr lines to `events` as they arrive. The returned `RunRecord` is
/// also delivered through the channel as a final [`RunEvent::Completed`] so
/// receivers that only watch the channel can react to completion without
/// awaiting the task handle separately.
pub async fn run_agent_streaming(
    config: &HarnessConfig,
    runs_dir: &Path,
    request: RunRequest,
    events: UnboundedSender<RunEvent>,
) -> Result<RunRecord> {
    let profile = config
        .profiles
        .get(&request.profile)
        .ok_or_else(|| anyhow!("unknown profile `{}`", request.profile))?;
    if profile.command.is_empty() {
        return Err(anyhow!(
            "profile `{}` has an empty command",
            request.profile
        ));
    }

    let id = Uuid::new_v4().to_string();
    let started_at = Utc::now();
    let run_dir = runs_dir.join(format!("{}_{}", started_at.format("%Y%m%dT%H%M%SZ"), id));
    fs::create_dir_all(&run_dir)
        .await
        .with_context(|| format!("failed to create run dir {}", run_dir.display()))?;

    let cwd = request
        .cwd
        .clone()
        .or_else(|| profile.cwd.clone())
        .unwrap_or(std::env::current_dir().context("failed to read current directory")?);
    let timeout_secs = request.timeout_secs.or(profile.timeout_secs);
    let stdout_log = run_dir.join("stdout.log");
    let stderr_log = run_dir.join("stderr.log");
    let prompt_file = run_dir.join("prompt.txt");
    fs::write(&prompt_file, request.prompt.as_bytes())
        .await
        .with_context(|| format!("failed to write {}", prompt_file.display()))?;

    let prompt = resolved_prompt(profile, &request);
    let command_line = resolved_command(profile, &prompt, &request);
    let mut command = Command::new(&command_line[0]);
    command.args(&command_line[1..]);
    command.current_dir(&cwd);
    command.envs(&profile.env);
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    let mut child = command
        .spawn()
        .with_context(|| format!("failed to spawn `{}`", command_line.join(" ")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("failed to capture stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("failed to capture stderr"))?;

    let _ = events.send(RunEvent::Started(RunStarted {
        id: id.clone(),
        command: command_line.clone(),
        cwd: cwd.clone(),
        started_at,
    }));

    let stdout_task = tokio::spawn(stream_pipe(
        stdout,
        stdout_log.clone(),
        StreamKind::Stdout,
        events.clone(),
    ));
    let stderr_task = tokio::spawn(stream_pipe(
        stderr,
        stderr_log.clone(),
        StreamKind::Stderr,
        events.clone(),
    ));

    let started = std::time::Instant::now();
    let wait_result = match timeout_secs {
        Some(seconds) => {
            match tokio::time::timeout(Duration::from_secs(seconds), child.wait()).await {
                Ok(wait) => WaitOutcome::Exited(wait.context("failed while waiting for agent")?),
                Err(_) => {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    WaitOutcome::TimedOut
                }
            }
        }
        None => WaitOutcome::Exited(
            child
                .wait()
                .await
                .context("failed while waiting for agent")?,
        ),
    };
    let duration_ms = started.elapsed().as_millis();
    stdout_task.await.context("stdout task failed")??;
    stderr_task.await.context("stderr task failed")??;

    let (status, exit_code) = match wait_result {
        WaitOutcome::Exited(exit_status) if exit_status.success() => {
            (RunStatus::Succeeded, exit_status.code())
        }
        WaitOutcome::Exited(exit_status) => (RunStatus::Failed, exit_status.code()),
        WaitOutcome::TimedOut => (RunStatus::TimedOut, None),
    };
    let captured_session_id = match profile.session_id_capture_prefix.as_deref() {
        Some(prefix) if !prefix.is_empty() => extract_session_id(&stdout_log, prefix)
            .await
            .ok()
            .flatten(),
        _ => None,
    };
    let record = RunRecord {
        id,
        profile: request.profile,
        label: request.label,
        prompt: request.prompt,
        command: command_line,
        cwd,
        started_at,
        finished_at: Utc::now(),
        duration_ms,
        timeout_secs,
        status,
        exit_code,
        stdout_log,
        stderr_log,
        captured_session_id,
    };
    write_record(&run_dir, &record).await?;
    let _ = events.send(RunEvent::Completed(Box::new(record.clone())));
    Ok(record)
}

/// Scan a captured stdout log for the first line beginning with `prefix` and
/// return the trimmed remainder. Returns `Ok(None)` if no line matches.
async fn extract_session_id(log_path: &Path, prefix: &str) -> Result<Option<String>> {
    let body = fs::read_to_string(log_path).await?;
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix(prefix) {
            let trimmed = rest.trim();
            if !trimmed.is_empty() {
                return Ok(Some(trimmed.to_string()));
            }
        }
    }
    Ok(None)
}

pub async fn run_jobs(
    config: HarnessConfig,
    runs_dir: PathBuf,
    jobs: Vec<JobSpec>,
    concurrency: usize,
) -> Vec<Result<RunRecord>> {
    let config = Arc::new(config);
    let runs_dir = Arc::new(runs_dir);
    let semaphore = Arc::new(Semaphore::new(concurrency.max(1)));
    futures::stream::iter(jobs.into_iter().map(|job| {
        let config = Arc::clone(&config);
        let runs_dir = Arc::clone(&runs_dir);
        let semaphore = Arc::clone(&semaphore);
        async move {
            let permit = semaphore
                .acquire_owned()
                .await
                .context("job semaphore closed")?;
            let _permit = permit;
            let request = RunRequest {
                profile: job.profile.unwrap_or_else(|| DEFAULT_PROFILE.to_string()),
                prompt: job.prompt,
                label: job.label.or(job.id),
                cwd: job.cwd,
                timeout_secs: job.timeout_secs,
                bypass_permissions: job.bypass_permissions,
                desktop_control: job.desktop_control,
                prompt_prefix: None,
                provider_session_id: None,
            };
            run_agent(&config, &runs_dir, request).await
        }
    }))
    .buffer_unordered(concurrency.max(1))
    .collect()
    .await
}

pub async fn read_jobs(path: &Path) -> Result<Vec<JobSpec>> {
    let extension = path.extension().and_then(|ext| ext.to_str()).unwrap_or("");
    match extension {
        "json" => {
            let body = fs::read_to_string(path)
                .await
                .with_context(|| format!("failed to read jobs file {}", path.display()))?;
            serde_json::from_str::<Vec<JobSpec>>(&body)
                .with_context(|| format!("failed to parse jobs file {}", path.display()))
        }
        "csv" => {
            let body = fs::read_to_string(path)
                .await
                .with_context(|| format!("failed to read jobs file {}", path.display()))?;
            let mut reader = csv::Reader::from_reader(body.as_bytes());
            reader
                .deserialize()
                .collect::<std::result::Result<Vec<JobSpec>, csv::Error>>()
                .with_context(|| format!("failed to parse jobs file {}", path.display()))
        }
        _ => Err(anyhow!(
            "unsupported jobs file extension `{extension}`; use .json or .csv"
        )),
    }
}

pub async fn list_runs(runs_dir: &Path) -> Result<Vec<(PathBuf, RunRecord)>> {
    let mut entries = match fs::read_dir(runs_dir).await {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(err).with_context(|| format!("failed to read {}", runs_dir.display()));
        }
    };
    let mut runs = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if !entry.file_type().await?.is_dir() {
            continue;
        }
        let record_path = path.join("record.json");
        let Ok(record) = read_record(&record_path).await else {
            continue;
        };
        runs.push((path, record));
    }
    runs.sort_by_key(|(_, record)| record.started_at);
    runs.reverse();
    Ok(runs)
}

pub async fn read_transcript(record: &RunRecord) -> Result<String> {
    let stdout = read_optional_string(&record.stdout_log).await?;
    let stderr = read_optional_string(&record.stderr_log).await?;
    Ok(format!(
        "stdout:\n{}\n\nstderr:\n{}",
        stdout.trim_end(),
        stderr.trim_end()
    ))
}

pub async fn find_run(
    runs_dir: &Path,
    id_prefix: Option<&str>,
) -> Result<Option<(PathBuf, RunRecord)>> {
    let runs = list_runs(runs_dir).await?;
    Ok(match id_prefix {
        Some(prefix) => runs
            .into_iter()
            .find(|(_, record)| record.id.starts_with(prefix)),
        None => runs.into_iter().next(),
    })
}

impl HarnessConfig {
    fn with_default_profile(mut self) -> Self {
        if !self.profiles.contains_key(DEFAULT_PROFILE) {
            self.profiles
                .insert(DEFAULT_PROFILE.to_string(), default_profile());
        }
        self
    }
}

fn default_config() -> HarnessConfig {
    HarnessConfig {
        profiles: BTreeMap::from([(DEFAULT_PROFILE.to_string(), default_profile())]),
    }
}

fn default_profile() -> AgentProfile {
    AgentProfile {
        command: vec!["opencode".to_string(), "run".to_string()],
        bypass_args: vec!["--dangerously-skip-permissions".to_string()],
        desktop_args: vec!["--dangerously-skip-permissions".to_string()],
        desktop_prompt_prefix: Some(
            "You may use desktop-capable shell commands and GUI automation when needed. You may open, edit, create, move, and organize files in the current desktop session."
                .to_string(),
        ),
        env: BTreeMap::new(),
        cwd: None,
        timeout_secs: Some(300),
        prompt_arg: true,
        continue_args: Vec::new(),
        session_id_capture_prefix: None,
    }
}

fn default_prompt_arg() -> bool {
    true
}

fn resolved_prompt(profile: &AgentProfile, request: &RunRequest) -> String {
    let mut prompt = request.prompt.clone();
    if let Some(prefix) = &request.prompt_prefix {
        prompt = format!("{prefix}\n\n{prompt}");
    }
    if request.desktop_control {
        let prefix = profile
            .desktop_prompt_prefix
            .as_deref()
            .unwrap_or("You may use desktop control when needed.");
        return format!("{prefix}\n\n{prompt}");
    }
    prompt
}

struct ParsedSkill {
    title: Option<String>,
    description: Option<String>,
    triggers: Vec<String>,
    body: String,
}

/// Parse a SKILL.md file. The format is optional YAML-ish frontmatter (lines
/// between leading and trailing `---`) followed by the markdown body. Only
/// `name`, `description`, and `triggers` are recognized; the rest of the
/// frontmatter is ignored. The body keeps the markdown intact (including any
/// `# Title` line) so it can be injected verbatim into the prompt.
fn parse_skill_file(raw: &str) -> ParsedSkill {
    let (frontmatter, body) = split_frontmatter(raw);
    let title = first_markdown_heading(body);
    let mut description = None;
    let mut triggers = Vec::new();
    if let Some(text) = frontmatter {
        parse_frontmatter(text, &mut description, &mut triggers);
    }
    ParsedSkill {
        title,
        description,
        triggers,
        body: body.to_string(),
    }
}

fn split_frontmatter(raw: &str) -> (Option<&str>, &str) {
    let Some(rest) = raw.strip_prefix("---\n") else {
        return (None, raw);
    };
    let Some(end) = rest.find("\n---") else {
        return (None, raw);
    };
    let frontmatter = &rest[..end];
    let after = &rest[end + 4..];
    // Skip any blank lines between the closing fence and the body markdown.
    let body = after.trim_start_matches('\n');
    (Some(frontmatter), body)
}

fn first_markdown_heading(body: &str) -> Option<String> {
    body.lines()
        .find_map(|line| line.trim().strip_prefix("# ").map(str::trim))
        .filter(|title| !title.is_empty())
        .map(str::to_string)
}

fn parse_frontmatter(text: &str, description: &mut Option<String>, triggers: &mut Vec<String>) {
    let mut in_triggers = false;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("- ").and_then(|s| in_triggers.then_some(s)) {
            let value = strip_quotes(rest.trim());
            if !value.is_empty() {
                triggers.push(value.to_string());
            }
            continue;
        }
        if line.starts_with(char::is_whitespace) && in_triggers {
            // Still inside the triggers list with indented dash items.
            if let Some(rest) = line.trim_start().strip_prefix("- ") {
                let value = strip_quotes(rest.trim());
                if !value.is_empty() {
                    triggers.push(value.to_string());
                }
                continue;
            }
        }
        in_triggers = false;
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            "description" if !value.is_empty() => {
                *description = Some(strip_quotes(value).to_string());
            }
            "triggers" => {
                if value.is_empty() {
                    in_triggers = true;
                } else {
                    // Inline form: `triggers: [a, b]` or `triggers: a`.
                    if let Some(inner) = value
                        .strip_prefix('[')
                        .and_then(|v| v.strip_suffix(']'))
                    {
                        for part in inner.split(',') {
                            let part = strip_quotes(part.trim());
                            if !part.is_empty() {
                                triggers.push(part.to_string());
                            }
                        }
                    } else {
                        triggers.push(strip_quotes(value).to_string());
                    }
                }
            }
            _ => {}
        }
    }
}

fn strip_quotes(value: &str) -> &str {
    if (value.starts_with('"') && value.ends_with('"') && value.len() >= 2)
        || (value.starts_with('\'') && value.ends_with('\'') && value.len() >= 2)
    {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

fn resolved_command(profile: &AgentProfile, prompt: &str, request: &RunRequest) -> Vec<String> {
    let mut command = profile.command.clone();
    if request.bypass_permissions {
        command.extend(profile.bypass_args.clone());
    }
    if request.desktop_control {
        command.extend(profile.desktop_args.clone());
    }
    if let Some(session_id) = request.provider_session_id.as_deref() {
        for arg in &profile.continue_args {
            command.push(arg.replace("{session_id}", session_id));
        }
    }
    if profile.prompt_arg {
        command.push(prompt.to_string());
    }
    command
}

async fn stream_pipe<R>(
    reader: R,
    path: PathBuf,
    kind: StreamKind,
    events: UnboundedSender<RunEvent>,
) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut file = fs::File::create(&path)
        .await
        .with_context(|| format!("failed to create {}", path.display()))?;
    let mut reader = BufReader::new(reader);
    let mut buf = Vec::with_capacity(1024);
    loop {
        buf.clear();
        let bytes = reader
            .read_until(b'\n', &mut buf)
            .await
            .with_context(|| format!("failed to read {kind:?}"))?;
        if bytes == 0 {
            break;
        }
        file.write_all(&buf).await?;
        let text = strip_trailing_newline(&buf);
        let event = match kind {
            StreamKind::Stdout => RunEvent::Stdout(text),
            StreamKind::Stderr => RunEvent::Stderr(text),
        };
        if events.send(event).is_err() {
            // Receiver dropped; keep draining to disk so the run record is
            // complete, but no point allocating strings the caller will not
            // see.
            return drain_pipe(reader, &mut file).await;
        }
    }
    Ok(())
}

async fn drain_pipe<R>(mut reader: BufReader<R>, file: &mut fs::File) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buf = [0_u8; 8192];
    loop {
        let count = tokio::io::AsyncReadExt::read(&mut reader, &mut buf).await?;
        if count == 0 {
            break;
        }
        file.write_all(&buf[..count]).await?;
    }
    Ok(())
}

fn strip_trailing_newline(bytes: &[u8]) -> String {
    let mut end = bytes.len();
    while end > 0 && (bytes[end - 1] == b'\n' || bytes[end - 1] == b'\r') {
        end -= 1;
    }
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

async fn write_record(run_dir: &Path, record: &RunRecord) -> Result<()> {
    let record_path = run_dir.join("record.json");
    let body = serde_json::to_vec_pretty(record)?;
    fs::write(&record_path, body)
        .await
        .with_context(|| format!("failed to write {}", record_path.display()))
}

async fn read_record(path: &Path) -> Result<RunRecord> {
    let body = fs::read_to_string(path)
        .await
        .with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&body).with_context(|| format!("failed to parse {}", path.display()))
}

async fn read_optional_string(path: &Path) -> Result<String> {
    match fs::read_to_string(path).await {
        Ok(body) => Ok(body),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(err) => Err(err).with_context(|| format!("failed to read {}", path.display())),
    }
}

enum WaitOutcome {
    Exited(ExitStatus),
    TimedOut,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn runs_command_and_writes_record() -> Result<()> {
        let temp = TempDir::new()?;
        let config = HarnessConfig {
            profiles: BTreeMap::from([(
                "default".to_string(),
                AgentProfile {
                    command: vec![
                        "sh".to_string(),
                        "-c".to_string(),
                        "printf '%s' \"$1\"".to_string(),
                        "sh".to_string(),
                    ],
                    bypass_args: Vec::new(),
                    desktop_args: Vec::new(),
                    desktop_prompt_prefix: None,
                    env: BTreeMap::new(),
                    cwd: None,
                    timeout_secs: Some(5),
                    prompt_arg: true,
                    continue_args: Vec::new(),
                    session_id_capture_prefix: None,
                },
            )]),
        };
        let record = run_agent(
            &config,
            temp.path(),
            RunRequest {
                profile: "default".to_string(),
                prompt: "hello".to_string(),
                label: None,
                cwd: None,
                timeout_secs: None,
                bypass_permissions: false,
                desktop_control: false,
                prompt_prefix: None,
                provider_session_id: None,
            },
        )
        .await?;

        assert_eq!(record.status, RunStatus::Succeeded);
        assert_eq!(read_optional_string(&record.stdout_log).await?, "hello");
        assert_eq!(list_runs(temp.path()).await?.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn streams_stdout_and_stderr_events() -> Result<()> {
        let temp = TempDir::new()?;
        let config = HarnessConfig {
            profiles: BTreeMap::from([(
                "default".to_string(),
                AgentProfile {
                    command: vec![
                        "sh".to_string(),
                        "-c".to_string(),
                        "printf 'one\\ntwo\\n'; printf 'oops\\n' 1>&2; printf 'three\\n'"
                            .to_string(),
                    ],
                    bypass_args: Vec::new(),
                    desktop_args: Vec::new(),
                    desktop_prompt_prefix: None,
                    env: BTreeMap::new(),
                    cwd: None,
                    timeout_secs: Some(5),
                    prompt_arg: false,
                    continue_args: Vec::new(),
                    session_id_capture_prefix: None,
                },
            )]),
        };

        let (tx, mut rx) = mpsc::unbounded_channel();
        let record = run_agent_streaming(
            &config,
            temp.path(),
            RunRequest {
                profile: "default".to_string(),
                prompt: "ignored".to_string(),
                label: None,
                cwd: None,
                timeout_secs: None,
                bypass_permissions: false,
                desktop_control: false,
                prompt_prefix: None,
                provider_session_id: None,
            },
            tx,
        )
        .await?;

        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }

        assert_eq!(record.status, RunStatus::Succeeded);

        let started_count = events
            .iter()
            .filter(|event| matches!(event, RunEvent::Started(_)))
            .count();
        assert_eq!(started_count, 1, "expected exactly one Started event");

        let stdout_lines: Vec<&str> = events
            .iter()
            .filter_map(|event| match event {
                RunEvent::Stdout(line) => Some(line.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(stdout_lines, vec!["one", "two", "three"]);

        let stderr_lines: Vec<&str> = events
            .iter()
            .filter_map(|event| match event {
                RunEvent::Stderr(line) => Some(line.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(stderr_lines, vec!["oops"]);

        let completed_count = events
            .iter()
            .filter(|event| matches!(event, RunEvent::Completed(_)))
            .count();
        assert_eq!(completed_count, 1, "expected exactly one Completed event");

        // The disk log should still contain the same bytes — the streaming
        // tap must not skip writing.
        assert_eq!(read_optional_string(&record.stdout_log).await?, "one\ntwo\nthree\n");
        assert_eq!(read_optional_string(&record.stderr_log).await?, "oops\n");
        Ok(())
    }

    #[tokio::test]
    async fn session_round_trips_through_disk() -> Result<()> {
        let temp = TempDir::new()?;
        let mut session = Session::new("default".to_string(), temp.path().to_path_buf());
        session.bypass = true;
        session.record_user("hi".to_string());
        session.record_assistant("hello".to_string(), "run-123".to_string());
        save_session(temp.path(), &session).await?;

        let loaded = load_session(temp.path(), &session.id).await?;
        assert_eq!(loaded.id, session.id);
        assert!(loaded.bypass);
        assert_eq!(loaded.run_ids, vec!["run-123".to_string()]);
        match &loaded.transcript[0] {
            SessionTurn::User { text, .. } => assert_eq!(text, "hi"),
            other => panic!("unexpected first turn: {other:?}"),
        }
        match &loaded.transcript[1] {
            SessionTurn::Assistant { text, run_id, .. } => {
                assert_eq!(text, "hello");
                assert_eq!(run_id, "run-123");
            }
            other => panic!("unexpected second turn: {other:?}"),
        }
        Ok(())
    }

    #[tokio::test]
    async fn list_sessions_orders_by_recency() -> Result<()> {
        let temp = TempDir::new()?;
        let mut older = Session::new("default".to_string(), temp.path().to_path_buf());
        let mut newer = Session::new("default".to_string(), temp.path().to_path_buf());
        // Force older to have an earlier updated_at by reaching in. In practice
        // recency comes from chronological saves; here we set it explicitly so
        // the assertion is deterministic.
        older.updated_at -= chrono::Duration::seconds(60);
        newer.updated_at += chrono::Duration::seconds(60);
        save_session(temp.path(), &older).await?;
        save_session(temp.path(), &newer).await?;

        let sessions = list_sessions(temp.path()).await?;
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].id, newer.id);
        assert_eq!(sessions[1].id, older.id);
        Ok(())
    }

    #[tokio::test]
    async fn load_session_resolves_id_prefix() -> Result<()> {
        let temp = TempDir::new()?;
        let session = Session::new("default".to_string(), temp.path().to_path_buf());
        save_session(temp.path(), &session).await?;
        let loaded = load_session(temp.path(), &session.short_id()).await?;
        assert_eq!(loaded.id, session.id);
        Ok(())
    }

    #[tokio::test]
    async fn fork_creates_new_id_and_preserves_transcript() -> Result<()> {
        let temp = TempDir::new()?;
        let mut session = Session::new("default".to_string(), temp.path().to_path_buf());
        session.record_user("first".to_string());
        session.record_assistant("reply".to_string(), "run-1".to_string());
        let fork = session.fork();
        assert_ne!(fork.id, session.id);
        assert_eq!(fork.transcript.len(), 2);
        assert_eq!(fork.run_ids, session.run_ids);
        Ok(())
    }

    #[test]
    fn parses_skill_with_frontmatter() {
        let raw = "---\nname: weather\ndescription: Answer weather questions\ntriggers:\n  - \"weather\"\n  - forecast\n---\n\n# Weather\n\nUse the weather skill.\n";
        let parsed = parse_skill_file(raw);
        assert_eq!(parsed.description.as_deref(), Some("Answer weather questions"));
        assert_eq!(parsed.triggers, vec!["weather", "forecast"]);
        assert_eq!(parsed.title.as_deref(), Some("Weather"));
        assert!(parsed.body.starts_with("# Weather"));
    }

    #[test]
    fn parses_skill_without_frontmatter() {
        let raw = "# Plain\n\nNo metadata here.\n";
        let parsed = parse_skill_file(raw);
        assert!(parsed.description.is_none());
        assert!(parsed.triggers.is_empty());
        assert_eq!(parsed.title.as_deref(), Some("Plain"));
        assert!(parsed.body.starts_with("# Plain"));
    }

    #[test]
    fn parses_inline_triggers_array() {
        let raw = "---\ndescription: x\ntriggers: [a, \"b c\", d]\n---\nbody";
        let parsed = parse_skill_file(raw);
        assert_eq!(parsed.triggers, vec!["a", "b c", "d"]);
    }

    #[test]
    fn skill_matches_prompt_case_insensitive() {
        let skill = Skill {
            name: "weather".to_string(),
            path: PathBuf::new(),
            title: None,
            description: None,
            triggers: vec!["FoRecAst".to_string()],
            body: String::new(),
        };
        assert!(skill.matches_prompt("What's the forecast for Tokyo?"));
        assert!(!skill.matches_prompt("Tell me a joke."));
    }

    #[tokio::test]
    async fn appends_continue_args_when_session_id_set() -> Result<()> {
        let temp = TempDir::new()?;
        let config = HarnessConfig {
            profiles: BTreeMap::from([(
                "default".to_string(),
                AgentProfile {
                    command: vec![
                        "sh".to_string(),
                        "-c".to_string(),
                        // Print every arg one per line so the test can assert
                        // that `--session abc123` was appended.
                        "for arg in \"$@\"; do printf '%s\\n' \"$arg\"; done".to_string(),
                        "sh".to_string(),
                    ],
                    bypass_args: Vec::new(),
                    desktop_args: Vec::new(),
                    desktop_prompt_prefix: None,
                    env: BTreeMap::new(),
                    cwd: None,
                    timeout_secs: Some(5),
                    prompt_arg: true,
                    continue_args: vec!["--session".to_string(), "{session_id}".to_string()],
                    session_id_capture_prefix: None,
                },
            )]),
        };
        let record = run_agent(
            &config,
            temp.path(),
            RunRequest {
                profile: "default".to_string(),
                prompt: "hello".to_string(),
                label: None,
                cwd: None,
                timeout_secs: None,
                bypass_permissions: false,
                desktop_control: false,
                prompt_prefix: None,
                provider_session_id: Some("abc123".to_string()),
            },
        )
        .await?;
        let stdout = read_optional_string(&record.stdout_log).await?;
        assert!(stdout.contains("--session"), "stdout was: {stdout}");
        assert!(stdout.contains("abc123"), "stdout was: {stdout}");
        Ok(())
    }

    #[tokio::test]
    async fn captures_session_id_from_stdout() -> Result<()> {
        let temp = TempDir::new()?;
        let config = HarnessConfig {
            profiles: BTreeMap::from([(
                "default".to_string(),
                AgentProfile {
                    command: vec![
                        "sh".to_string(),
                        "-c".to_string(),
                        "printf 'session_id: xyz789\\nhello\\n'".to_string(),
                    ],
                    bypass_args: Vec::new(),
                    desktop_args: Vec::new(),
                    desktop_prompt_prefix: None,
                    env: BTreeMap::new(),
                    cwd: None,
                    timeout_secs: Some(5),
                    prompt_arg: false,
                    continue_args: Vec::new(),
                    session_id_capture_prefix: Some("session_id: ".to_string()),
                },
            )]),
        };
        let record = run_agent(
            &config,
            temp.path(),
            RunRequest {
                profile: "default".to_string(),
                prompt: "ignored".to_string(),
                label: None,
                cwd: None,
                timeout_secs: None,
                bypass_permissions: false,
                desktop_control: false,
                prompt_prefix: None,
                provider_session_id: None,
            },
        )
        .await?;
        assert_eq!(record.captured_session_id.as_deref(), Some("xyz789"));
        Ok(())
    }

    #[tokio::test]
    async fn marks_timeout() -> Result<()> {
        let temp = TempDir::new()?;
        let config = HarnessConfig {
            profiles: BTreeMap::from([(
                "default".to_string(),
                AgentProfile {
                    command: vec!["sh".to_string(), "-c".to_string(), "sleep 2".to_string()],
                    bypass_args: Vec::new(),
                    desktop_args: Vec::new(),
                    desktop_prompt_prefix: None,
                    env: BTreeMap::new(),
                    cwd: None,
                    timeout_secs: Some(1),
                    prompt_arg: false,
                    continue_args: Vec::new(),
                    session_id_capture_prefix: None,
                },
            )]),
        };
        let record = run_agent(
            &config,
            temp.path(),
            RunRequest {
                profile: "default".to_string(),
                prompt: "ignored".to_string(),
                label: None,
                cwd: None,
                timeout_secs: None,
                bypass_permissions: false,
                desktop_control: false,
                prompt_prefix: None,
                provider_session_id: None,
            },
        )
        .await?;

        assert_eq!(record.status, RunStatus::TimedOut);
        Ok(())
    }
}
