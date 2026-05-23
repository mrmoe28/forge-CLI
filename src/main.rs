use anyhow::Context;
use anyhow::Result;
use clap::Args;
use clap::Parser;
use clap::Subcommand;
use forge_cli::HarnessConfig;
use forge_cli::RunEvent;
use forge_cli::RunRecord;
use forge_cli::RunRequest;
use forge_cli::RunStatus;
use forge_cli::Skill;
use forge_cli::default_runs_dir;
use forge_cli::discover_skills;
use forge_cli::find_run;
use forge_cli::list_runs;
use forge_cli::load_config;
use forge_cli::read_jobs;
use forge_cli::read_transcript;
use forge_cli::run_agent_streaming;
use forge_cli::run_jobs;
use std::io::IsTerminal;
use std::io::Write;
use std::path::PathBuf;
use tokio::sync::mpsc;

mod commands;
mod composer;
mod terminal_ui;

#[derive(Debug, Parser)]
#[command(name = "forge")]
#[command(about = "Interactive CLI and job harness for external coding agents")]
struct Cli {
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[arg(long, global = true)]
    runs_dir: Option<PathBuf>,

    #[arg(long, global = true)]
    sessions_dir: Option<PathBuf>,

    #[clap(flatten)]
    interactive: InteractiveArgs,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Start the interactive CLI.
    Interactive(InteractiveArgs),

    /// Run one prompt through a configured agent profile.
    Run(RunArgs),

    /// Run a quick profile smoke test.
    Smoke(SmokeArgs),

    /// Run many jobs from a JSON or CSV file.
    Jobs(JobsArgs),

    /// List recent runs.
    List(ListArgs),

    /// Show stdout/stderr for a run.
    Transcript(TranscriptArgs),

    /// Retry a previous run using the same prompt/profile/cwd/timeout.
    Retry(RetryArgs),
}

#[derive(Debug, Args, Clone)]
struct InteractiveArgs {
    /// Use the plain line-oriented REPL instead of the full-screen terminal UI.
    #[arg(long)]
    plain: bool,

    #[arg(long, default_value = "default")]
    profile: String,

    #[arg(long)]
    cwd: Option<PathBuf>,

    #[arg(long)]
    timeout_secs: Option<u64>,

    /// Pass profile bypass arguments, e.g. opencode --dangerously-skip-permissions.
    #[arg(long)]
    bypass: bool,

    /// Enable desktop-control prompting and profile desktop arguments.
    #[arg(long)]
    desktop: bool,
}

#[derive(Debug, Parser)]
struct RunArgs {
    prompt: String,

    #[arg(long, default_value = "default")]
    profile: String,

    #[arg(long)]
    label: Option<String>,

    #[arg(long)]
    cwd: Option<PathBuf>,

    #[arg(long)]
    timeout_secs: Option<u64>,

    #[arg(long)]
    bypass: bool,

    #[arg(long)]
    desktop: bool,
}

#[derive(Debug, Parser)]
struct SmokeArgs {
    #[arg(long, default_value = "default")]
    profile: String,

    #[arg(long, default_value = "Reply exactly: ok")]
    prompt: String,

    #[arg(long, default_value_t = 60)]
    timeout_secs: u64,
}

#[derive(Debug, Parser)]
struct JobsArgs {
    file: PathBuf,

    #[arg(long, default_value_t = 2)]
    concurrency: usize,
}

#[derive(Debug, Parser)]
struct ListArgs {
    #[arg(long, default_value_t = 20)]
    limit: usize,
}

#[derive(Debug, Parser)]
struct TranscriptArgs {
    /// Run id prefix. Defaults to the most recent run.
    id: Option<String>,
}

#[derive(Debug, Parser)]
struct RetryArgs {
    /// Run id prefix. Defaults to the most recent failed or timed-out run.
    id: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let runs_dir = match cli.runs_dir {
        Some(path) => path,
        None => default_runs_dir()?,
    };
    let sessions_dir = match cli.sessions_dir {
        Some(path) => path,
        None => forge_cli::default_sessions_dir()?,
    };
    let config = load_config(cli.config.as_deref()).await?;

