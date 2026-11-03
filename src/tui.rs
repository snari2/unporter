//! `unporter tui`: a live, interactive view of listening ports.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use ratatui::DefaultTerminal;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Clear, Paragraph, Row, Table, TableState, Wrap};

use crate::kill::{self, Action, KillOptions, Outcome};
use crate::output::{path_label, project_label};
use crate::scan::{self, PortEntry};
use crate::util::{self, human_bytes, human_duration, tilde};

const REFRESH_EVERY: Duration = Duration::from_secs(2);
const STATUS_FOR: Duration = Duration::from_secs(5);

enum Request {
    Refresh,
    Kill(Vec<Action>, KillOptions),
}

enum Message {
    Entries(Result<Vec<PortEntry>, String>),
    KillDone { summary: String, ok: bool },
}

#[derive(Debug, PartialEq)]
enum Mode {
    Normal,
    Filter,
    Confirm { actions: Vec<Action>, force: bool },
    Help,
}

struct App {
    entries: Vec<PortEntry>,
    loaded: bool,
    show_all: bool,
    filter: String,
    table: TableState,
    /// (port, pid) of the selected row, so selection survives refreshes.
    selected: Option<(u16, u32)>,
    mode: Mode,
    status: Option<(String, bool, Instant)>,
    busy: bool,
    quit: bool,
    /// Quit requested while a kill was running: leave once it reports back.
    quit_when_done: bool,
    final_summary: Option<String>,
}

pub fn run(show_all: bool) -> Result<()> {
    let (req_tx, req_rx) = mpsc::channel();
    let (msg_tx, msg_rx) = mpsc::channel();
    thread::spawn(move || worker(req_rx, msg_tx));

    let mut terminal = ratatui::init();
    // With bracketed paste, pasted text arrives as one Paste event instead of
    // keystrokes, so pasting `y` or a command can't drive the UI.
    let _ = crossterm::execute!(std::io::stdout(), EnableBracketedPaste);
    let result = event_loop(&mut terminal, show_all, &req_tx, &msg_rx);
    let _ = crossterm::execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    if let Ok(Some(summary)) = &result {
        eprintln!("{summary}");
    }
    result.map(|_| ())
}

