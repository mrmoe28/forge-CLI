use crate::InteractiveArgs;
use crate::commands;
use crate::commands::InputClass;
use crate::commands::SlashCommand;
use crate::commands::classify_input;
use crate::composer::Composer;
use anyhow::Context;
use anyhow::Result;
use crossterm::event;
use crossterm::event::Event;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use crossterm::execute;
use crossterm::terminal;
use forge_cli::HarnessConfig;
use forge_cli::RunEvent;
use forge_cli::RunRecord;
use forge_cli::RunRequest;
use forge_cli::RunStatus;
use forge_cli::Session;
use forge_cli::SessionTurn;
use forge_cli::Skill;
use forge_cli::create_skill;
use forge_cli::discover_skills;
use forge_cli::find_run;
use forge_cli::list_runs;
use forge_cli::list_sessions;
use forge_cli::load_session;
use forge_cli::read_transcript;
use forge_cli::run_agent_streaming;
use forge_cli::save_session;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Constraint;
use ratatui::layout::Direction;
use ratatui::layout::Layout;
use ratatui::layout::Margin;
use ratatui::layout::Position;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Block;
use ratatui::widgets::Borders;
use ratatui::widgets::Clear;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Wrap;
use std::io;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::task::JoinHandle;

pub async fn run_terminal_ui(
    config: HarnessConfig,
    runs_dir: PathBuf,
    sessions_dir: PathBuf,
    args: InteractiveArgs,
) -> Result<()> {
    let mut terminal = enter_terminal()?;
    let _guard = TerminalGuard;
    let session_cwd = args
        .cwd
        .clone()
        .unwrap_or(std::env::current_dir().context("failed to read current directory")?);
    let mut session = Session::new(args.profile.clone(), session_cwd);
    session.bypass = args.bypass;
    session.desktop = args.desktop;
    session.timeout_secs = args.timeout_secs;
    save_session(&sessions_dir, &session).await?;
    let skills = discover_skills(&session.cwd).await?;
    let mut state = TuiState::new(config, session, sessions_dir, runs_dir, skills);

    loop {
        terminal.draw(|frame| render(frame, &state))?;

        pump_active_run(&mut state).await;

        let poll_ms = if state.active_run.is_some() { 33 } else { 100 };
        if event::poll(Duration::from_millis(poll_ms))? {
            match event::read()? {
                Event::Key(key) if handle_key(&mut state, key) => break,
                Event::Key(_) => {}
                Event::Paste(text) => {
                    state.composer.insert_str(&text);
                    state.selected_suggestion = 0;
                }
                _ => {}
            }
        }

        if let Some(action) = state.pending_action.take() {
            match action {
                Action::Submit(prompt) => {
                    if state.active_run.is_some() {
                        state.status = "Run already in progress".to_string();
                    } else {
                        start_run(&mut state, prompt);
                    }
                }
                Action::Command(command) => {
                    if handle_command(&mut state, &command).await? {
                        break;
                    }
                }
            }
        }
    }

    Ok(())
}

struct TuiState {
    config: HarnessConfig,
    session: Session,
    sessions_dir: PathBuf,
    runs_dir: PathBuf,
    skills: Vec<Skill>,
    composer: Composer,
    status: String,
    transcript: Vec<TranscriptEntry>,
    last_run_id: Option<String>,
    scroll: u16,
    selected_suggestion: usize,
    pending_action: Option<Action>,
    pending_approval: Option<PendingApproval>,
    active_run: Option<ActiveRun>,
}

/// A guarded-mode pre-submission approval card. The user must approve the
/// prompt before it is forwarded to the agent. We can't inspect tool calls
/// inside an opaque subprocess, so the harness-level approval is the act of
/// running the agent at all.
struct PendingApproval {
    prompt: String,
    profile: String,
    command: Vec<String>,
    cwd: PathBuf,
}

impl TuiState {
    fn new(
        config: HarnessConfig,
        session: Session,
        sessions_dir: PathBuf,
        runs_dir: PathBuf,
        skills: Vec<Skill>,
    ) -> Self {
        let transcript = vec![
            TranscriptEntry::system(format!("Forge — session {}", session.short_id())),
            TranscriptEntry::system(
                "Type a prompt, or /help for commands. Alt+Enter inserts a newline.",
            ),
        ];
        let mut composer = Composer::new();
        composer.load_history_from_turns(&session.transcript);
        Self {
            config,
            session,
            sessions_dir,
            runs_dir,
            skills,
            composer,
            status: "ready".to_string(),
            transcript,
            last_run_id: None,
            scroll: 0,
            selected_suggestion: 0,
            pending_action: None,
            pending_approval: None,
            active_run: None,
        }
    }

    fn push_entries(&mut self, lines: impl IntoIterator<Item = TranscriptEntry>) {
        self.transcript.extend(lines);
        self.scroll = u16::MAX;
    }

    fn push_system(&mut self, line: impl Into<String>) {
        self.push_entries([TranscriptEntry::system(line)]);
    }

    /// Refresh derived UI state after `self.session` has been swapped (via
    /// `/new`, `/resume`, or `/fork`). Rebuilds the visible transcript and
    /// reloads the composer history so up-arrow walks back through the new
    /// session's prompts.
    fn sync_after_session_swap(&mut self) {
        self.rebuild_visual_transcript();
        self.composer
            .load_history_from_turns(&self.session.transcript);
    }

    /// Replace the visible transcript with one derived from `session.transcript`.
    /// Used after `/resume` and `/fork` to bring the screen in sync with the
    /// loaded session.
    fn rebuild_visual_transcript(&mut self) {
        let mut entries = vec![TranscriptEntry::system(format!(
            "Session {} ({} turn{})",
            self.session.short_id(),
            self.session.turn_count(),
            if self.session.turn_count() == 1 {
                ""
            } else {
                "s"
            }
        ))];
        for turn in &self.session.transcript {
            match turn {
                SessionTurn::User { text, .. } => {
                    for line in text.lines() {
                        entries.push(TranscriptEntry::user(line));
                    }
                }
                SessionTurn::Assistant { text, .. } => {
                    for (index, line) in text.lines().enumerate() {
                        if line.is_empty() {
                            continue;
                        }
                        if index == 0 {
                            entries.push(TranscriptEntry::assistant(line));
                        } else {
                            entries.push(TranscriptEntry::assistant_cont(line));
                        }
                    }
                }
                SessionTurn::System { text, .. } => {
                    entries.push(TranscriptEntry::system(text.as_str()));
                }
            }
        }
        self.transcript = entries;
        self.scroll = u16::MAX;
    }
}

/// Tracks one running agent invocation. The TUI loop drains `rx` between
/// frames so streamed lines render incrementally, and awaits `handle` once
/// `handle.is_finished()` to surface the final [`RunRecord`] or task error.
struct ActiveRun {
    handle: JoinHandle<Result<RunRecord>>,
    rx: UnboundedReceiver<RunEvent>,
    assistant_open: bool,
    error_open: bool,
    /// Concatenated stdout lines, separated by `\n`, captured so the final
    /// assistant turn can be persisted to the session transcript.
    assistant_buffer: String,
}

enum Action {
    Submit(String),
    Command(String),
}

#[derive(Clone)]
struct TranscriptEntry {
    kind: TranscriptKind,
    text: String,
}

impl TranscriptEntry {
    fn user(text: impl Into<String>) -> Self {
        Self {
            kind: TranscriptKind::User,
            text: text.into(),
        }
    }

    fn assistant(text: impl Into<String>) -> Self {
        Self {
            kind: TranscriptKind::Assistant,
            text: text.into(),
        }
    }

    fn assistant_cont(text: impl Into<String>) -> Self {
        Self {
            kind: TranscriptKind::AssistantCont,
            text: text.into(),
        }
    }

    fn system(text: impl Into<String>) -> Self {
        Self {
            kind: TranscriptKind::System,
            text: text.into(),
        }
    }

    fn error(text: impl Into<String>) -> Self {
        Self {
            kind: TranscriptKind::Error,
            text: text.into(),
        }
    }

    fn error_cont(text: impl Into<String>) -> Self {
        Self {
            kind: TranscriptKind::ErrorCont,
            text: text.into(),
        }
    }
}

#[derive(Clone, Copy)]
enum TranscriptKind {
    User,
    Assistant,
    AssistantCont,
    System,
    Error,
    ErrorCont,
}