    match cli.command {
        None => interactive_loop(config, runs_dir, sessions_dir, cli.interactive).await,
        Some(Command::Interactive(args)) => {
            interactive_loop(config, runs_dir, sessions_dir, args).await
        }
        Some(Command::Run(args)) => {
            let record = run_with_live_output(&config, &runs_dir, args.into_request()).await?;
            print_run(&record);
            status_to_result(record.status)
        }
        Some(Command::Smoke(args)) => {
            let record = run_with_live_output(
                &config,
                &runs_dir,
                RunRequest {
                    profile: args.profile,
                    prompt: args.prompt,
                    label: Some("smoke".to_string()),
                    cwd: None,
                    timeout_secs: Some(args.timeout_secs),
                    bypass_permissions: false,
                    desktop_control: false,
                    prompt_prefix: None,
                    provider_session_id: None,
                },
            )
            .await?;
            print_run(&record);
            status_to_result(record.status)
        }
        Some(Command::Jobs(args)) => {
            let jobs = read_jobs(&args.file).await?;
            let results = run_jobs(config, runs_dir, jobs, args.concurrency).await;
            let mut failed = 0;
            for result in results {
                match result {
                    Ok(record) => {
                        if record.status != RunStatus::Succeeded {
                            failed += 1;
                        }
                        print_run(&record);
                    }
                    Err(err) => {
                        failed += 1;
                        eprintln!("job failed before record creation: {err:#}");
                    }
                }
            }
            if failed == 0 {
                Ok(())
            } else {
                anyhow::bail!("{failed} job(s) failed")
            }
        }
        Some(Command::List(args)) => {
            let runs = list_runs(&runs_dir).await?;
            for (_, record) in runs.into_iter().take(args.limit) {
                print_run(&record);
            }
            Ok(())
        }
        Some(Command::Transcript(args)) => {
            let Some((_, record)) = find_run(&runs_dir, args.id.as_deref()).await? else {
                anyhow::bail!("no matching run found");
            };
            println!("{}", read_transcript(&record).await?);
            Ok(())
        }
        Some(Command::Retry(args)) => {
            let (_, record) = find_retry_source(&runs_dir, args.id.as_deref()).await?;
            let retry = run_with_live_output(
                &config,
                &runs_dir,
                RunRequest {
                    profile: record.profile,
                    prompt: record.prompt,
                    label: record.label.map(|label| format!("retry:{label}")),
                    cwd: Some(record.cwd),
                    timeout_secs: record.timeout_secs,
                    bypass_permissions: false,
                    desktop_control: false,
                    prompt_prefix: None,
                    provider_session_id: None,
                },
            )
            .await
            .context("retry failed")?;
            print_run(&retry);
            status_to_result(retry.status)
        }
    }
}

impl Default for InteractiveArgs {
    fn default() -> Self {
        Self {
            plain: false,
            profile: "default".to_string(),
            cwd: None,
            timeout_secs: None,
            bypass: false,
            desktop: false,
        }
    }
}

impl RunArgs {
    fn into_request(self) -> RunRequest {
        RunRequest {
            profile: self.profile,
            prompt: self.prompt,
            label: self.label,
            cwd: self.cwd,
            timeout_secs: self.timeout_secs,
            bypass_permissions: self.bypass,
            desktop_control: self.desktop,
            prompt_prefix: None,
            provider_session_id: None,
        }
    }
}