/// Scans (every couple of seconds or on request) and kills off the UI thread
/// so the interface never freezes while a process takes its time to exit.
fn worker(requests: Receiver<Request>, messages: Sender<Message>) {
    let send_scan = |messages: &Sender<Message>| {
        let entries = scan::scan().map_err(|e| e.to_string());
        messages.send(Message::Entries(entries)).is_ok()
    };
    if !send_scan(&messages) {
        return;
    }
    loop {
        match requests.recv_timeout(REFRESH_EVERY) {
            Ok(Request::Refresh) | Err(RecvTimeoutError::Timeout) => {}
            Ok(Request::Kill(actions, opts)) => {
                let (summary, ok) = run_actions(&actions, opts);
                if messages.send(Message::KillDone { summary, ok }).is_err() {
                    return;
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
        if !send_scan(&messages) {
            return;
        }
    }
}

fn run_actions(actions: &[Action], opts: KillOptions) -> (String, bool) {
    let mut parts = Vec::new();
    let mut all_ok = true;
    for action in actions {
        let outcome = kill::execute(action, opts);
        all_ok &= outcome.is_success();
        let ports = ports_label(action.ports());
        parts.push(match outcome {
            Outcome::Exited => format!("stopped {} — freed {ports}", action.describe()),
            Outcome::Killed => format!("force-killed {} — freed {ports}", action.describe()),
            Outcome::Gone => format!("{} had already exited", action.describe()),
            Outcome::Stopped => format!("stopped {} — freed {ports}", action.describe()),
            Outcome::Skipped(reason) | Outcome::Failed(reason) => reason,
        });
    }
    (parts.join("; "), all_ok)
}

fn ports_label(ports: &[u16]) -> String {
    ports
        .iter()
        .map(|p| format!(":{p}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Runs until the user quits. Returns the summary of a kill that finished
/// while quitting, so it can be printed after the screen is restored.
fn event_loop(
    terminal: &mut DefaultTerminal,
    show_all: bool,
    requests: &Sender<Request>,
    messages: &Receiver<Message>,
) -> Result<Option<String>> {
    let mut app = App {
        entries: Vec::new(),
        loaded: false,
        show_all,
        filter: String::new(),
        table: TableState::default(),
        selected: None,
        mode: Mode::Normal,
        status: None,
        busy: false,
        quit: false,
        quit_when_done: false,
        final_summary: None,
    };
    while !app.quit {
        while let Ok(msg) = messages.try_recv() {
            app.handle_message(msg);
        }
        if app
            .status
            .as_ref()
            .is_some_and(|(_, _, at)| at.elapsed() > STATUS_FOR)
        {
            app.status = None;
        }
        terminal.draw(|f| draw(f, &mut app))?;
        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            app.handle_key(key, requests);
        }
    }
    Ok(app.final_summary)
}

impl App {
    fn visible(&self) -> Vec<&PortEntry> {
        let needle = self.filter.to_lowercase();
        self.entries
            .iter()
            .filter(|e| self.show_all || e.dev)
            .filter(|e| needle.is_empty() || matches_filter(e, &needle))
            .collect()
    }

    fn hidden_count(&self) -> usize {
        self.entries.iter().filter(|e| !e.dev).count()
    }

    fn selected_entry(&self) -> Option<&PortEntry> {
        let visible = self.visible();
        self.table.selected().and_then(|i| visible.get(i).copied())
    }

    /// Re-point the table selection at the remembered (port, pid), falling
    /// back to the nearest row when it disappeared.
    fn sync_selection(&mut self) {
        let visible = self.visible();
        if visible.is_empty() {
            self.table.select(None);
            return;
        }
        let idx = self
            .selected
            .and_then(|key| visible.iter().position(|e| (e.port, e.pid) == key))
            .unwrap_or_else(|| self.table.selected().unwrap_or(0).min(visible.len() - 1));
        self.selected = Some((visible[idx].port, visible[idx].pid));
        self.table.select(Some(idx));
    }

    fn move_selection(&mut self, delta: isize) {
        let len = self.visible().len();
        if len == 0 {
            return;
        }
        let current = self.table.selected().unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, len as isize - 1) as usize;
        self.table.select(Some(next));
        let e = self.visible()[next];
        self.selected = Some((e.port, e.pid));
    }

    fn set_status(&mut self, text: impl Into<String>, ok: bool) {
        self.status = Some((text.into(), ok, Instant::now()));
    }

    fn handle_message(&mut self, msg: Message) {
        match msg {
            Message::Entries(Ok(entries)) => {
                self.entries = entries;
                self.loaded = true;
                self.sync_selection();
            }
            Message::Entries(Err(e)) => {
                self.loaded = true;
                self.set_status(format!("scan failed: {e}"), false);
            }
            Message::KillDone { summary, ok } => {
                self.busy = false;
                if self.quit_when_done {
                    self.quit = true;
                    self.final_summary = Some(summary.clone());
                }
                self.set_status(summary, ok);
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent, requests: &Sender<Request>) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        match &self.mode {
            Mode::Help => self.mode = Mode::Normal,
            Mode::Filter => match key.code {
                KeyCode::Esc => {
                    self.filter.clear();
                    self.mode = Mode::Normal;
                    self.sync_selection();
                }
                KeyCode::Enter => self.mode = Mode::Normal,
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.sync_selection();
                }
                KeyCode::Down => self.move_selection(1),
                KeyCode::Up => self.move_selection(-1),
                KeyCode::Char(c) => {
                    self.filter.push(c);
                    self.sync_selection();
                }
                _ => {}
            },
            Mode::Confirm { actions, .. }
                if actions.iter().all(|a| matches!(a, Action::Skip { .. })) =>
            {
                self.mode = Mode::Normal;
            }
            // Only an explicit `y` confirms; anything else (including Enter, so
            // a command typed into the wrong window can't kill) cancels.
            Mode::Confirm { .. } => match key.code {
                KeyCode::Char('y' | 'Y') => {
                    let Mode::Confirm { actions, force } =
                        std::mem::replace(&mut self.mode, Mode::Normal)
                    else {
                        unreachable!()
                    };
                    let opts = KillOptions {
                        force,
                        ..KillOptions::default()
                    };
                    self.busy = true;
                    self.set_status("stopping…", true);
                    let _ = requests.send(Request::Kill(actions, opts));
                }
                _ => self.mode = Mode::Normal,
            },
            Mode::Normal => match key.code {
                KeyCode::Char('q') => self.request_quit(),
                KeyCode::Esc if !self.filter.is_empty() => {
                    self.filter.clear();
                    self.sync_selection();
                }
                KeyCode::Esc => self.request_quit(),
                KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
                KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
                KeyCode::PageDown => self.move_selection(10),
                KeyCode::PageUp => self.move_selection(-10),
                KeyCode::Home | KeyCode::Char('g') => self.move_selection(isize::MIN / 2),
                KeyCode::End | KeyCode::Char('G') => self.move_selection(isize::MAX / 2),
                KeyCode::Char('x' | 'd') | KeyCode::Delete => self.ask_kill(false),
                KeyCode::Char('X' | 'D') => self.ask_kill(true),
                KeyCode::Char('o') | KeyCode::Enter => {
                    if let Some(e) = self.selected_entry() {
                        let url = e.url();
                        match util::open_url(&url) {
                            Ok(()) => self.set_status(format!("opened {url}"), true),
                            Err(err) => {
                                self.set_status(format!("could not open {url}: {err}"), false)
                            }
                        }
                    }
                }
                KeyCode::Char('a') => {
                    self.show_all = !self.show_all;
                    self.sync_selection();
                }
                KeyCode::Char('r') => {
                    let _ = requests.send(Request::Refresh);
                    self.set_status("refreshing…", true);
                }
                KeyCode::Char('/') => self.mode = Mode::Filter,
                KeyCode::Char('?') => self.mode = Mode::Help,
                _ => {}
            },
        }
    }

    /// Quit, unless a kill is still running: then wait for it (so SIGKILL
    /// escalation still happens) unless asked a second time.
    fn request_quit(&mut self) {
        if !self.busy || self.quit_when_done {
            self.quit = true;
        } else {
            self.quit_when_done = true;
            self.set_status(
                "still stopping; will quit when done (press q again to leave now)",
                true,
            );
        }
    }

    fn ask_kill(&mut self, force: bool) {
        if self.busy {
            self.set_status("still stopping the previous process…", false);
            return;
        }
        let Some(sel) = self.selected_entry() else {
            return;
        };
        // Everything the same process (or container) holds goes with it.
        let related: Vec<&PortEntry> = self
            .entries
            .iter()
            .filter(|e| match (&sel.container, &e.container) {
                (Some(a), Some(b)) => a == b,
                (None, None) => e.pid == sel.pid,
                _ => false,
            })
            .collect();
        let actions = kill::plan(related);
        self.mode = Mode::Confirm { actions, force };
    }
}

fn matches_filter(e: &PortEntry, needle: &str) -> bool {
    let haystacks = [
        e.port.to_string(),
        e.pid.to_string(),
        e.process.to_lowercase(),
        e.kind.unwrap_or("").to_lowercase(),
        project_label(e).to_lowercase(),
        path_label(e).to_lowercase(),
    ];
    haystacks.iter().any(|h| h.contains(needle))
}

const ACCENT: Color = Color::Cyan;

fn draw(f: &mut Frame, app: &mut App) {
    let [header, body, details, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(5),
        Constraint::Length(8),
        Constraint::Length(1),
    ])
    .areas(f.area());

    draw_header(f, app, header);
    draw_table(f, app, body);
    draw_details(f, app, details);
    draw_footer(f, app, footer);

    match &app.mode {
        Mode::Confirm { actions, force } => draw_confirm(f, actions, *force),
        Mode::Help => draw_help(f),
        _ => {}
    }
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let visible = app.visible().len();
    let mut spans = vec![
        Span::styled(
            " unporter ",
            Style::new().bg(ACCENT).fg(Color::Black).bold(),
        ),
        Span::raw("  "),
        Span::raw(if app.loaded {
            format!(
                "{visible} {}",
                if app.show_all {
                    "listeners"
                } else {
                    "dev servers"
                }
            )
        } else {
            "scanning…".to_owned()
        }),
    ];
    let hidden = app.hidden_count();
    if !app.show_all && hidden > 0 {
        spans.push(Span::styled(
            format!("  ({hidden} hidden, press a)"),
            Style::new().dim(),
        ));
    }
    if !app.filter.is_empty() || app.mode == Mode::Filter {
        spans.push(Span::raw("  filter: "));
        spans.push(Span::styled(
            app.filter.clone(),
            Style::new().fg(Color::Yellow),
        ));
        if app.mode == Mode::Filter {
            spans.push(Span::styled("▏", Style::new().fg(Color::Yellow)));
        }
    }
    f.render_widget(Line::from(spans), area);
}

fn draw_table(f: &mut Frame, app: &mut App, area: Rect) {
    let visible = app.visible();
    let rows: Vec<Row> = visible
        .iter()
        .map(|e| {
            let base = if e.dev {
                Style::new()
            } else {
                Style::new().dim()
            };
            Row::new(vec![
                Cell::from(e.port.to_string()).style(Style::new().fg(ACCENT).bold()),
                Cell::from(e.pid.to_string()),
                Cell::from(e.process.clone()).bold(),
                Cell::from(e.kind.unwrap_or("-")).fg(Color::Magenta),
                Cell::from(project_label(e)).fg(Color::Green),
                Cell::from(e.bind_label()),
                Cell::from(
                    e.uptime_secs
                        .map(human_duration)
                        .unwrap_or_else(|| "-".into()),
                ),
                Cell::from(
                    e.memory_bytes
                        .map(human_bytes)
                        .unwrap_or_else(|| "-".into()),
                ),
            ])
            .style(base)
        })
        .collect();
    let empty = rows.is_empty();
    let widths = [
        Constraint::Length(6),
        Constraint::Length(7),
        Constraint::Fill(2),
        Constraint::Length(16),
        Constraint::Fill(2),
        Constraint::Length(10),
        Constraint::Length(8),
        Constraint::Length(9),
    ];
    let header = Row::new([
        "PORT", "PID", "PROCESS", "KIND", "PROJECT", "BIND", "UPTIME", "MEM",
    ])
    .style(Style::new().bold().underlined());
    let table = Table::new(rows, widths)
        .header(header)
        .row_highlight_style(
            Style::new()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ")
        .block(Block::bordered().title(" ports "));
    f.render_stateful_widget(table, area, &mut app.table);

    if empty && app.loaded {
        let msg = if app.filter.is_empty() {
            "No dev servers listening. Press a to show everything."
        } else {
            "Nothing matches the filter. Press Esc to clear it."
        };
        let inner = area.centered_vertically(Constraint::Length(1));
        f.render_widget(Paragraph::new(msg).centered().dim(), inner);
    }
}

fn draw_details(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::bordered().title(" details ");
    let Some(e) = app.selected_entry() else {
        f.render_widget(block, area);
        return;
    };
    let label = |s: &'static str| Span::styled(format!("{s:<10}"), Style::new().dim());
    let mut lines = vec![Line::from(vec![
        label("command"),
        Span::raw(e.command_line()),
    ])];
    if let Some(c) = &e.container {
        lines.push(Line::from(vec![
            label("container"),
            Span::raw(c.clone()).fg(Color::Green),
        ]));
    }
    if let Some(p) = &e.project {
        lines.push(Line::from(vec![
            label("project"),
            Span::raw(p.name.clone()).fg(Color::Green),
            Span::styled(
                format!("  {} ({})", tilde(&p.root), p.marker),
                Style::new().dim(),
            ),
        ]));
    } else if let Some(cwd) = &e.cwd {
        lines.push(Line::from(vec![label("cwd"), Span::raw(tilde(cwd))]));
    }
    if let (Some(pp), Some(cmd)) = (e.parent_pid, &e.parent_command) {
        lines.push(Line::from(vec![
            label("parent"),
            Span::raw(format!("{pp}  {cmd}")),
        ]));
    }
    let addrs: Vec<String> = e.addresses.iter().map(|a| a.to_string()).collect();
    let mut listen = vec![
        label("listen"),
        Span::raw(addrs.join(", ")),
        Span::styled(format!("   {}", e.url()), Style::new().fg(ACCENT)),
    ];
    if e.addresses.iter().any(|a| !a.is_loopback()) {
        listen.push(Span::styled(
            "   reachable from other machines",
            Style::new().fg(Color::Yellow),
        ));
    }
    lines.push(Line::from(listen));
    if let Some(u) = &e.user {
        lines.push(Line::from(vec![label("user"), Span::raw(u.clone())]));
    }
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(block),
        area,
    );
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    if let Some((text, ok, _)) = &app.status {
        let color = if *ok { Color::Green } else { Color::Red };
        f.render_widget(
            Line::styled(format!(" {text}"), Style::new().fg(color)),
            area,
        );
        return;
    }
    let keys: &[(&str, &str)] = match app.mode {
        Mode::Filter => &[("type", "filter"), ("enter", "done"), ("esc", "clear")],
        _ => &[
            ("↑↓", "move"),
            ("x", "kill"),
            ("X", "force kill"),
            ("o", "open"),
            ("/", "filter"),
            ("a", "all"),
            ("?", "help"),
            ("q", "quit"),
        ],
    };
    let mut spans = vec![Span::raw(" ")];
    for (key, what) in keys {
        spans.push(Span::styled(*key, Style::new().fg(ACCENT).bold()));
        spans.push(Span::styled(format!(" {what}  "), Style::new().dim()));
    }
    f.render_widget(Line::from(spans), area);
}

fn popup(f: &mut Frame, width: u16, height: u16) -> Rect {
    let area = f.area().centered(
        Constraint::Length(width.min(f.area().width)),
        Constraint::Length(height.min(f.area().height)),
    );
    f.render_widget(Clear, area);
    area
}

fn draw_confirm(f: &mut Frame, actions: &[Action], force: bool) {
    let verb = if force {
        "Force kill (SIGKILL)"
    } else {
        "Stop"
    };
    let mut lines = Vec::new();
    for a in actions {
        lines.push(match a {
            Action::Skip { reason, .. } => {
                Line::styled(format!("✗ {reason}"), Style::new().fg(Color::Yellow))
            }
            _ => Line::from(vec![
                Span::raw(format!("{verb} ")),
                Span::styled(a.describe(), Style::new().bold()),
                Span::raw(format!(" on {}?", ports_label(a.ports()))),
            ]),
        });
    }
    lines.push(Line::raw(""));
    let any_runnable = actions.iter().any(|a| !matches!(a, Action::Skip { .. }));
    lines.push(if any_runnable {
        Line::from(vec![
            Span::styled("y", Style::new().fg(Color::Green).bold()),
            Span::raw(" yes   "),
            Span::styled("any other key", Style::new().fg(Color::Red).bold()),
            Span::raw(" cancels"),
        ])
    } else {
        Line::styled("press any key", Style::new().dim())
    });
    let height = lines.len() as u16 + 4;
    let area = popup(f, 72, height);
    let title = if force { " force kill " } else { " kill " };
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: true }).block(
            Block::bordered()
                .title(title)
                .border_style(Style::new().fg(Color::Red)),
        ),
        area,
    );
}

fn draw_help(f: &mut Frame) {
    let rows = [
        ("↑/k ↓/j", "move selection (g/G: first/last)"),
        (
            "x d Del",
            "stop the selected process (SIGTERM, then SIGKILL after 3s)",
        ),
        ("X D", "force kill immediately (SIGKILL)"),
        ("o Enter", "open http://localhost:<port> in your browser"),
        ("/", "filter by port, process, framework or project"),
        ("a", "toggle showing system services and apps"),
        ("r", "refresh now (also refreshes every 2s)"),
        ("q Esc", "quit"),
    ];
    let lines: Vec<Line> = rows
        .iter()
        .map(|(k, v)| {
            Line::from(vec![
                Span::styled(format!(" {k:<9}"), Style::new().fg(ACCENT).bold()),
                Span::raw(*v),
            ])
        })
        .collect();
    let area = popup(f, 70, lines.len() as u16 + 2);
    f.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" keys ")),
        area,
    );
}
