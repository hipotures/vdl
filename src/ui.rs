//! Ratatui + Crossterm terminal UI with mouse support and clean terminal restoration.

use std::io::{self, Stdout};
use std::process::Command;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table, TableState};

use crate::config::Config;
use crate::domain::format_age;
use crate::repository::{SourceRepository, normalize_url};
use crate::state::{AppSnapshot, apply_action, snapshot};

fn terminal_text(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Url,
    Table,
    Confirm,
}

#[derive(Debug, Clone, Copy)]
struct ButtonHit {
    rect: Rect,
    action: ButtonAction,
}

#[derive(Debug, Clone, Copy)]
enum ButtonAction {
    Add,
    ClearUrl,
    Refresh,
    DownloadNow,
    Disable,
    Detach,
    Quit,
    ConfirmDisable,
    CancelDisable,
}

struct TuiState {
    repository: SourceRepository,
    snapshot: AppSnapshot,
    url_input: String,
    selected: Option<String>,
    confirm_id: Option<String>,
    message: String,
    mouse_enabled: bool,
    focus: Focus,
    should_quit: bool,
    table_state: TableState,
    table_area: Rect,
    url_area: Rect,
    buttons: Vec<ButtonHit>,
}

impl TuiState {
    fn new(repository: SourceRepository, mouse_override: Option<bool>) -> Self {
        let mouse_enabled = mouse_override
            .unwrap_or_else(|| std::env::var("VDL_MOUSE").map(|v| v != "0").unwrap_or(true));
        let mut state = Self {
            repository,
            snapshot: AppSnapshot {
                sources: Vec::new(),
                download: None,
                owner_running: false,
                pending: 0,
                time: 0.0,
            },
            url_input: String::new(),
            selected: None,
            confirm_id: None,
            message: "Add sources at any time. F2 toggles mouse capture for text selection."
                .to_string(),
            mouse_enabled,
            focus: Focus::Url,
            should_quit: false,
            table_state: TableState::default(),
            table_area: Rect::default(),
            url_area: Rect::default(),
            buttons: Vec::new(),
        };
        state.refresh();
        state
    }

    fn refresh(&mut self) {
        match snapshot(&self.repository) {
            Ok(snap) => {
                self.snapshot = snap;
                let still_exists = self
                    .selected
                    .as_deref()
                    .is_some_and(|id| self.snapshot.sources.iter().any(|s| s.id == id));
                if !still_exists {
                    self.selected = self.snapshot.sources.first().map(|s| s.id.clone());
                }
                self.sync_table_state();
            }
            Err(err) => {
                self.message = format!("Refresh failed: {err}");
            }
        }
    }

    fn sync_table_state(&mut self) {
        let idx = self
            .selected
            .as_deref()
            .and_then(|id| self.snapshot.sources.iter().position(|s| s.id == id));
        self.table_state.select(idx);
    }

    fn status_line(&self) -> String {
        let status = if let Some(ref busy) = self.snapshot.download {
            format!("Downloading {busy}")
        } else if self.snapshot.owner_running {
            "Ready".to_string()
        } else {
            "Downloader stopped".to_string()
        };
        terminal_text(&format!(
            " vdl  |  {status}  |  {} pending",
            self.snapshot.pending
        ))
    }