async fn interactive_loop(
    config: forge_cli::HarnessConfig,
    runs_dir: PathBuf,
    sessions_dir: PathBuf,
    args: InteractiveArgs,
) -> Result<()> {
    if !args.plain && std::io::stdout().is_terminal() {
        return terminal_ui::run_terminal_ui(config, runs_dir, sessions_dir, args).await;
    }

    let session_cwd = args
        .cwd
        .clone()
        .unwrap_or(std::env::current_dir().context("failed to read current directory")?);
    let mut session = forge_cli::Session::new(args.profile.clone(), session_cwd);
    session.bypass = args.bypass;
    session.desktop = args.desktop;
    session.timeout_secs = args.timeout_secs;
    forge_cli::save_session(&sessions_dir, &session).await?;
    let skills = discover_skills(&session.cwd).await?;
    let mut plain = PlainState {
        session,
        sessions_dir,
        skills,
        last_run_id: None,
    };

    println!("external agent harness");
    println!(
        "session {} (profile: {})",
        plain.session.short_id(),
        plain.session.profile
    );
    println!(
        "commands: /help, /profile <name>, /skills, /skill <name>, /bypass on|off, /desktop on|off, /runs, /last, /retry [id], /new, /sessions, /resume [id], /fork [id], /exit"
    );

    loop {
        print!("forge> ");
        std::io::stdout()
            .flush()
            .context("failed to flush prompt")?;

        let mut input = String::new();
        let bytes = std::io::stdin()
            .read_line(&mut input)
            .context("failed to read input")?;
        if bytes == 0 {
            println!();
            return Ok(());
        }
        let input = input.trim();
        if input.is_empty() {
            continue;
        }

        match commands::classify_input(input) {
            commands::InputClass::Command(command) => {
                if handle_interactive_command(&config, &runs_dir, &mut plain, command).await? {
                    return Ok(());
                }
                continue;
            }
            commands::InputClass::UnknownSlash(token) => {
                let hint = commands::fuzzy_search(token)
                    .first()
                    .map(|cmd| cmd.name)
                    .unwrap_or("help");
                println!("unknown command: /{token}. Try /{hint} (or /help for the full list)");
                continue;
            }
            // Path-looking or plain-prompt input falls through to the agent.
            commands::InputClass::Path | commands::InputClass::Prompt => {}
        }

        plain.session.record_user(input.to_string());
        let record = run_with_live_output(
            &config,
            &runs_dir,
            RunRequest {
                profile: plain.session.profile.clone(),
                prompt: input.to_string(),
                label: None,
                cwd: Some(plain.session.cwd.clone()),
                timeout_secs: plain.session.timeout_secs,
                bypass_permissions: plain.session.bypass,
                desktop_control: plain.session.desktop,
                prompt_prefix: active_skill_prompt(
                    &plain.skills,
                    &plain.session.active_skills,
                    input,
                ),
                provider_session_id: plain.session.provider_session_id.clone(),
            },
        )
        .await?;
        plain.last_run_id = Some(record.id.clone());
        if let Some(id) = record.captured_session_id.clone()
            && plain.session.provider_session_id.as_ref() != Some(&id)
        {
            plain.session.provider_session_id = Some(id.clone());
            println!("captured provider session id: {id}");
        }
        plain
            .session
            .record_assistant(read_transcript_text(&record).await?, record.id.clone());
        forge_cli::save_session(&plain.sessions_dir, &plain.session).await?;
        print_run(&record);
    }
}

struct PlainState {
    session: forge_cli::Session,
    sessions_dir: PathBuf,
    skills: Vec<Skill>,
    last_run_id: Option<String>,
}

/// Read just the stdout portion of a completed run so it can be stored on the
/// session transcript as the assistant turn. The on-disk format from
/// [`read_transcript`] also includes stderr, which we drop.
async fn read_transcript_text(record: &forge_cli::RunRecord) -> Result<String> {
    let body = read_transcript(record).await?;
    let stdout = body
        .strip_prefix("stdout:\n")
        .and_then(|rest| {
            rest.split_once("\n\nstderr:\n")
                .map(|(a, _)| a)
                .or(Some(rest))
        })
        .unwrap_or(body.as_str());
    Ok(stdout.trim_end_matches('\n').to_string())
}