fn render(frame: &mut ratatui::Frame<'_>, state: &TuiState) {
    let suggestions = suggestions(state);
    let suggestions_height = suggestions_height(&suggestions);
    let selected_command = selected_command_for(&suggestions, state.selected_suggestion);
    let details_height = if suggestions_height > 0 && selected_command.is_some() {
        2
    } else {
        0
    };
    let input_inner_lines = state.composer.line_count().clamp(1, 8) as u16;
    let input_height = input_inner_lines + 2;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(suggestions_height),
            Constraint::Length(details_height),
            Constraint::Length(input_height),
            Constraint::Length(1),
        ])
        .split(frame.area());

    let header = Paragraph::new(Line::from(vec![
        Span::styled(
            "forge",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("   "),
        Span::styled(
            format!("profile: {}", state.session.profile),
            Style::default().fg(Color::Yellow),
        ),
        Span::raw("   "),
        Span::styled(
            mode_label(state),
            Style::default().fg(if state.session.bypass || state.session.desktop {
                Color::Red
            } else {
                Color::DarkGray
            }),
        ),
        Span::raw("   "),
        Span::styled(
            skills_label(state),
            Style::default().fg(if state.session.active_skills.is_empty() {
                Color::DarkGray
            } else {
                Color::Magenta
            }),
        ),
    ]));
    frame.render_widget(header, chunks[0]);

    let body = state
        .transcript
        .iter()
        .map(render_entry)
        .collect::<Vec<_>>();
    let transcript_area = chunks[1].inner(Margin {
        vertical: 1,
        horizontal: 1,
    });
    let transcript_scroll = transcript_scroll(state, transcript_area.height, body.len());
    let transcript = Paragraph::new(body)
        .wrap(Wrap { trim: false })
        .scroll((transcript_scroll, 0));
    frame.render_widget(transcript, transcript_area);

    if suggestions_height > 0 {
        render_suggestions(frame, chunks[2], &suggestions, state.selected_suggestion);
    }

    if details_height > 0
        && let Some(cmd) = selected_command
    {
        render_command_details(frame, chunks[3], cmd);
    }

    let (cursor_row, cursor_col) = state.composer.cursor_row_col();
    let visible_rows = input_inner_lines as usize;
    let input_scroll = cursor_row.saturating_sub(visible_rows.saturating_sub(1));
    let mut composer_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(" message ");
    if let Some(chip) = composer_chip(state.composer.text()) {
        composer_block = composer_block
            .title_top(Line::from(Span::styled(chip.label(), chip.style())).right_aligned());
    }
    let input = Paragraph::new(state.composer.text())
        .style(Style::default().fg(Color::White))
        .scroll((input_scroll as u16, 0))
        .block(composer_block);
    frame.render_widget(input, chunks[4]);
    set_input_cursor(
        frame,
        chunks[4],
        cursor_col,
        cursor_row.saturating_sub(input_scroll),
    );

    render_status_bar(frame, chunks[5], state);

    if let Some(approval) = state.pending_approval.as_ref() {
        render_approval_card(frame, approval);
    }
}