    fn preview_line(&self) -> String {
        if self.url_input.is_empty() {
            return "Paste a profile/channel URL. Query parameters will be removed.".to_string();
        }
        match normalize_url(&self.url_input) {
            Ok(clean) => terminal_text(&format!("Will save: {clean}")),
            Err(err) => terminal_text(&err.to_string()),
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let len = self.snapshot.sources.len();
        if len == 0 {
            self.selected = None;
            self.table_state.select(None);
            return;
        }
        let current = self
            .selected
            .as_deref()
            .and_then(|id| self.snapshot.sources.iter().position(|s| s.id == id))
            .unwrap_or(0);
        let next = (current as isize + delta).clamp(0, (len - 1) as isize) as usize;
        self.selected = Some(self.snapshot.sources[next].id.clone());
        self.table_state.select(Some(next));
    }

    fn submit_url(&mut self) {
        let raw = self.url_input.clone();
        match self.repository.add_source(&raw) {
            Ok(result) => {
                let verb = if result.created {
                    "Added"
                } else {
                    "Already exists"
                };
                let suffix = if result.source.active {
                    ""
                } else {
                    " (inactive)"
                };
                self.message = format!("{verb}: {}{suffix}", result.source.id());
                self.url_input.clear();
                self.refresh();
            }
            Err(err) => {
                self.message = err.to_string();
            }
        }
    }

    fn trigger_download(&mut self) {
        let Some(id) = self.selected.clone() else {
            return;
        };
        match apply_action(&self.repository, "download-now", &id) {
            Ok(()) => {
                self.message = format!("Download requested: {id}");
                self.refresh();
            }
            Err(err) => {
                self.message = err.to_string();
            }
        }
    }

    fn ask_disable(&mut self) {
        if let Some(ref id) = self.selected
            && self
                .snapshot
                .sources
                .iter()
                .any(|s| s.id == *id && s.active)
        {
            self.confirm_id = Some(id.clone());
            self.focus = Focus::Confirm;
        }
    }

    fn cancel_disable(&mut self) {
        self.confirm_id = None;
        self.focus = Focus::Table;
    }

    fn confirm_disable(&mut self) {
        let Some(id) = self.confirm_id.take() else {
            return;
        };
        self.focus = Focus::Table;
        match apply_action(&self.repository, "disable", &id) {
            Ok(()) => {
                self.message = format!("Disabled: {id}");
                self.refresh();
            }
            Err(err) => {
                self.message = err.to_string();
            }
        }
    }

    fn detach(&mut self) {
        match Command::new("tmux")
            .args(["-L", "vdl", "detach-client", "-s", "main"])
            .status()
        {
            Ok(status) if status.success() => {}
            Ok(status) => {
                self.message = format!("Could not detach: tmux exited with {status}");
            }
            Err(err) => {
                self.message = format!("Could not detach: {err}");
            }
        }
    }

    fn handle_button(&mut self, action: ButtonAction) {
        match action {
            ButtonAction::Add => self.submit_url(),
            ButtonAction::ClearUrl => {
                self.url_input.clear();
                self.focus = Focus::Url;
            }
            ButtonAction::Refresh => self.refresh(),
            ButtonAction::DownloadNow => self.trigger_download(),
            ButtonAction::Disable => self.ask_disable(),
            ButtonAction::Detach => self.detach(),
            ButtonAction::Quit => self.should_quit = true,
            ButtonAction::ConfirmDisable => self.confirm_disable(),
            ButtonAction::CancelDisable => self.cancel_disable(),
        }
    }

    fn handle_key(&mut self, key: KeyEvent, terminal: &mut Terminal<CrosstermBackend<Stdout>>) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        // Global shortcuts
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('q') | KeyCode::Char('c'))
        {
            self.should_quit = true;
            return;
        }
        match key.code {
            KeyCode::F(2) => {
                self.mouse_enabled = !self.mouse_enabled;
                if self.mouse_enabled {
                    let _ = execute!(terminal.backend_mut(), EnableMouseCapture);
                } else {
                    let _ = execute!(terminal.backend_mut(), DisableMouseCapture);
                }
                return;
            }
            KeyCode::F(6) => {
                self.detach();
                return;
            }
            KeyCode::Esc => {
                if self.confirm_id.is_some() {
                    self.cancel_disable();
                } else {
                    self.focus = Focus::Table;
                }
                return;
            }
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Url => Focus::Table,
                    Focus::Table => {
                        if self.confirm_id.is_some() {
                            Focus::Confirm
                        } else {
                            Focus::Url
                        }
                    }
                    Focus::Confirm => Focus::Url,
                };
                return;
            }
            KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::Url => {
                        if self.confirm_id.is_some() {
                            Focus::Confirm
                        } else {
                            Focus::Table
                        }
                    }
                    Focus::Table => Focus::Url,
                    Focus::Confirm => Focus::Table,
                };
                return;
            }
            _ => {}
        }

        match self.focus {
            Focus::Url => match key.code {
                KeyCode::Enter => self.submit_url(),
                KeyCode::Backspace => {
                    self.url_input.pop();
                }
                KeyCode::Char(c)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !key.modifiers.contains(KeyModifiers::ALT) =>
                {
                    self.url_input.push(c);
                }
                _ => {}
            },
            Focus::Confirm => match key.code {
                KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => self.confirm_disable(),
                KeyCode::Char('n') | KeyCode::Char('N') => self.cancel_disable(),
                _ => {}
            },
            Focus::Table => match key.code {
                KeyCode::Up => self.move_selection(-1),
                KeyCode::Down => self.move_selection(1),
                KeyCode::PageUp => self.move_selection(-10),
                KeyCode::PageDown => self.move_selection(10),
                KeyCode::Char('q') => self.should_quit = true,
                KeyCode::Char('a') => self.focus = Focus::Url,
                KeyCode::Char('n') => self.trigger_download(),
                KeyCode::Char('d') => self.ask_disable(),
                KeyCode::Char('r') => self.refresh(),
                _ => {}
            },
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollUp => self.move_selection(-3),
            MouseEventKind::ScrollDown => self.move_selection(3),
            MouseEventKind::Up(MouseButton::Left) => {
                let col = mouse.column;
                let row = mouse.row;
                for btn in &self.buttons.clone() {
                    if col >= btn.rect.x
                        && col < btn.rect.x + btn.rect.width
                        && row >= btn.rect.y
                        && row < btn.rect.y + btn.rect.height
                    {
                        self.handle_button(btn.action);
                        return;
                    }
                }
                if col >= self.url_area.x
                    && col < self.url_area.x + self.url_area.width
                    && row >= self.url_area.y
                    && row < self.url_area.y + self.url_area.height
                {
                    self.focus = Focus::Url;
                    return;
                }
                if col >= self.table_area.x
                    && col < self.table_area.x + self.table_area.width
                    && row > self.table_area.y
                    && row < self.table_area.y + self.table_area.height
                {
                    // Header occupies first row of table_area.
                    let offset = self.table_state.offset();
                    let clicked = offset + (row - self.table_area.y - 1) as usize;
                    if let Some(src) = self.snapshot.sources.get(clicked) {
                        self.selected = Some(src.id.clone());
                        self.table_state.select(Some(clicked));
                        self.focus = Focus::Table;
                    }
                }
            }
            _ => {}
        }
    }
}