async fn handle_interactive_command(
    config: &forge_cli::HarnessConfig,
    runs_dir: &std::path::Path,
    plain: &mut PlainState,
    command: &str,
) -> Result<bool> {
    let mut parts = command.split_whitespace();
    match parts.next().unwrap_or_default() {
        "exit" | "quit" | "q" => Ok(true),
        "help" | "h" => {
            println!("Enter a prompt to run it with the current profile.");
            let mut last_category: Option<commands::Category> = None;
            for cmd in commands::COMMANDS {
                if last_category != Some(cmd.category) {
                    println!("── {} ──", cmd.category.label());
                    last_category = Some(cmd.category);
                }
                println!("{:<32}  {}", cmd.usage, cmd.summary);
            }
            Ok(false)
        }
        "profile" => {
            let Some(profile) = parts.next() else {
                println!("profile: {}", plain.session.profile);
                return Ok(false);
            };
            if !config.profiles.contains_key(profile) {
                anyhow::bail!("unknown profile `{profile}`");
            }
            plain.session.profile = profile.to_string();
            forge_cli::save_session(&plain.sessions_dir, &plain.session).await?;
            println!("profile: {}", plain.session.profile);
            Ok(false)
        }
        "profiles" => {
            for name in config.profiles.keys() {
                println!("{name}");
            }
            Ok(false)
        }
        "skills" => {
            if plain.skills.is_empty() {
                println!("no skills found");
                return Ok(false);
            }
            for skill in &plain.skills {
                let active = if plain.session.active_skills.contains(&skill.name) {
                    "*"
                } else {
                    " "
                };
                let triggers = if skill.triggers.is_empty() {
                    String::new()
                } else {
                    format!("  [triggers: {}]", skill.triggers.join(", "))
                };
                println!("{active} {} — {}{triggers}", skill.name, skill.summary());
            }
            Ok(false)
        }
        "skill" => {
            let Some(name) = parts.next() else {
                if plain.session.active_skills.is_empty() {
                    println!("active skills: none");
                } else {
                    println!("active skills: {}", plain.session.active_skills.join(", "));
                }
                return Ok(false);
            };
            if matches!(name, "off" | "clear" | "none") {
                plain.session.active_skills.clear();
                forge_cli::save_session(&plain.sessions_dir, &plain.session).await?;
                println!("skills cleared");
                return Ok(false);
            }
            if !plain.skills.iter().any(|skill| skill.name == name) {
                println!("unknown skill: {name}");
                return Ok(false);
            }
            if !plain
                .session
                .active_skills
                .iter()
                .any(|skill| skill == name)
            {
                plain.session.active_skills.push(name.to_string());
            }
            forge_cli::save_session(&plain.sessions_dir, &plain.session).await?;
            println!("active skills: {}", plain.session.active_skills.join(", "));
            Ok(false)
        }
        "bypass" => {
            plain.session.bypass = parse_toggle(parts.next(), plain.session.bypass);
            forge_cli::save_session(&plain.sessions_dir, &plain.session).await?;
            println!("bypass: {}", on_off(plain.session.bypass));
            Ok(false)
        }
        "desktop" => {
            plain.session.desktop = parse_toggle(parts.next(), plain.session.desktop);
            if plain.session.desktop {
                plain.session.bypass = true;
            }
            forge_cli::save_session(&plain.sessions_dir, &plain.session).await?;
            println!(
                "desktop: {}, bypass: {}",
                on_off(plain.session.desktop),
                on_off(plain.session.bypass)
            );
            Ok(false)
        }
        "runs" => {
            for (_, record) in list_runs(runs_dir).await?.into_iter().take(10) {
                print_run(&record);
            }
            Ok(false)
        }
        "last" => {
            let id = plain.last_run_id.as_deref();
            let Some((_, record)) = find_run(runs_dir, id).await? else {
                println!("no runs yet");
                return Ok(false);
            };
            print_transcript(&record).await?;
            Ok(false)
        }
        "retry" => {
            let id = parts.next().or(plain.last_run_id.as_deref());
            let Some((_, record)) = find_run(runs_dir, id).await? else {
                println!("no matching run found");
                return Ok(false);
            };
            plain.session.record_user(record.prompt.clone());
            let prompt_prefix =
                active_skill_prompt(&plain.skills, &plain.session.active_skills, &record.prompt);
            let retry = run_with_live_output(
                config,
                runs_dir,
                RunRequest {
                    profile: record.profile,
                    prompt: record.prompt,
                    label: record.label.map(|label| format!("retry:{label}")),
                    cwd: Some(record.cwd),
                    timeout_secs: record.timeout_secs,
                    bypass_permissions: plain.session.bypass,
                    desktop_control: plain.session.desktop,
                    prompt_prefix,
                    provider_session_id: plain.session.provider_session_id.clone(),
                },
            )
            .await?;
            plain.last_run_id = Some(retry.id.clone());
            if let Some(id) = retry.captured_session_id.clone()
                && plain.session.provider_session_id.as_ref() != Some(&id)
            {
                plain.session.provider_session_id = Some(id.clone());
                println!("captured provider session id: {id}");
            }
            plain
                .session
                .record_assistant(read_transcript_text(&retry).await?, retry.id.clone());
            forge_cli::save_session(&plain.sessions_dir, &plain.session).await?;
            print_run(&retry);
            Ok(false)
        }
        "new" => {
            forge_cli::save_session(&plain.sessions_dir, &plain.session).await?;
            let mut next =
                forge_cli::Session::new(plain.session.profile.clone(), plain.session.cwd.clone());
            next.bypass = plain.session.bypass;
            next.desktop = plain.session.desktop;
            next.timeout_secs = plain.session.timeout_secs;
            forge_cli::save_session(&plain.sessions_dir, &next).await?;
            plain.session = next;
            plain.last_run_id = None;
            plain.skills = discover_skills(&plain.session.cwd).await?;
            println!("new session: {}", plain.session.short_id());
            Ok(false)
        }
        "sessions" => {
            let sessions = forge_cli::list_sessions(&plain.sessions_dir).await?;
            if sessions.is_empty() {
                println!("no sessions yet");
                return Ok(false);
            }
            for session in sessions.into_iter().take(20) {
                let active = if session.id == plain.session.id {
                    "*"
                } else {
                    " "
                };
                let name = session
                    .name
                    .clone()
                    .unwrap_or_else(|| "(unnamed)".to_string());
                println!(
                    "{active} {} profile={} turns={} updated={} {}",
                    session.short_id(),
                    session.profile,
                    session.turn_count(),
                    session.updated_at.format("%Y-%m-%d %H:%M"),
                    name
                );
            }
            Ok(false)
        }
        "resume" => {
            let next = match parts.next() {
                Some(id) => forge_cli::load_session(&plain.sessions_dir, id).await?,
                None => {
                    let mut sessions = forge_cli::list_sessions(&plain.sessions_dir).await?;
                    sessions.retain(|session| session.id != plain.session.id);
                    match sessions.into_iter().next() {
                        Some(session) => session,
                        None => {
                            println!("no other sessions to resume");
                            return Ok(false);
                        }
                    }
                }
            };
            forge_cli::save_session(&plain.sessions_dir, &plain.session).await?;
            plain.session = next;
            plain.last_run_id = plain.session.run_ids.last().cloned();
            plain.skills = discover_skills(&plain.session.cwd).await?;
            println!(
                "resumed {} ({} turns)",
                plain.session.short_id(),
                plain.session.turn_count()
            );
            Ok(false)
        }
        "fork" => {
            let source = match parts.next() {
                Some(id) => forge_cli::load_session(&plain.sessions_dir, id).await?,
                None => plain.session.clone(),
            };
            let fork = source.fork();
            forge_cli::save_session(&plain.sessions_dir, &plain.session).await?;
            forge_cli::save_session(&plain.sessions_dir, &fork).await?;
            plain.session = fork;
            plain.last_run_id = plain.session.run_ids.last().cloned();
            plain.skills = discover_skills(&plain.session.cwd).await?;
            println!("forked {}", plain.session.short_id());
            Ok(false)
        }
        "status" => {
            let skills = if plain.session.active_skills.is_empty() {
                "none".to_string()
            } else {
                plain.session.active_skills.join(", ")
            };
            let mode = match (plain.session.bypass, plain.session.desktop) {
                (true, true) => "desktop+bypass",
                (true, false) => "bypass",
                (false, true) => "desktop",
                (false, false) => "guarded",
            };
            println!(
                "session: {} ({} turns)",
                plain.session.short_id(),
                plain.session.turn_count()
            );
            println!("profile: {}", plain.session.profile);
            println!("cwd: {}", plain.session.cwd.display());
            println!("mode: {mode}");
            println!("active skills: {skills}");
            println!(
                "last run: {}",
                plain.last_run_id.as_deref().unwrap_or("(none)")
            );
            Ok(false)
        }
        "model" => {
            let Some(profile) = config.profiles.get(&plain.session.profile) else {
                println!("profile `{}` is no longer defined", plain.session.profile);
                return Ok(false);
            };
            println!("profile: {}", plain.session.profile);
            println!("command: {}", profile.command.join(" "));
            Ok(false)
        }
        "permissions" => {
            match parts.next() {
                None => {
                    let mode = match (plain.session.bypass, plain.session.desktop) {
                        (true, true) => "desktop+bypass",
                        (true, false) => "bypass",
                        (false, true) => "desktop",
                        (false, false) => "guarded",
                    };
                    println!("permissions: {mode}");
                }
                Some("guarded") => {
                    plain.session.bypass = false;
                    plain.session.desktop = false;
                    forge_cli::save_session(&plain.sessions_dir, &plain.session).await?;
                    println!("permissions: guarded");
                }
                Some("bypass") => {
                    plain.session.bypass = true;
                    plain.session.desktop = false;
                    forge_cli::save_session(&plain.sessions_dir, &plain.session).await?;
                    println!("permissions: bypass");
                }
                Some("desktop") => {
                    plain.session.bypass = true;
                    plain.session.desktop = true;
                    forge_cli::save_session(&plain.sessions_dir, &plain.session).await?;
                    println!("permissions: desktop");
                }
                Some(other) => {
                    println!("unknown permission mode `{other}`");
                }
            }
            Ok(false)
        }
        "compact" => {
            let keep = parts
                .next()
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(20);
            let before = plain.session.transcript.len();
            if before > keep {
                let dropped = before - keep;
                plain.session.transcript.drain(..dropped);
                forge_cli::save_session(&plain.sessions_dir, &plain.session).await?;
                println!("compacted: dropped {dropped} turn(s)");
            } else {
                println!("nothing to compact ({before} turns)");
            }
            Ok(false)
        }
        "provider" => {
            match parts.next() {
                None | Some("show") => match &plain.session.provider_session_id {
                    Some(id) => println!("provider session id: {id}"),
                    None => println!("provider session id: (none)"),
                },
                Some("clear") => {
                    plain.session.provider_session_id = None;
                    forge_cli::save_session(&plain.sessions_dir, &plain.session).await?;
                    println!("provider session id cleared");
                }
                Some("set") => {
                    let Some(id) = parts.next() else {
                        println!("Usage: /provider set <id>");
                        return Ok(false);
                    };
                    plain.session.provider_session_id = Some(id.to_string());
                    forge_cli::save_session(&plain.sessions_dir, &plain.session).await?;
                    println!("provider session id: {id}");
                }
                Some(other) => println!("Unknown /provider subcommand `{other}`"),
            }
            Ok(false)
        }
        "clear" => {
            // ANSI clear + cursor home. Works in any VT100-ish terminal.
            print!("\x1b[2J\x1b[H");
            std::io::Write::flush(&mut std::io::stdout()).ok();
            Ok(false)
        }
        cmd @ ("smoke" | "inspect" | "open-run" | "logs" | "export" | "jobs") => {
            println!(
                "/{cmd} is only available in the interactive TUI. Run `forge` without `--plain` to use it."
            );
            Ok(false)
        }
        unknown => {
            // Defensive: dispatch only sees `classify_input -> Command(_)`, so
            // this arm should be unreachable. Keep it as a clear error.
            println!("unknown command: /{unknown}");
            Ok(false)
        }
    }
}