fn render_approval_card(frame: &mut ratatui::Frame<'_>, approval: &PendingApproval) {
    let area = centered_rect(80, 70, frame.area());
    frame.render_widget(Clear, area);
    let mut lines: Vec<Line<'_>> = vec![
        Line::from(Span::styled(
            "Approve agent run?",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "profile",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(Span::raw(approval.profile.as_str())),
        Line::from(""),
        Line::from(Span::styled(
            "command",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(Span::raw(approval.command.join(" "))),
        Line::from(""),
        Line::from(Span::styled("cwd", Style::default().fg(Color::DarkGray))),
        Line::from(Span::raw(approval.cwd.display().to_string())),
        Line::from(""),
        Line::from(Span::styled("prompt", Style::default().fg(Color::DarkGray))),
    ];
    for prompt_line in approval.prompt.lines().take(10) {
        lines.push(Line::from(Span::raw(prompt_line)));
    }
    if approval.prompt.lines().count() > 10 {
        lines.push(Line::from(Span::styled(
            "…",
            Style::default().fg(Color::DarkGray),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(
            " y ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" approve once    "),
        Span::styled(
            " a ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" approve & bypass for session    "),
        Span::styled(
            " n ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Red)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" deny (edit)"),
    ]));

    let card = Paragraph::new(lines).wrap(Wrap { trim: false }).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
            .title(" guarded mode "),
    );
    frame.render_widget(card, area);
}

fn session_to_markdown(session: &Session) -> String {
    let mut out = String::new();
    out.push_str(&format!("# Session {}\n\n", session.id));
    if let Some(name) = &session.name {
        out.push_str(&format!("**name:** {name}\n"));
    }
    out.push_str(&format!("**profile:** {}\n", session.profile));
    out.push_str(&format!("**cwd:** {}\n", session.cwd.display()));
    out.push_str(&format!(
        "**bypass:** {}, **desktop:** {}\n",
        session.bypass, session.desktop
    ));
    out.push_str(&format!(
        "**created:** {}\n",
        session.created_at.to_rfc3339()
    ));
    out.push_str(&format!(
        "**updated:** {}\n\n",
        session.updated_at.to_rfc3339()
    ));
    for turn in &session.transcript {
        match turn {
            SessionTurn::User { text, at } => {
                out.push_str(&format!("## User · {}\n\n", at.to_rfc3339()));
                out.push_str(text);
                out.push_str("\n\n");
            }
            SessionTurn::Assistant { text, run_id, at } => {
                out.push_str(&format!(
                    "## Assistant · {} · run {}\n\n",
                    at.to_rfc3339(),
                    run_id
                ));
                out.push_str(text);
                out.push_str("\n\n");
            }
            SessionTurn::System { text, at } => {
                out.push_str(&format!("## System · {}\n\n", at.to_rfc3339()));
                out.push_str(text);
                out.push_str("\n\n");
            }
        }
    }
    out
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

fn render_command_details(frame: &mut ratatui::Frame<'_>, area: Rect, cmd: &'static SlashCommand) {
    let lines = vec![
        Line::from(vec![
            Span::styled(
                cmd.usage,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("   "),
            Span::styled(
                format!("[{}]", cmd.category.label()),
                Style::default().fg(Color::Magenta),
            ),
        ]),
        Line::from(Span::styled(cmd.help, Style::default().fg(Color::Gray))),
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn render_suggestions(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    suggestions: &[Suggestion],
    selected: usize,
) {
    let lines = suggestions
        .iter()
        .take(area.height as usize)
        .enumerate()
        .map(|(index, suggestion)| {
            let selected_style = if index == selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            Line::from(vec![
                Span::styled(format!(" {:<18}", suggestion.label), selected_style),
                Span::styled(
                    suggestion.description.as_str(),
                    if index == selected {
                        Style::default().fg(Color::Black).bg(Color::Cyan)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    },
                ),
            ])
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_entry(entry: &TranscriptEntry) -> Line<'_> {
    match entry.kind {
        TranscriptKind::User => Line::from(vec![
            Span::styled(
                "▌ ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(entry.text.as_str()),
        ]),
        TranscriptKind::Assistant => Line::from(vec![
            Span::styled(
                "▌ ",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(entry.text.as_str()),
        ]),
        TranscriptKind::AssistantCont => {
            Line::from(vec![Span::raw("  "), Span::raw(entry.text.as_str())])
        }
        TranscriptKind::System => Line::from(Span::styled(
            entry.text.as_str(),
            Style::default().fg(Color::DarkGray),
        )),
        TranscriptKind::Error => Line::from(vec![
            Span::styled(
                "▌ ",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::raw(entry.text.as_str()),
        ]),
        TranscriptKind::ErrorCont => {
            Line::from(vec![Span::raw("  "), Span::raw(entry.text.as_str())])
        }
    }
}

fn transcript_scroll(state: &TuiState, height: u16, body_len: usize) -> u16 {
    let max_scroll = body_len.saturating_sub(height as usize) as u16;
    state.scroll.min(max_scroll)
}

fn status_style(status: &str) -> Style {
    if status.contains("Running") {
        Style::default().fg(Color::Yellow)
    } else if status.contains("Failed") || status.contains("TimedOut") || status.contains("Unknown")
    {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(Color::Green)
    }
}

fn mode_label(state: &TuiState) -> &'static str {
    match (state.session.bypass, state.session.desktop) {
        (true, true) => "desktop+bypass",
        (true, false) => "bypass",
        (false, true) => "desktop",
        (false, false) => "guarded",
    }
}

fn skills_label(state: &TuiState) -> String {
    if state.session.active_skills.is_empty() {
        return format!("skills: {}", state.skills.len());
    }
    format!("skills: {}", state.session.active_skills.join(","))
}

fn render_status_bar(frame: &mut ratatui::Frame<'_>, area: Rect, state: &TuiState) {
    let home = dirs_home();
    let data = build_status_bar_data(
        &state.session.short_id(),
        &state.session.cwd,
        home.as_deref(),
        mode_label(state),
        state.active_run.is_some(),
        &state.status,
    );

    let dim = Style::default().fg(Color::DarkGray);
    let sep = || Span::styled("  ·  ", dim);
    let run_style = if state.active_run.is_some() {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        dim
    };
    let mode_style = if state.session.bypass || state.session.desktop {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let mut left = vec![
        Span::raw(" "),
        Span::styled(data.run_state, run_style),
        sep(),
        Span::styled(data.session.clone(), Style::default().fg(Color::Cyan)),
        sep(),
        Span::styled(data.cwd.clone(), Style::default().fg(Color::White)),
        sep(),
        Span::styled(data.mode, mode_style),
    ];

    // Right-side content: ephemeral message if any, otherwise a brief hint.
    let right_text = match &data.message {
        Some(msg) => msg.clone(),
        None => "enter send  ·  /help".to_string(),
    };
    let right_style = match &data.message {
        Some(msg) => status_style(msg),
        None => dim,
    };

    // Left chars used so we can right-align the message manually within `area`.
    let left_chars: usize = left.iter().map(|span| span.content.chars().count()).sum();
    let total = area.width as usize;
    let right_chars = right_text.chars().count();
    let padding = total
        .saturating_sub(left_chars)
        .saturating_sub(right_chars)
        .saturating_sub(1);
    if padding > 0 {
        left.push(Span::raw(" ".repeat(padding)));
    } else {
        // Not enough room; drop a separator instead of overflowing.
        left.push(Span::raw(" "));
    }
    left.push(Span::styled(right_text, right_style));

    let bar = Paragraph::new(Line::from(left));
    frame.render_widget(bar, area);
}

/// Look up the user's home directory once per frame. Returns `None` when the
/// environment doesn't expose `HOME` (e.g. in some CI sandboxes).
fn dirs_home() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(std::path::PathBuf::from)
}

/// Right-aligned chip on the composer border that tells the user how Enter
/// will treat the current text. Returns `None` when the input is empty, a
/// plain prompt, or a known slash command — i.e. cases where the default look
/// already communicates intent. Keeping the selector pure makes it testable
/// without ratatui.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ComposerChip {
    /// Input looks like a filesystem path; will be sent to the agent as
    /// prompt text rather than dispatched as a command.
    Path,
    /// Input starts with `/` but is neither a known command nor a path;
    /// Enter will block and surface a "did you mean" hint.
    UnknownSlash,
}

impl ComposerChip {
    fn label(self) -> &'static str {
        match self {
            ComposerChip::Path => " path ",
            ComposerChip::UnknownSlash => " unknown ",
        }
    }

    fn style(self) -> Style {
        match self {
            ComposerChip::Path => Style::default()
                .fg(Color::Black)
                .bg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
            ComposerChip::UnknownSlash => Style::default()
                .fg(Color::Black)
                .bg(Color::Red)
                .add_modifier(Modifier::BOLD),
        }
    }
}

fn composer_chip(input: &str) -> Option<ComposerChip> {
    if input.trim().is_empty() {
        return None;
    }
    match commands::classify_input(input) {
        commands::InputClass::Path => Some(ComposerChip::Path),
        commands::InputClass::UnknownSlash(_) => Some(ComposerChip::UnknownSlash),
        commands::InputClass::Command(_) | commands::InputClass::Prompt => None,
    }
}

/// Pure data backing the persistent bottom status bar. Kept separate from
/// rendering so the layout can be unit-tested without standing up a terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StatusBarData {
    run_state: &'static str,
    session: String,
    cwd: String,
    mode: &'static str,
    /// Ephemeral message (e.g. "Profile switched to dev"). When `None`, the
    /// renderer falls back to a brief key hint.
    message: Option<String>,
}

fn build_status_bar_data(
    session_short_id: &str,
    cwd: &std::path::Path,
    home: Option<&std::path::Path>,
    mode: &'static str,
    running: bool,
    ephemeral: &str,
) -> StatusBarData {
    let run_state = if running { "● running" } else { "○ idle" };
    let cwd_display = abbreviate_home(cwd, home);
    let message = if ephemeral.trim().is_empty() {
        None
    } else {
        Some(ephemeral.to_string())
    };
    StatusBarData {
        run_state,
        session: format!("session {session_short_id}"),
        cwd: format!("cwd {cwd_display}"),
        mode,
        message,
    }
}

/// Replace the user's home prefix with `~` so the cwd fits on a single line.
/// Returns the original path when `home` is `None` or the prefix doesn't match.
fn abbreviate_home(cwd: &std::path::Path, home: Option<&std::path::Path>) -> String {
    let cwd_str = cwd.display().to_string();
    let Some(home) = home else {
        return cwd_str;
    };
    let home_str = home.display().to_string();
    if home_str.is_empty() {
        return cwd_str;
    }
    if cwd == home {
        return "~".to_string();
    }
    if let Some(rest) = cwd_str.strip_prefix(&home_str)
        && rest.starts_with(std::path::MAIN_SEPARATOR)
    {
        return format!("~{rest}");
    }
    cwd_str
}

fn set_input_cursor(frame: &mut ratatui::Frame<'_>, area: Rect, col: usize, row: usize) {
    let max_x = area.width.saturating_sub(2) as usize;
    let max_y = area.height.saturating_sub(2) as usize;
    let cursor_x = area.x + 1 + col.min(max_x) as u16;
    let cursor_y = area.y + 1 + row.min(max_y) as u16;
    frame.set_cursor_position(Position::new(cursor_x, cursor_y));
}

fn handle_key(state: &mut TuiState, key: KeyEvent) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    if state.pending_approval.is_some() {
        return handle_approval_key(state, key);
    }

    match key.code {
        KeyCode::Char('c') if ctrl => true,
        KeyCode::Char('d') if ctrl && state.composer.is_empty() => true,
        KeyCode::Esc => {
            // Esc cancels an in-flight run (keeping the TUI open). When no run
            // is active, Esc exits — matching the previous behaviour.
            if cancel_active_run(
                &mut state.active_run,
                &mut state.transcript,
                &mut state.status,
            ) {
                state.scroll = u16::MAX;
                return false;
            }
            true
        }

        // Submission and newline insertion. Alt+Enter / Shift+Enter inserts a
        // literal newline; Enter submits.
        KeyCode::Enter if alt || shift => {
            state.composer.newline();
            state.selected_suggestion = 0;
            false
        }
        KeyCode::Enter => {
            let suggestions = suggestions(state);
            if !suggestions.is_empty()
                && let Some(action) = suggestions
                    .get(
                        state
                            .selected_suggestion
                            .min(suggestions.len().saturating_sub(1)),
                    )
                    .map(|suggestion| suggestion.action.clone())
            {
                apply_suggestion(state, action);
                return false;
            }
            let trimmed = state.composer.text().trim().to_string();
            if trimmed.is_empty() {
                state.composer.clear();
                return false;
            }
            match classify_input(&trimmed) {
                InputClass::UnknownSlash(token) => {
                    // Don't submit; show a "did-you-mean" hint and keep the
                    // composer text so the user can correct it.
                    let token = token.to_string();
                    let hint = commands::fuzzy_search(&token)
                        .first()
                        .map(|cmd| cmd.name)
                        .unwrap_or("help");
                    state.status = format!(
                        "Unknown command /{token}. Try /{hint} (or /help for the full list)"
                    );
                    false
                }
                InputClass::Command(_) => {
                    let Some(submitted) = state.composer.submit() else {
                        return false;
                    };
                    let submitted_trim = submitted.trim().to_string();
                    // Re-classify the submitted form to extract the body
                    // (handles trailing whitespace differences cleanly).
                    if let InputClass::Command(rest) = classify_input(&submitted_trim) {
                        state.pending_action = Some(Action::Command(rest.to_string()));
                    } else {
                        state.pending_action = Some(Action::Submit(submitted_trim));
                    }
                    false
                }
                InputClass::Path | InputClass::Prompt => {
                    if state.active_run.is_some() {
                        state.status = "Run in progress; wait or press Esc to exit".to_string();
                        return false;
                    }
                    // Guarded mode: show an approval card for prompt
                    // submissions instead of running the agent immediately.
                    if !state.session.bypass && !state.session.desktop {
                        state.pending_approval = build_pending_approval(state, trimmed);
                        return false;
                    }
                    let Some(submitted) = state.composer.submit() else {
                        return false;
                    };
                    state.pending_action = Some(Action::Submit(submitted.trim().to_string()));
                    false
                }
            }
        }

        // Word-level edits and movement.
        KeyCode::Char('w') if ctrl => {
            state.composer.delete_word_back();
            state.selected_suggestion = 0;
            false
        }
        KeyCode::Char('u') if ctrl => {
            state.composer.delete_to_line_start();
            state.selected_suggestion = 0;
            false
        }
        KeyCode::Char('k') if ctrl => {
            state.composer.delete_to_line_end();
            state.selected_suggestion = 0;
            false
        }
        KeyCode::Char('a') if ctrl => {
            state.composer.move_home();
            false
        }
        KeyCode::Char('e') if ctrl => {
            state.composer.move_end();
            false
        }
        KeyCode::Char('b') if ctrl => {
            state.composer.move_left();
            false
        }
        KeyCode::Char('f') if ctrl => {
            state.composer.move_right();
            false
        }
        KeyCode::Backspace if alt => {
            state.composer.delete_word_back();
            state.selected_suggestion = 0;
            false
        }

        KeyCode::Char(ch) => {
            state.composer.insert_char(ch);
            state.selected_suggestion = 0;
            false
        }
        KeyCode::Backspace => {
            state.composer.backspace();
            state.selected_suggestion = 0;
            false
        }
        KeyCode::Delete => {
            state.composer.delete_forward();
            state.selected_suggestion = 0;
            false
        }
        KeyCode::Tab => {
            let suggestions = suggestions(state);
            if !suggestions.is_empty() {
                state.selected_suggestion = (state.selected_suggestion + 1) % suggestions.len();
            }
            false
        }
        KeyCode::Left if ctrl => {
            state.composer.move_word_left();
            false
        }
        KeyCode::Right if ctrl => {
            state.composer.move_word_right();
            false
        }
        KeyCode::Left => {
            state.composer.move_left();
            false
        }
        KeyCode::Right => {
            state.composer.move_right();
            false
        }
        KeyCode::Home => {
            state.composer.move_home();
            false
        }
        KeyCode::End => {
            state.composer.move_end();
            false
        }
        KeyCode::PageUp => {
            state.scroll = state.scroll.saturating_sub(8);
            false
        }
        KeyCode::PageDown => {
            state.scroll = state.scroll.saturating_add(8);
            false
        }
        KeyCode::Up => {
            let suggestions = suggestions(state);
            if !suggestions.is_empty() {
                state.selected_suggestion = state.selected_suggestion.saturating_sub(1);
                return false;
            }
            // Multiline-aware: try moving up a line in the composer first.
            if state.composer.move_up() {
                return false;
            }
            // Otherwise fall back to history when the composer is single-line.
            if state.composer.history_prev() {
                return false;
            }
            state.scroll = state.scroll.saturating_sub(1);
            false
        }
        KeyCode::Down => {
            let suggestions = suggestions(state);
            if !suggestions.is_empty() {
                state.selected_suggestion =
                    (state.selected_suggestion + 1).min(suggestions.len().saturating_sub(1));
                return false;
            }
            if state.composer.move_down() {
                return false;
            }
            if state.composer.history_next() {
                return false;
            }
            state.scroll = state.scroll.saturating_add(1);
            false
        }
        _ => false,
    }
}

#[derive(Clone)]
struct Suggestion {
    label: String,
    description: String,
    action: SuggestionAction,
}

#[derive(Clone)]
enum SuggestionAction {
    Command(&'static str),
    Skill(String),
    SkillCreatePrefix,
}

fn suggestions(state: &TuiState) -> Vec<Suggestion> {
    let input = state.composer.text().trim_start();
    let Some(query) = input.strip_prefix('/') else {
        return Vec::new();
    };
    if query.contains('\n') {
        // No completions once the prompt has gone multi-line; the user is
        // composing a body, not picking a command.
        return Vec::new();
    }
    if query.contains('/') {
        return Vec::new();
    }
    // Treat path-like input (e.g. existing `/tmp`) as prompt text so Enter
    // never auto-applies a fuzzy command match.
    if matches!(classify_input(input.trim_end()), InputClass::Path) {
        return Vec::new();
    }
    if let Some(skill_query) = query.strip_prefix("skill ") {
        return skill_suggestions(state, skill_query.trim());
    }
    command_suggestions(query.trim())
}

fn command_suggestions(query: &str) -> Vec<Suggestion> {
    commands::fuzzy_search(query)
        .into_iter()
        .map(|cmd| Suggestion {
            label: format!("/{}", cmd.name),
            description: format!("[{}] {}", cmd.category.label(), cmd.summary),
            action: SuggestionAction::Command(cmd.name),
        })
        .collect()
}

fn selected_command_for(
    suggestions: &[Suggestion],
    selected: usize,
) -> Option<&'static SlashCommand> {
    let suggestion = suggestions.get(selected.min(suggestions.len().saturating_sub(1)))?;
    if let SuggestionAction::Command(name) = &suggestion.action {
        commands::lookup(name)
    } else {
        None
    }
}

fn skill_suggestions(state: &TuiState, query: &str) -> Vec<Suggestion> {
    let mut suggestions = Vec::new();
    suggestions.push(Suggestion {
        label: "/skill clear".to_string(),
        description: "clear active skills".to_string(),
        action: SuggestionAction::Command("skill clear"),
    });
    suggestions.push(Suggestion {
        label: "/skill create".to_string(),
        description: "create a new local skill".to_string(),
        action: SuggestionAction::SkillCreatePrefix,
    });
    suggestions.extend(
        state
            .skills
            .iter()
            .filter(|skill| query.is_empty() || skill.name.starts_with(query))
            .take(8)
            .map(|skill| Suggestion {
                label: format!("/skill {}", skill.name),
                description: skill.summary().to_string(),
                action: SuggestionAction::Skill(skill.name.clone()),
            }),
    );
    suggestions
}

fn suggestions_height(suggestions: &[Suggestion]) -> u16 {
    if suggestions.is_empty() {
        0
    } else {
        suggestions.len().min(8) as u16
    }
}

fn apply_suggestion(state: &mut TuiState, action: SuggestionAction) {
    match action {
        SuggestionAction::Command(command) => {
            if command == "skill" {
                state.composer.replace("/skill ".to_string());
            } else if command == "profile" {
                state.composer.replace("/profile ".to_string());
            } else if command == "bypass" {
                state.composer.replace("/bypass ".to_string());
            } else if command == "desktop" {
                state.composer.replace("/desktop ".to_string());
            } else {
                state.pending_action = Some(Action::Command(command.to_string()));
                state.composer.clear();
            }
        }
        SuggestionAction::Skill(skill) => {
            state.pending_action = Some(Action::Command(format!("skill {skill}")));
            state.composer.clear();
        }
        SuggestionAction::SkillCreatePrefix => {
            state.composer.replace("/skill create ".to_string());
        }
    }
    state.selected_suggestion = 0;
}

fn build_pending_approval(state: &TuiState, prompt: String) -> Option<PendingApproval> {
    let profile = state.config.profiles.get(&state.session.profile)?;
    Some(PendingApproval {
        prompt,
        profile: state.session.profile.clone(),
        command: profile.command.clone(),
        cwd: state.session.cwd.clone(),
    })
}

fn handle_approval_key(state: &mut TuiState, key: KeyEvent) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    // Hard-exit shortcuts still work, in case the card is stuck.
    if let KeyCode::Char('c') = key.code
        && ctrl
    {
        return true;
    }
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
            approve_pending(state, false);
            false
        }
        KeyCode::Char('a') | KeyCode::Char('A') => {
            approve_pending(state, true);
            false
        }
        KeyCode::Char('n')
        | KeyCode::Char('N')
        | KeyCode::Char('e')
        | KeyCode::Char('E')
        | KeyCode::Esc => {
            state.pending_approval = None;
            state.status = "Run denied; edit and resubmit".to_string();
            false
        }
        // Allow scrolling the transcript while the card is up.
        KeyCode::Up => {
            state.scroll = state.scroll.saturating_sub(1);
            false
        }
        KeyCode::Down => {
            state.scroll = state.scroll.saturating_add(1);
            false
        }
        KeyCode::PageUp => {
            state.scroll = state.scroll.saturating_sub(8);
            false
        }
        KeyCode::PageDown => {
            state.scroll = state.scroll.saturating_add(8);
            false
        }
        _ => false,
    }
}

fn approve_pending(state: &mut TuiState, persist_bypass: bool) {
    let Some(approval) = state.pending_approval.take() else {
        return;
    };
    if persist_bypass {
        state.session.bypass = true;
    }
    // Consume composer text (it matches the approval prompt) so the entry
    // lands in history.
    let _ = state.composer.submit();
    state.pending_action = Some(Action::Submit(approval.prompt));
}

/// Abort the in-flight run, if any, and surface a `Run cancelled` line. Returns
/// `true` when a run was actually cancelled; the Esc handler uses that to
/// decide whether to suppress the default "exit TUI" behavior.
fn cancel_active_run(
    active_run: &mut Option<ActiveRun>,
    transcript: &mut Vec<TranscriptEntry>,
    status: &mut String,
) -> bool {
    let Some(active) = active_run.take() else {
        return false;
    };
    // Abort drops the future, which drops the kill_on_drop child handle and
    // reaps the agent process. We don't await the JoinHandle here — the TUI
    // loop returns immediately so the user sees the cancel land.
    active.handle.abort();
    let suffix = if active.assistant_open || active.error_open {
        " (partial output preserved above)"
    } else {
        ""
    };
    transcript.push(TranscriptEntry::system(format!("Run cancelled{suffix}")));
    *status = "Run cancelled".to_string();
    true
}

fn start_run(state: &mut TuiState, prompt: String) {
    state.push_entries([TranscriptEntry::user(prompt.clone())]);
    state.status = "Running…".to_string();
    state.session.record_user(prompt.clone());

    let (tx, rx) = mpsc::unbounded_channel();
    let prompt_prefix = active_skill_prompt_for(state, &prompt);
    let request = RunRequest {
        profile: state.session.profile.clone(),
        prompt,
        label: None,
        cwd: Some(state.session.cwd.clone()),
        timeout_secs: state.session.timeout_secs,
        bypass_permissions: state.session.bypass,
        desktop_control: state.session.desktop,
        prompt_prefix,
        provider_session_id: state.session.provider_session_id.clone(),
    };
    let config = state.config.clone();
    let runs_dir = state.runs_dir.clone();
    let handle =
        tokio::spawn(async move { run_agent_streaming(&config, &runs_dir, request, tx).await });

    state.active_run = Some(ActiveRun {
        handle,
        rx,
        assistant_open: false,
        error_open: false,
        assistant_buffer: String::new(),
    });
}

async fn pump_active_run(state: &mut TuiState) {
    if state.active_run.is_none() {
        return;
    }

    let mut pending = Vec::new();
    let mut finished = false;
    if let Some(active) = state.active_run.as_mut() {
        while let Ok(event) = active.rx.try_recv() {
            pending.push(event);
        }
        if active.handle.is_finished() {
            finished = true;
        }
    }
    for event in pending {
        apply_run_event(state, event);
    }

    if finished && let Some(active) = state.active_run.take() {
        match active.handle.await {
            Ok(Ok(record)) => {
                state.last_run_id = Some(record.id.clone());
                state.status = format!("{:?} in {}ms", record.status, record.duration_ms);
                if !active.assistant_open && !active.error_open {
                    state.push_entries([TranscriptEntry::system(format!(
                        "(no output) exit={:?}",
                        record.exit_code
                    ))]);
                }
                if record.status != RunStatus::Succeeded && !active.error_open {
                    state.push_entries([TranscriptEntry::error(format!(
                        "exit code: {:?}",
                        record.exit_code
                    ))]);
                }
                if let Some(provider_id) = record.captured_session_id.clone()
                    && state.session.provider_session_id.as_ref() != Some(&provider_id)
                {
                    state.session.provider_session_id = Some(provider_id.clone());
                    state.push_system(format!("captured provider session id: {provider_id}"));
                }
                state
                    .session
                    .record_assistant(active.assistant_buffer, record.id.clone());
                if let Err(err) = save_session(&state.sessions_dir, &state.session).await {
                    state.push_entries([TranscriptEntry::error(format!(
                        "failed to save session: {err}"
                    ))]);
                }
            }
            Ok(Err(err)) => {
                state.status = format!("Failed: {err}");
                state.push_entries([TranscriptEntry::error(format!("{err:#}"))]);
            }
            Err(join_err) => {
                state.status = format!("Run task panicked: {join_err}");
            }
        }
    }
}

fn apply_run_event(state: &mut TuiState, event: RunEvent) {
    match event {
        RunEvent::Started(started) => {
            state.status = format!(
                "Running ({})",
                started.id.chars().take(8).collect::<String>()
            );
        }
        RunEvent::Stdout(line) => {
            if line.is_empty() {
                return;
            }
            let was_open = if let Some(active) = state.active_run.as_mut() {
                let was = active.assistant_open;
                active.assistant_open = true;
                if !active.assistant_buffer.is_empty() {
                    active.assistant_buffer.push('\n');
                }
                active.assistant_buffer.push_str(&line);
                was
            } else {
                false
            };
            let entry = if was_open {
                TranscriptEntry::assistant_cont(line)
            } else {
                TranscriptEntry::assistant(line)
            };
            state.push_entries([entry]);
        }
        RunEvent::Stderr(line) => {
            if line.is_empty() {
                return;
            }
            let was_open = if let Some(active) = state.active_run.as_mut() {
                let was = active.error_open;
                active.error_open = true;
                was
            } else {
                false
            };
            let entry = if was_open {
                TranscriptEntry::error_cont(line)
            } else {
                TranscriptEntry::error(line)
            };
            state.push_entries([entry]);
        }
        RunEvent::Completed(_) => {
            // The JoinHandle await in `pump_active_run` is the source of
            // truth for completion. The event is forwarded for stream-only
            // consumers; the TUI ignores it.
        }
    }
}

async fn handle_command(state: &mut TuiState, command: &str) -> Result<bool> {
    let mut parts = command.split_whitespace();
    match parts.next().unwrap_or_default() {
        "exit" | "quit" | "q" => Ok(true),
        "help" | "h" => {
            let mut entries = Vec::new();
            let mut last_category: Option<commands::Category> = None;
            for cmd in commands::COMMANDS {
                if last_category != Some(cmd.category) {
                    entries.push(TranscriptEntry::system(format!(
                        "── {} ──",
                        cmd.category.label()
                    )));
                    last_category = Some(cmd.category);
                }
                entries.push(TranscriptEntry::system(format!(
                    "{:<32}  {}",
                    cmd.usage, cmd.summary
                )));
            }
            state.push_entries(entries);
            Ok(false)
        }
        "skills" => {
            if state.skills.is_empty() {
                state.push_system("no skills found");
                return Ok(false);
            }
            let entries = state
                .skills
                .iter()
                .map(|skill| {
                    let active = if state.session.active_skills.contains(&skill.name) {
                        "*"
                    } else {
                        " "
                    };
                    let triggers = if skill.triggers.is_empty() {
                        String::new()
                    } else {
                        format!("  [triggers: {}]", skill.triggers.join(", "))
                    };
                    TranscriptEntry::system(format!(
                        "{active} {} — {}{}",
                        skill.name,
                        skill.summary(),
                        triggers
                    ))
                })
                .collect::<Vec<_>>();
            state.push_entries(entries);
            Ok(false)
        }
        "skill" => {
            let Some(name) = parts.next() else {
                if state.session.active_skills.is_empty() {
                    state.push_system("active skills: none");
                } else {
                    state.push_system(format!(
                        "active skills: {}",
                        state.session.active_skills.join(", ")
                    ));
                }
                return Ok(false);
            };
            if name == "edit" {
                let Some(target) = parts.next() else {
                    state.status = "Usage: /skill edit <name>".to_string();
                    return Ok(false);
                };
                let Some(skill) = state.skills.iter().find(|s| s.name == target) else {
                    state.status = format!("Unknown skill `{target}`");
                    return Ok(false);
                };
                state.push_entries([
                    TranscriptEntry::system(format!("path: {}", skill.path.display())),
                    TranscriptEntry::system(
                        "Open the path in your editor; reload with /skill reload",
                    ),
                ]);
                return Ok(false);
            }
            if name == "reload" {
                state.skills = discover_skills(&state.session.cwd).await?;
                state.status = format!("Reloaded {} skill(s)", state.skills.len());
                return Ok(false);
            }
            if name == "create" {
                let Some(new_name) = parts.next() else {
                    state.composer.replace("/skill create ".to_string());
                    state.status = "Enter a skill name".to_string();
                    return Ok(false);
                };
                match create_skill(&state.session.cwd, new_name).await {
                    Ok(skill) => {
                        state.status = format!("Created skill `{}`", skill.name);
                        state.session.active_skills.clear();
                        state.session.active_skills.push(skill.name.clone());
                        state.skills = discover_skills(&state.session.cwd).await?;
                        state.push_system(format!("created {}", skill.path.display()));
                        let _ = save_session(&state.sessions_dir, &state.session).await;
                    }
                    Err(err) => {
                        state.status = format!("{err}");
                    }
                }
                return Ok(false);
            }
            if matches!(name, "off" | "clear" | "none") {
                state.session.active_skills.clear();
                state.status = "Skills cleared".to_string();
                let _ = save_session(&state.sessions_dir, &state.session).await;
                return Ok(false);
            }
            if !state.skills.iter().any(|skill| skill.name == name) {
                state.status = format!("Unknown skill `{name}`");
                return Ok(false);
            }
            if !state
                .session
                .active_skills
                .iter()
                .any(|skill| skill == name)
            {
                state.session.active_skills.push(name.to_string());
            }
            state.status = format!("Active skills: {}", state.session.active_skills.join(", "));
            let _ = save_session(&state.sessions_dir, &state.session).await;
            Ok(false)
        }
        "bypass" => {
            state.session.bypass = parse_toggle(parts.next(), state.session.bypass);
            state.status = format!("Bypass mode {}", on_off(state.session.bypass));
            let _ = save_session(&state.sessions_dir, &state.session).await;
            Ok(false)
        }
        "desktop" => {
            state.session.desktop = parse_toggle(parts.next(), state.session.desktop);
            if state.session.desktop {
                state.session.bypass = true;
            }
            state.status = format!(
                "Desktop mode {}; bypass {}",
                on_off(state.session.desktop),
                on_off(state.session.bypass)
            );
            let _ = save_session(&state.sessions_dir, &state.session).await;
            Ok(false)
        }
        "clear" => {
            state.transcript.clear();
            state.scroll = 0;
            state.status = "Cleared transcript".to_string();
            Ok(false)
        }
        "profile" => {
            let Some(profile) = parts.next() else {
                state.push_system(format!("profile: {}", state.session.profile));
                return Ok(false);
            };
            if !state.config.profiles.contains_key(profile) {
                state.status = format!("Unknown profile `{profile}`");
                return Ok(false);
            }
            state.session.profile = profile.to_string();
            state.status = format!("Profile switched to {}", state.session.profile);
            let _ = save_session(&state.sessions_dir, &state.session).await;
            Ok(false)
        }
        "profiles" => {
            let entries: Vec<_> = state
                .config
                .profiles
                .keys()
                .map(|name| TranscriptEntry::system(format!("profile: {name}")))
                .collect();
            state.push_entries(entries);
            Ok(false)
        }
        "runs" => {
            let runs = list_runs(&state.runs_dir).await?;
            let entries: Vec<_> = runs
                .into_iter()
                .take(10)
                .map(|(_, record)| {
                    TranscriptEntry::system(format!(
                        "{} {:?} profile={} duration={}ms",
                        record.id, record.status, record.profile, record.duration_ms
                    ))
                })
                .collect();
            state.push_entries(entries);
            Ok(false)
        }
        "last" => {
            let id = state.last_run_id.clone();
            let Some((_, record)) = find_run(&state.runs_dir, id.as_deref()).await? else {
                state.status = "No runs yet".to_string();
                return Ok(false);
            };
            let transcript = read_transcript(&record).await?;
            state.push_entries(render_record(&record, transcript));
            Ok(false)
        }
        "retry" => {
            if state.active_run.is_some() {
                state.status = "Run already in progress".to_string();
                return Ok(false);
            }
            let id_opt = parts
                .next()
                .map(str::to_string)
                .or_else(|| state.last_run_id.clone());
            let Some((_, record)) = find_run(&state.runs_dir, id_opt.as_deref()).await? else {
                state.status = "No matching run found".to_string();
                return Ok(false);
            };
            start_run(state, record.prompt);
            Ok(false)
        }
        "new" => {
            if state.active_run.is_some() {
                state.status = "Run in progress; wait before starting a new session".to_string();
                return Ok(false);
            }
            save_session(&state.sessions_dir, &state.session).await?;
            let mut next = Session::new(state.session.profile.clone(), state.session.cwd.clone());
            next.bypass = state.session.bypass;
            next.desktop = state.session.desktop;
            next.timeout_secs = state.session.timeout_secs;
            save_session(&state.sessions_dir, &next).await?;
            state.session = next;
            state.last_run_id = None;
            state.skills = discover_skills(&state.session.cwd).await?;
            state.sync_after_session_swap();
            state.status = format!("New session {}", state.session.short_id());
            Ok(false)
        }
        "sessions" => {
            let sessions = list_sessions(&state.sessions_dir).await?;
            if sessions.is_empty() {
                state.push_system("no sessions yet");
                return Ok(false);
            }
            let current_id = state.session.id.clone();
            let entries = sessions
                .into_iter()
                .take(20)
                .map(|session| {
                    let active = if session.id == current_id { "*" } else { " " };
                    let name = session
                        .name
                        .clone()
                        .unwrap_or_else(|| "(unnamed)".to_string());
                    TranscriptEntry::system(format!(
                        "{active} {} profile={} turns={} updated={} {}",
                        session.short_id(),
                        session.profile,
                        session.turn_count(),
                        session.updated_at.format("%Y-%m-%d %H:%M"),
                        name
                    ))
                })
                .collect::<Vec<_>>();
            state.push_entries(entries);
            Ok(false)
        }
        "resume" => {
            if state.active_run.is_some() {
                state.status = "Run in progress; wait before resuming".to_string();
                return Ok(false);
            }
            let id_arg = parts.next();
            let next = match id_arg {
                Some(id) => load_session(&state.sessions_dir, id).await?,
                None => {
                    let mut sessions = list_sessions(&state.sessions_dir).await?;
                    sessions.retain(|session| session.id != state.session.id);
                    match sessions.into_iter().next() {
                        Some(session) => session,
                        None => {
                            state.status = "No other sessions to resume".to_string();
                            return Ok(false);
                        }
                    }
                }
            };
            save_session(&state.sessions_dir, &state.session).await?;
            state.session = next;
            state.last_run_id = state.session.run_ids.last().cloned();
            state.skills = discover_skills(&state.session.cwd).await?;
            state.sync_after_session_swap();
            state.status = format!("Resumed {}", state.session.short_id());
            Ok(false)
        }
        "fork" => {
            if state.active_run.is_some() {
                state.status = "Run in progress; wait before forking".to_string();
                return Ok(false);
            }
            let source = match parts.next() {
                Some(id) => load_session(&state.sessions_dir, id).await?,
                None => state.session.clone(),
            };
            let fork = source.fork();
            save_session(&state.sessions_dir, &state.session).await?;
            save_session(&state.sessions_dir, &fork).await?;
            state.session = fork;
            state.last_run_id = state.session.run_ids.last().cloned();
            state.skills = discover_skills(&state.session.cwd).await?;
            state.sync_after_session_swap();
            state.status = format!("Forked {}", state.session.short_id());
            Ok(false)
        }
        "status" => {
            let mode = mode_label(state);
            let skills = if state.session.active_skills.is_empty() {
                "none".to_string()
            } else {
                state.session.active_skills.join(", ")
            };
            state.push_entries([
                TranscriptEntry::system(format!(
                    "session: {} ({} turns)",
                    state.session.short_id(),
                    state.session.turn_count()
                )),
                TranscriptEntry::system(format!("profile: {}", state.session.profile)),
                TranscriptEntry::system(format!("cwd: {}", state.session.cwd.display())),
                TranscriptEntry::system(format!("mode: {mode}")),
                TranscriptEntry::system(format!("active skills: {skills}")),
                TranscriptEntry::system(format!(
                    "last run: {}",
                    state.last_run_id.as_deref().unwrap_or("(none)")
                )),
            ]);
            Ok(false)
        }
        "model" => {
            let Some(profile) = state.config.profiles.get(&state.session.profile) else {
                state.status = format!("Profile `{}` is no longer defined", state.session.profile);
                return Ok(false);
            };
            state.push_entries([
                TranscriptEntry::system(format!("profile: {}", state.session.profile)),
                TranscriptEntry::system(format!("command: {}", profile.command.join(" "))),
            ]);
            Ok(false)
        }
        "permissions" => {
            let arg = parts.next();
            match arg {
                None => {
                    state.push_system(format!("permissions: {}", mode_label(state)));
                }
                Some("guarded") => {
                    state.session.bypass = false;
                    state.session.desktop = false;
                    save_session(&state.sessions_dir, &state.session).await?;
                    state.status = "permissions: guarded".to_string();
                }
                Some("bypass") => {
                    state.session.bypass = true;
                    state.session.desktop = false;
                    save_session(&state.sessions_dir, &state.session).await?;
                    state.status = "permissions: bypass".to_string();
                }
                Some("desktop") => {
                    state.session.bypass = true;
                    state.session.desktop = true;
                    save_session(&state.sessions_dir, &state.session).await?;
                    state.status = "permissions: desktop".to_string();
                }
                Some(other) => {
                    state.status = format!("Unknown permission mode `{other}`");
                }
            }
            Ok(false)
        }
        "compact" => {
            let keep = parts
                .next()
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(20);
            let before = state.session.transcript.len();
            if before > keep {
                let drop_count = before - keep;
                state.session.transcript.drain(..drop_count);
                save_session(&state.sessions_dir, &state.session).await?;
                state.sync_after_session_swap();
                state.status = format!("compacted: dropped {drop_count} turn(s)");
            } else {
                state.status = format!("nothing to compact ({before} turns)");
            }
            Ok(false)
        }
        "smoke" => {
            if state.active_run.is_some() {
                state.status = "Run already in progress".to_string();
                return Ok(false);
            }
            let prompt = parts.collect::<Vec<_>>().join(" ");
            let prompt = if prompt.trim().is_empty() {
                "Reply exactly: ok".to_string()
            } else {
                prompt
            };
            start_run(state, prompt);
            Ok(false)
        }
        "inspect" => {
            let id_opt = parts
                .next()
                .map(str::to_string)
                .or_else(|| state.last_run_id.clone());
            let Some((path, record)) = find_run(&state.runs_dir, id_opt.as_deref()).await? else {
                state.status = "No matching run found".to_string();
                return Ok(false);
            };
            let body = serde_json::to_string_pretty(&record)
                .unwrap_or_else(|err| format!("(failed to serialize: {err})"));
            let entries = std::iter::once(TranscriptEntry::system(format!(
                "run dir: {}",
                path.display()
            )))
            .chain(body.lines().map(TranscriptEntry::system))
            .collect::<Vec<_>>();
            state.push_entries(entries);
            Ok(false)
        }
        "open-run" => {
            let Some(id) = parts.next() else {
                state.status = "Usage: /open-run <id>".to_string();
                return Ok(false);
            };
            let id = id.to_string();
            let Some((_, record)) = find_run(&state.runs_dir, Some(&id)).await? else {
                state.status = format!("No run matching `{id}`");
                return Ok(false);
            };
            let transcript = read_transcript(&record).await?;
            state.push_entries(render_record(&record, transcript));
            Ok(false)
        }
        "logs" => {
            let id_opt = parts
                .next()
                .map(str::to_string)
                .or_else(|| state.last_run_id.clone());
            let Some((_, record)) = find_run(&state.runs_dir, id_opt.as_deref()).await? else {
                state.status = "No matching run found".to_string();
                return Ok(false);
            };
            state.push_entries([
                TranscriptEntry::system(format!("run id: {}", record.id)),
                TranscriptEntry::system(format!("stdout: {}", record.stdout_log.display())),
                TranscriptEntry::system(format!("stderr: {}", record.stderr_log.display())),
            ]);
            Ok(false)
        }
        "export" => {
            let Some(path) = parts.next() else {
                state.status = "Usage: /export <path>".to_string();
                return Ok(false);
            };
            let path = PathBuf::from(path);
            if tokio::fs::try_exists(&path).await.unwrap_or(false) {
                state.status = format!("Refusing to overwrite {}", path.display());
                return Ok(false);
            }
            let body = session_to_markdown(&state.session);
            tokio::fs::write(&path, body)
                .await
                .with_context(|| format!("failed to write {}", path.display()))?;
            state.status = format!("Exported to {}", path.display());
            Ok(false)
        }
        "provider" => {
            match parts.next() {
                None | Some("show") => {
                    let msg = match &state.session.provider_session_id {
                        Some(id) => format!("provider session id: {id}"),
                        None => "provider session id: (none)".to_string(),
                    };
                    state.push_system(msg);
                }
                Some("clear") => {
                    state.session.provider_session_id = None;
                    save_session(&state.sessions_dir, &state.session).await?;
                    state.status = "provider session id cleared".to_string();
                }
                Some("set") => {
                    let Some(id) = parts.next() else {
                        state.status = "Usage: /provider set <id>".to_string();
                        return Ok(false);
                    };
                    state.session.provider_session_id = Some(id.to_string());
                    save_session(&state.sessions_dir, &state.session).await?;
                    state.status = format!("provider session id: {id}");
                }
                Some(other) => {
                    state.status = format!("Unknown /provider subcommand `{other}`");
                }
            }
            Ok(false)
        }
        "jobs" => {
            if state.active_run.is_some() {
                state.status = "Run already in progress".to_string();
                return Ok(false);
            }
            let Some(file) = parts.next() else {
                state.status = "Usage: /jobs <file> [concurrency]".to_string();
                return Ok(false);
            };
            let concurrency = parts
                .next()
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(2);
            let file = PathBuf::from(file);
            state.push_system(format!(
                "Running batch from {} (concurrency={concurrency}). The UI is frozen until the batch completes.",
                file.display()
            ));
            let jobs = forge_cli::read_jobs(&file).await?;
            let total = jobs.len();
            let results = forge_cli::run_jobs(
                state.config.clone(),
                state.runs_dir.clone(),
                jobs,
                concurrency,
            )
            .await;
            let mut succeeded = 0usize;
            let mut failed = 0usize;
            let mut entries = Vec::with_capacity(results.len());
            for result in results {
                match result {
                    Ok(record) => {
                        if record.status == RunStatus::Succeeded {
                            succeeded += 1;
                        } else {
                            failed += 1;
                        }
                        entries.push(TranscriptEntry::system(format!(
                            "{} {:?} profile={} duration={}ms",
                            record.id, record.status, record.profile, record.duration_ms
                        )));
                    }
                    Err(err) => {
                        failed += 1;
                        entries.push(TranscriptEntry::error(format!("job failed: {err:#}")));
                    }
                }
            }
            state.push_entries(entries);
            state.status = format!("batch done: {succeeded}/{total} succeeded, {failed} failed");
            Ok(false)
        }
        unknown => {
            state.status = format!("Unknown command: /{unknown}");
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

/// Build the skill preamble for `user_prompt`. This is the union of:
///
/// 1. Skills that are explicitly active in the session (sticky).
/// 2. Skills whose trigger phrases match the prompt (one-shot per turn).
///
/// Skills with no triggers and that aren't activated are *not* injected, so
/// the prompt no longer balloons with every discovered SKILL.md.
fn active_skill_prompt_for(state: &TuiState, user_prompt: &str) -> Option<String> {
    let mut chosen: Vec<&Skill> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for name in &state.session.active_skills {
        if let Some(skill) = state.skills.iter().find(|s| &s.name == name)
            && seen.insert(skill.name.as_str())
        {
            chosen.push(skill);
        }
    }
    for skill in &state.skills {
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

fn render_record(record: &forge_cli::RunRecord, transcript: String) -> Vec<TranscriptEntry> {
    let mut lines = vec![TranscriptEntry::system(format!(
        "{} {:?} profile={} duration={}ms",
        record.id, record.status, record.profile, record.duration_ms
    ))];
    let (stdout, stderr) = split_transcript(&transcript);
    if !stdout.trim().is_empty() {
        lines.extend(role_lines(stdout.lines(), TranscriptKind::Assistant));
    }
    if record.status != RunStatus::Succeeded || !stderr.trim().is_empty() {
        lines.push(TranscriptEntry::error(format!(
            "exit code: {:?}",
            record.exit_code
        )));
        if !stderr.trim().is_empty() {
            lines.extend(role_lines(stderr.lines(), TranscriptKind::Error));
        }
    }
    lines
}

fn role_lines<'a>(
    lines: impl Iterator<Item = &'a str>,
    role: TranscriptKind,
) -> impl Iterator<Item = TranscriptEntry> {
    lines
        .filter(|line| !line.trim().is_empty())
        .enumerate()
        .map(move |(index, line)| match (role, index) {
            (TranscriptKind::Assistant, 0) => TranscriptEntry::assistant(line),
            (TranscriptKind::Assistant, _) => TranscriptEntry::assistant_cont(line),
            (TranscriptKind::Error, 0) => TranscriptEntry::error(line),
            (TranscriptKind::Error, _) => TranscriptEntry::error_cont(line),
            _ => TranscriptEntry::system(line),
        })
}

fn split_transcript(transcript: &str) -> (&str, &str) {
    let Some(stdout_start) = transcript.strip_prefix("stdout:\n") else {
        return (transcript, "");
    };
    match stdout_start.split_once("\n\nstderr:\n") {
        Some((stdout, stderr)) => (stdout, stderr),
        None => (stdout_start, ""),
    }
}

fn enter_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    terminal::enable_raw_mode().context("failed to enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, terminal::EnterAlternateScreen).context("failed to enter alternate screen")?;
    // Best-effort: terminals that don't understand bracketed paste will simply
    // deliver paste contents as individual key events. We don't bail if this
    // sequence is rejected.
    let _ = execute!(stdout, event::EnableBracketedPaste);
    Terminal::new(CrosstermBackend::new(stdout)).context("failed to create terminal")
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), event::DisableBracketedPaste);
        let _ = terminal::disable_raw_mode();
        let _ = execute!(io::stdout(), terminal::LeaveAlternateScreen);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    #[test]
    fn composer_chip_hides_when_empty_or_known_command() {
        assert_eq!(composer_chip(""), None);
        assert_eq!(composer_chip("   "), None);
        assert_eq!(composer_chip("hello there"), None, "plain prompt → no chip");
        assert_eq!(
            composer_chip("/help"),
            None,
            "known command → no chip; default look applies"
        );
        assert_eq!(composer_chip("/skill foo"), None);
    }

    #[test]
    fn composer_chip_flags_path_like_input() {
        assert_eq!(
            composer_chip("/home/mrmoe28/Project"),
            Some(ComposerChip::Path)
        );
        assert_eq!(
            composer_chip("/no/such/dir/here"),
            Some(ComposerChip::Path),
            "nested missing path still flags as Path via heuristic"
        );
    }

    #[test]
    fn composer_chip_flags_unknown_slash() {
        assert_eq!(composer_chip("/foobar"), Some(ComposerChip::UnknownSlash));
    }

    #[test]
    fn composer_chip_labels_are_padded_and_short() {
        // Padding spaces on each side keep the chip readable when rendered as
        // a colored block on the border.
        let path = ComposerChip::Path.label();
        assert!(path.starts_with(' ') && path.ends_with(' '));
        assert!(path.trim().len() <= 10);
        let unknown = ComposerChip::UnknownSlash.label();
        assert!(unknown.starts_with(' ') && unknown.ends_with(' '));
        assert!(unknown.trim().len() <= 10);
    }

    #[test]
    fn status_bar_idle_with_home_abbreviated() {
        let cwd = std::path::PathBuf::from("/home/me/forge-CLI");
        let home = std::path::PathBuf::from("/home/me");
        let data = build_status_bar_data("4f3a8b", &cwd, Some(&home), "guarded", false, "");
        assert_eq!(data.run_state, "○ idle");
        assert_eq!(data.session, "session 4f3a8b");
        assert_eq!(data.cwd, "cwd ~/forge-CLI");
        assert_eq!(data.mode, "guarded");
        assert!(data.message.is_none(), "no ephemeral status → no message");
    }

    #[test]
    fn status_bar_running_shows_running_marker_and_message() {
        let cwd = std::path::PathBuf::from("/tmp/repo");
        let data = build_status_bar_data("abcdef", &cwd, None, "bypass", true, "Running…");
        assert_eq!(data.run_state, "● running");
        assert_eq!(data.cwd, "cwd /tmp/repo", "no home → keep absolute path");
        assert_eq!(data.mode, "bypass");
        assert_eq!(data.message.as_deref(), Some("Running…"));
    }

    #[test]
    fn status_bar_trims_whitespace_only_messages() {
        let cwd = std::path::PathBuf::from("/x");
        let data = build_status_bar_data("a", &cwd, None, "guarded", false, "   \t  ");
        assert!(
            data.message.is_none(),
            "whitespace-only ephemeral status must collapse to None"
        );
    }

    #[test]
    fn abbreviate_home_handles_exact_match_and_outside_paths() {
        let home = std::path::PathBuf::from("/home/me");
        assert_eq!(
            abbreviate_home(&std::path::PathBuf::from("/home/me"), Some(&home)),
            "~"
        );
        assert_eq!(
            abbreviate_home(
                &std::path::PathBuf::from("/home/me/projects/x"),
                Some(&home)
            ),
            "~/projects/x"
        );
        // Path outside home is returned verbatim.
        assert_eq!(
            abbreviate_home(&std::path::PathBuf::from("/var/log"), Some(&home)),
            "/var/log"
        );
        // Prefix-but-not-segment match must NOT be abbreviated.
        assert_eq!(
            abbreviate_home(&std::path::PathBuf::from("/home/megan/repo"), Some(&home)),
            "/home/megan/repo"
        );
    }

    #[test]
    fn cancel_returns_false_when_no_active_run() {
        let mut active_run: Option<ActiveRun> = None;
        let mut transcript: Vec<TranscriptEntry> = Vec::new();
        let mut status = String::new();
        assert!(!cancel_active_run(
            &mut active_run,
            &mut transcript,
            &mut status
        ));
        assert!(transcript.is_empty());
        assert!(status.is_empty());
    }

    #[tokio::test]
    async fn cancel_aborts_handle_and_clears_state() {
        let (_tx, rx) = mpsc::unbounded_channel();
        // A future that would never complete on its own; we'll abort it.
        let handle = tokio::spawn(async {
            std::future::pending::<()>().await;
            unreachable!("the task is aborted before it can finish")
        });
        let mut active_run = Some(ActiveRun {
            handle,
            rx,
            assistant_open: false,
            error_open: false,
            assistant_buffer: String::new(),
        });
        let mut transcript: Vec<TranscriptEntry> = Vec::new();
        let mut status = "Running…".to_string();

        let cancelled = cancel_active_run(&mut active_run, &mut transcript, &mut status);

        assert!(cancelled);
        assert!(active_run.is_none(), "active_run slot should be cleared");
        assert_eq!(transcript.len(), 1);
        assert!(
            transcript[0].text.starts_with("Run cancelled"),
            "expected a `Run cancelled` system line, got: {:?}",
            transcript[0].text
        );
        assert_eq!(status, "Run cancelled");
    }

    #[tokio::test]
    async fn cancel_notes_partial_output_when_streams_were_open() {
        let (_tx, rx) = mpsc::unbounded_channel();
        let handle = tokio::spawn(async {
            std::future::pending::<()>().await;
            unreachable!()
        });
        let mut active_run = Some(ActiveRun {
            handle,
            rx,
            assistant_open: true,
            error_open: false,
            assistant_buffer: "partial response so far".to_string(),
        });
        let mut transcript: Vec<TranscriptEntry> = Vec::new();
        let mut status = String::new();

        cancel_active_run(&mut active_run, &mut transcript, &mut status);

        assert!(
            transcript[0].text.contains("partial output preserved"),
            "expected partial-output hint, got: {:?}",
            transcript[0].text
        );
    }

    #[test]
    fn classifies_known_command() {
        assert!(matches!(classify_input("/help"), InputClass::Command(_)));
        assert!(matches!(
            classify_input("/skill foo"),
            InputClass::Command(_)
        ));
        // Alias resolves through commands::is_known.
        assert!(matches!(classify_input("/q"), InputClass::Command(_)));
        // Leading/trailing whitespace is tolerated.
        assert!(matches!(
            classify_input("  /status   "),
            InputClass::Command(_)
        ));
    }

    #[test]
    fn classifies_nested_path_as_path_even_if_missing() {
        assert_eq!(
            classify_input("/no/such/path/here"),
            InputClass::Path,
            "multi-segment slash input should be a Path"
        );
        assert_eq!(classify_input("/home/mrmoe28/Project"), InputClass::Path);
    }

    #[test]
    fn classifies_existing_absolute_path_as_path() {
        // Skip if `/tmp` is missing (unlikely on dev hosts).
        if std::path::Path::new("/tmp").exists() {
            assert_eq!(classify_input("/tmp"), InputClass::Path);
        }
    }

    #[test]
    fn classifies_unknown_single_token_slash() {
        match classify_input("/foobar") {
            InputClass::UnknownSlash(name) => assert_eq!(name, "foobar"),
            other => panic!("expected UnknownSlash, got {other:?}"),
        }
    }

    #[test]
    fn classifies_plain_prompt() {
        assert_eq!(classify_input("hello world"), InputClass::Prompt);
        assert_eq!(classify_input(""), InputClass::Prompt);
        assert_eq!(classify_input("./relative/path"), InputClass::Prompt);
    }

    #[test]
    fn classifier_extracts_command_body() {
        match classify_input("/skill foo") {
            InputClass::Command(body) => assert_eq!(body, "skill foo"),
            other => panic!("expected Command, got {other:?}"),
        }
        match classify_input("/help") {
            InputClass::Command(body) => assert_eq!(body, "help"),
            other => panic!("expected Command, got {other:?}"),
        }
        // Path-looking inputs are not commands.
        assert!(!matches!(
            classify_input("/home/mrmoe28/Project"),
            InputClass::Command(_)
        ));
        // Unknown single-token slash is not a command either.
        assert!(!matches!(classify_input("/foobar"), InputClass::Command(_)));
    }
}