fn draw_ui(frame: &mut ratatui::Frame<'_>, state: &mut TuiState) {
    state.buttons.clear();
    let area = frame.area();
    let has_confirm = state.confirm_id.is_some();

    let constraints = if has_confirm {
        vec![
            Constraint::Length(1), // status
            Constraint::Length(1), // url input
            Constraint::Length(1), // preview
            Constraint::Length(1), // button row 1
            Constraint::Length(1), // button row 2
            Constraint::Length(2), // confirm
            Constraint::Min(3),    // table
            Constraint::Length(1), // message
            Constraint::Length(1), // footer
        ]
    } else {
        vec![
            Constraint::Length(1), // status
            Constraint::Length(1), // url input
            Constraint::Length(1), // preview
            Constraint::Length(1), // button row 1
            Constraint::Length(1), // button row 2
            Constraint::Min(3),    // table
            Constraint::Length(1), // message
            Constraint::Length(1), // footer
        ]
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    let title_style = Style::default().add_modifier(Modifier::BOLD);
    let muted_style = Style::default().fg(Color::Rgb(0x88, 0x99, 0xaa));
    let msg_style = Style::default().fg(Color::Rgb(0x66, 0xbb, 0xee));

    frame.render_widget(
        Paragraph::new(state.status_line()).style(title_style),
        chunks[0],
    );

    state.url_area = chunks[1];
    let url_style = if state.focus == Focus::Url {
        Style::default().add_modifier(Modifier::UNDERLINED)
    } else {
        Style::default()
    };
    let cursor_mark = if state.focus == Focus::Url { "_" } else { "" };
    frame.render_widget(
        Paragraph::new(format!(
            "URL: {}{cursor_mark}",
            terminal_text(&state.url_input)
        ))
        .style(url_style),
        chunks[1],
    );

    frame.render_widget(
        Paragraph::new(state.preview_line()).style(muted_style),
        chunks[2],
    );

    render_button_row(
        frame,
        chunks[3],
        &[
            ("[ Add ]", ButtonAction::Add),
            ("[ Clear URL ]", ButtonAction::ClearUrl),
            ("[ Refresh ]", ButtonAction::Refresh),
        ],
        &mut state.buttons,
    );

    render_button_row(
        frame,
        chunks[4],
        &[
            ("[ Download now ]", ButtonAction::DownloadNow),
            ("[ Disable ]", ButtonAction::Disable),
            ("[ Detach ]", ButtonAction::Detach),
            ("[ Quit ]", ButtonAction::Quit),
        ],
        &mut state.buttons,
    );

    let (table_idx, msg_idx, footer_idx) = if has_confirm {
        let confirm_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Length(1)])
            .split(chunks[5]);
        let cid = state.confirm_id.as_deref().unwrap_or("");
        frame.render_widget(
            Paragraph::new(terminal_text(&format!(
                "Disable {cid}? A running download will finish; files are kept."
            )))
            .style(Style::default().fg(Color::Yellow)),
            confirm_chunks[0],
        );
        render_button_row(
            frame,
            confirm_chunks[1],
            &[
                ("[ Confirm (y) ]", ButtonAction::ConfirmDisable),
                ("[ Cancel (n) ]", ButtonAction::CancelDisable),
            ],
            &mut state.buttons,
        );
        (6, 7, 8)
    } else {
        (5, 6, 7)
    };

    state.table_area = chunks[table_idx];
    let wide = chunks[table_idx].width >= 72;
    if state.snapshot.sources.is_empty() {
        frame.render_widget(
            Paragraph::new("No sources. Paste a URL above.").style(muted_style),
            chunks[table_idx],
        );
    } else {
        let header_cells: Vec<&str> = if wide {
            vec!["#", "Source", "Last attempt", "State"]
        } else {
            vec!["#", "Source", "State"]
        };
        let header = Row::new(header_cells).style(Style::default().add_modifier(Modifier::BOLD));

        let rows: Vec<Row<'_>> = state
            .snapshot
            .sources
            .iter()
            .enumerate()
            .map(|(idx, row)| {
                let source_label = terminal_text(&format!("{} / {}", row.service, row.account));
                let mut cells = vec![Cell::from((idx + 1).to_string()), Cell::from(source_label)];
                if wide {
                    cells.push(Cell::from(format_age(row.last_check, None)));
                }
                cells.push(Cell::from(row.state.clone()));
                Row::new(cells)
            })
            .collect();

        let widths: Vec<Constraint> = if wide {
            vec![
                Constraint::Length(4),
                Constraint::Min(10),
                Constraint::Length(14),
                Constraint::Length(12),
            ]
        } else {
            vec![
                Constraint::Length(4),
                Constraint::Min(10),
                Constraint::Length(12),
            ]
        };

        let table = Table::new(rows, widths).header(header).row_highlight_style(
            Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED),
        );

        frame.render_stateful_widget(table, chunks[table_idx], &mut state.table_state);
    }

    frame.render_widget(
        Paragraph::new(terminal_text(&state.message)).style(msg_style),
        chunks[msg_idx],
    );
    frame.render_widget(
        Paragraph::new(format!(
            "Tab: focus  Arrows: select  Ctrl-Q: quit  F6: detach  F2: mouse {}",
            if state.mouse_enabled { "ON" } else { "OFF" }
        ))
        .style(muted_style),
        chunks[footer_idx],
    );
}