fn parse_toggle(value: Option<&str>, current: bool) -> bool {
    match value {
        Some("on" | "true" | "yes" | "1") => true,
        Some("off" | "false" | "no" | "0") => false,
        _ => !current,
    }
}

fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

fn active_skill_prompt(
    skills: &[Skill],
    active_skills: &[String],
    user_prompt: &str,
) -> Option<String> {
    let mut chosen: Vec<&Skill> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for name in active_skills {
        if let Some(skill) = skills.iter().find(|s| &s.name == name)
            && seen.insert(skill.name.as_str())
        {
            chosen.push(skill);
        }
    }
    for skill in skills {
        if skill.matches_prompt(user_prompt) && seen.insert(skill.name.as_str()) {
            chosen.push(skill);
        }
    }
    if chosen.is_empty() {
        return None;
    }
    let mut prompt = String::from("Use the following active skills for this request:\n");
    for skill in chosen {
        prompt.push_str("\n--- skill: ");
        prompt.push_str(&skill.name);
        prompt.push_str(" ---\n");
        prompt.push_str(&skill.body);
        prompt.push('\n');
    }
    Some(prompt)
}

async fn find_retry_source(
    runs_dir: &std::path::Path,
    id: Option<&str>,
) -> Result<(PathBuf, forge_cli::RunRecord)> {
    if let Some(id) = id {
        return find_run(runs_dir, Some(id))
            .await?
            .with_context(|| format!("no run matching `{id}`"));
    }
    let runs = list_runs(runs_dir).await?;
    runs.into_iter()
        .find(|(_, record)| record.status != RunStatus::Succeeded)
        .context("no failed or timed-out run found")
}

fn print_run(record: &forge_cli::RunRecord) {
    println!(
        "{} {:?} profile={} label={} duration={}ms",
        record.id,
        record.status,
        record.profile,
        record.label.as_deref().unwrap_or("-"),
        record.duration_ms
    );
}

async fn print_transcript(record: &forge_cli::RunRecord) -> Result<()> {
    let transcript = read_transcript(record).await?;
    println!("{transcript}");
    Ok(())
}

fn status_to_result(status: RunStatus) -> Result<()> {
    match status {
        RunStatus::Succeeded => Ok(()),
        RunStatus::Failed => anyhow::bail!("agent run failed"),
        RunStatus::TimedOut => anyhow::bail!("agent run timed out"),
    }
}

/// Run one prompt to completion, streaming stdout/stderr lines to the
/// process's stdout/stderr as they arrive. Returns the final [`RunRecord`].
async fn run_with_live_output(
    config: &HarnessConfig,
    runs_dir: &std::path::Path,
    request: RunRequest,
) -> Result<RunRecord> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let config = config.clone();
    let runs_dir = runs_dir.to_path_buf();
    let handle =
        tokio::spawn(async move { run_agent_streaming(&config, &runs_dir, request, tx).await });

    while let Some(event) = rx.recv().await {
        match event {
            RunEvent::Started(_) => {}
            RunEvent::Stdout(line) => println!("{line}"),
            RunEvent::Stderr(line) => eprintln!("{line}"),
            RunEvent::Completed(_) => {}
        }
    }

    handle.await.context("agent task panicked")?
}