fn render_button_row(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    items: &[(&str, ButtonAction)],
    hits: &mut Vec<ButtonHit>,
) {
    let mut x = area.x;
    let mut spans = Vec::new();
    for (i, &(label, action)) in items.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
            x = x.saturating_add(1);
        }
        let w = label.len() as u16;
        hits.push(ButtonHit {
            rect: Rect {
                x,
                y: area.y,
                width: w,
                height: 1,
            },
            action,
        });
        spans.push(Span::styled(label, Style::default().fg(Color::Cyan)));
        x = x.saturating_add(w);
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

pub fn run_tui(config: Config, mouse_override: Option<bool>) -> Result<(), String> {
    let repository = SourceRepository::new(&config.download_root).map_err(|e| e.to_string())?;
    let mut state = TuiState::new(repository, mouse_override);

    enable_raw_mode().map_err(|e| e.to_string())?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).map_err(|e| e.to_string())?;
    if state.mouse_enabled {
        let _ = execute!(stdout, EnableMouseCapture);
    }

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).map_err(|e| e.to_string())?;
    let mut last_refresh = Instant::now();

    let run_res = loop {
        if let Err(err) = terminal.draw(|frame| draw_ui(frame, &mut state)) {
            break Err(err.to_string());
        }
        if state.should_quit {
            break Ok(());
        }
        if last_refresh.elapsed() >= Duration::from_secs(1) {
            state.refresh();
            last_refresh = Instant::now();
        }
        let poll_wait = Duration::from_millis(100);
        match event::poll(poll_wait) {
            Ok(true) => match event::read() {
                Ok(Event::Key(key)) => state.handle_key(key, &mut terminal),
                Ok(Event::Mouse(mouse)) => state.handle_mouse(mouse),
                Ok(Event::Paste(text)) => {
                    if state.focus == Focus::Url {
                        state.url_input.push_str(&text);
                    }
                }
                Ok(_) => {}
                Err(err) => break Err(err.to_string()),
            },
            Ok(false) => {}
            Err(err) => break Err(err.to_string()),
        }
    };

    let _ = disable_raw_mode();
    let _ = execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    );
    let _ = terminal.show_cursor();
    run_res
}
