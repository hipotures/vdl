//! Ratatui + Crossterm terminal UI with btop/homestack-style border-integrated
//! hotkeys, mouse support, live filtering, sorting, optional log tail panel,
//! and clean terminal restoration.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Stdout};
use std::path::{Path, PathBuf};
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
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Cell, Clear, Paragraph, Row, Scrollbar, ScrollbarOrientation,
    ScrollbarState, Table, TableState, Wrap,
};

use crate::config::Config;
use crate::domain::format_age;
use crate::repository::{SourceRepository, normalize_url};
use crate::state::{AppSnapshot, SourceSnapshot, apply_action, snapshot};

const BORDER_FOCUSED: Color = Color::Rgb(0x7a, 0xa2, 0xf7);
const BORDER_UNFOCUSED: Color = Color::Rgb(0x41, 0x48, 0x68);
const HOTKEY_COLOR: Color = Color::Rgb(0xe0, 0x6c, 0x75);
const TEXT_PRIMARY: Color = Color::Rgb(0xc0, 0xca, 0xf5);
const MUTED: Color = Color::Rgb(0x6c, 0x76, 0x93);
const MSG_COLOR: Color = Color::Rgb(0x7d, 0xcf, 0xff);
const ROW_HIGHLIGHT_BG: Color = Color::Rgb(0x24, 0x2b, 0x3d);
const LOG_TAIL_LINES: usize = 8;

fn terminal_text(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn state_style(state: &str) -> Style {
    match state {
        "downloading" => Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
        "queued" => Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
        "finishing" => Style::default().fg(Color::Magenta),
        "active" => Style::default().fg(Color::Rgb(0x73, 0xda, 0xca)),
        _ => Style::default().fg(MUTED),
    }
}

fn service_style(service: &str) -> Style {
    match service.to_ascii_lowercase().as_str() {
        "tiktok" => Style::default().fg(Color::Rgb(0xbb, 0x9a, 0xf7)),
        "youtube" => Style::default().fg(HOTKEY_COLOR),
        "instagram" => Style::default().fg(Color::Rgb(0xe0, 0xaf, 0x68)),
        _ => Style::default().fg(MSG_COLOR),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Table,
    Url,
    Filter,
    Confirm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortMode {
    NameAsc,
    NewestFirst,
    OldestFirst,
}

impl SortMode {
    fn next(self) -> Self {
        match self {
            SortMode::NameAsc => SortMode::NewestFirst,
            SortMode::NewestFirst => SortMode::OldestFirst,
            SortMode::OldestFirst => SortMode::NameAsc,
        }
    }

    fn label(self) -> &'static str {
        match self {
            SortMode::NameAsc => "A-Z",
            SortMode::NewestFirst => "Newest",
            SortMode::OldestFirst => "Oldest",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ButtonHit {
    rect: Rect,
    action: ButtonAction,
}

#[derive(Debug, Clone, Copy)]
enum ButtonAction {
    FocusUrl,
    ClearUrl,
    DownloadNow,
    Disable,
    CycleSort,
    FocusFilter,
    ToggleLog,
    ToggleMouse,
    Refresh,
    Detach,
    Quit,
    ConfirmDisable,
    CancelDisable,
}

/// Helper for building btop/homestack-style border titles with `┐`...`┌` (top)
/// or `┘`...`└` (bottom) pills and clickable hit rects.
struct BorderPills {
    spans: Vec<Span<'static>>,
    x: u16,
    y: u16,
    border_style: Style,
    top: bool,
}

impl BorderPills {
    fn new(area_x: u16, y: u16, border_color: Color, top: bool) -> Self {
        Self {
            spans: Vec::new(),
            x: area_x.saturating_add(1), // after left corner
            y,
            border_style: Style::default().fg(border_color),
            top,
        }
    }

    fn open_cap(&self) -> &'static str {
        if self.top { "┐" } else { "┘" }
    }

    fn close_cap(&self) -> &'static str {
        if self.top { "┌" } else { "└" }
    }

    fn push_pill(
        &mut self,
        content: Vec<Span<'static>>,
        action: Option<ButtonAction>,
        hits: &mut Vec<ButtonHit>,
    ) {
        let open = self.open_cap();
        let close = self.close_cap();
        let content_width: usize = content.iter().map(|s| s.content.chars().count()).sum();
        self.spans.push(Span::styled(open, self.border_style));
        let pill_x = self.x.saturating_add(1);
        for span in content {
            self.spans.push(span);
        }
        self.spans.push(Span::styled(close, self.border_style));
        if let Some(act) = action {
            hits.push(ButtonHit {
                rect: Rect {
                    x: pill_x,
                    y: self.y,
                    width: content_width as u16,
                    height: 1,
                },
                action: act,
            });
        }
        self.x = self.x.saturating_add((content_width + 2) as u16);
    }

    /// Pill where the hotkey is the prefix of the label (e.g., `f` + `ilter`).
    fn push_hotkey_word(
        &mut self,
        key: &'static str,
        rest: &'static str,
        active: bool,
        action: ButtonAction,
        hits: &mut Vec<ButtonHit>,
    ) {
        let text_style = if active {
            Style::default()
                .fg(BORDER_FOCUSED)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(TEXT_PRIMARY)
        };
        self.push_pill(
            vec![
                Span::styled(
                    key,
                    Style::default()
                        .fg(HOTKEY_COLOR)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(rest, text_style),
            ],
            Some(action),
            hits,
        );
    }

    /// Pill where the hotkey is separate from the label (e.g., `F5` + ` refresh`).
    fn push_key_label(
        &mut self,
        key: &'static str,
        label: String,
        action: Option<ButtonAction>,
        hits: &mut Vec<ButtonHit>,
    ) {
        self.push_pill(
            vec![
                Span::styled(
                    key,
                    Style::default()
                        .fg(HOTKEY_COLOR)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!(" {label}"), Style::default().fg(TEXT_PRIMARY)),
            ],
            action,
            hits,
        );
    }

    fn finish(self) -> Line<'static> {
        Line::from(self.spans)
    }
}

fn right_pill(content: Vec<Span<'static>>, border_color: Color, top: bool) -> Line<'static> {
    let border_style = Style::default().fg(border_color);
    let (open, close) = if top { ("┐", "┌") } else { ("┘", "└") };
    let mut spans = Vec::with_capacity(content.len() + 2);
    spans.push(Span::styled(open, border_style));
    spans.extend(content);
    spans.push(Span::styled(close, border_style));
    Line::from(spans)
}

/// UTF-8 safe single-line text input with cursor movement.
#[derive(Debug, Default, Clone)]
struct TextInput {
    text: String,
    cursor: usize,
}

impl TextInput {
    fn as_str(&self) -> &str {
        &self.text
    }

    fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    fn byte_index(&self) -> usize {
        self.text
            .char_indices()
            .nth(self.cursor)
            .map_or(self.text.len(), |(idx, _)| idx)
    }

    fn char_len(&self) -> usize {
        self.text.chars().count()
    }

    fn insert_char(&mut self, c: char) {
        let idx = self.byte_index();
        self.text.insert(idx, c);
        self.cursor += 1;
    }

    fn insert_str(&mut self, s: &str) {
        let clean: String = s.chars().filter(|c| !c.is_control()).collect();
        let idx = self.byte_index();
        self.text.insert_str(idx, &clean);
        self.cursor += clean.chars().count();
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor -= 1;
        let idx = self.byte_index();
        self.text.remove(idx);
    }

    fn delete(&mut self) {
        if self.cursor >= self.char_len() {
            return;
        }
        let idx = self.byte_index();
        self.text.remove(idx);
    }

    fn delete_word(&mut self) {
        while self.cursor > 0 {
            let prev = self.text.chars().nth(self.cursor - 1);
            if prev.is_some_and(|c| c.is_whitespace() || c == '/') {
                self.backspace();
            } else {
                break;
            }
        }
        while self.cursor > 0 {
            let prev = self.text.chars().nth(self.cursor - 1);
            if prev.is_some_and(|c| !c.is_whitespace() && c != '/') {
                self.backspace();
            } else {
                break;
            }
        }
    }

    fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn move_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.char_len());
    }

    fn move_home(&mut self) {
        self.cursor = 0;
    }

    fn move_end(&mut self) {
        self.cursor = self.char_len();
    }

    fn render_with_cursor(&self, show_cursor: bool) -> Line<'static> {
        if !show_cursor {
            return Line::from(Span::styled(
                terminal_text(&self.text),
                Style::default().fg(TEXT_PRIMARY),
            ));
        }
        let idx = self.byte_index();
        let before = terminal_text(&self.text[..idx]);
        let rest = &self.text[idx..];
        let mut chars = rest.chars();
        let under = chars.next().map_or_else(
            || " ".to_string(),
            |c| {
                if c.is_control() {
                    " ".to_string()
                } else {
                    c.to_string()
                }
            },
        );
        let after = terminal_text(chars.as_str());
        Line::from(vec![
            Span::styled(before, Style::default().fg(TEXT_PRIMARY)),
            Span::styled(
                under,
                Style::default()
                    .bg(BORDER_FOCUSED)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(after, Style::default().fg(TEXT_PRIMARY)),
        ])
    }
}

fn read_log_tail(path: &Path, max_lines: usize) -> Vec<String> {
    let Ok(mut file) = File::open(path) else {
        return vec!["(no log file yet)".to_string()];
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let read_size = len.min(16 * 1024);
    if len > read_size && file.seek(SeekFrom::End(-(read_size as i64))).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::with_capacity(read_size as usize);
    if file.read_to_end(&mut buf).is_err() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<String> = text
        .lines()
        .map(terminal_text)
        .filter(|l| !l.trim().is_empty())
        .collect();
    if lines.len() > max_lines {
        lines = lines.split_off(lines.len() - max_lines);
    }
    if lines.is_empty() {
        vec!["(log is empty)".to_string()]
    } else {
        lines
    }
}

fn filter_and_sort_sources(
    sources: &[SourceSnapshot],
    filter: &str,
    sort_mode: SortMode,
) -> Vec<SourceSnapshot> {
    let needle = filter.trim().to_lowercase();
    let mut out: Vec<SourceSnapshot> = if needle.is_empty() {
        sources.to_vec()
    } else {
        sources
            .iter()
            .filter(|s| {
                s.account.to_lowercase().contains(&needle)
                    || s.service.to_lowercase().contains(&needle)
                    || s.state.to_lowercase().contains(&needle)
                    || s.id.to_lowercase().contains(&needle)
            })
            .cloned()
            .collect()
    };

    match sort_mode {
        SortMode::NameAsc => {
            out.sort_by(|a, b| {
                (
                    a.service.to_lowercase(),
                    a.account.to_lowercase(),
                    a.id.to_lowercase(),
                )
                    .cmp(&(
                        b.service.to_lowercase(),
                        b.account.to_lowercase(),
                        b.id.to_lowercase(),
                    ))
            });
        }
        SortMode::NewestFirst => {
            out.sort_by(|a, b| {
                let ta = a.last_check.unwrap_or(f64::NEG_INFINITY);
                let tb = b.last_check.unwrap_or(f64::NEG_INFINITY);
                tb.total_cmp(&ta)
                    .then_with(|| a.account.to_lowercase().cmp(&b.account.to_lowercase()))
            });
        }
        SortMode::OldestFirst => {
            out.sort_by(|a, b| {
                let ta = a.last_check.unwrap_or(f64::NEG_INFINITY);
                let tb = b.last_check.unwrap_or(f64::NEG_INFINITY);
                ta.total_cmp(&tb)
                    .then_with(|| a.account.to_lowercase().cmp(&b.account.to_lowercase()))
            });
        }
    }
    out
}

struct TuiState {
    repository: SourceRepository,
    log_file: PathBuf,
    snapshot: AppSnapshot,
    visible: Vec<SourceSnapshot>,
    url_input: TextInput,
    filter_input: TextInput,
    sort_mode: SortMode,
    show_log: bool,
    log_lines: Vec<String>,
    selected: Option<String>,
    confirm_id: Option<String>,
    message: Option<String>,
    message_is_error: bool,
    mouse_enabled: bool,
    focus: Focus,
    should_quit: bool,
    table_state: TableState,
    table_area: Rect,
    input_area: Rect,
    buttons: Vec<ButtonHit>,
}

impl TuiState {
    fn new(repository: SourceRepository, log_file: PathBuf, mouse_override: Option<bool>) -> Self {
        let mouse_enabled = mouse_override
            .unwrap_or_else(|| std::env::var("VDL_MOUSE").map(|v| v != "0").unwrap_or(true));
        let mut state = Self {
            repository,
            log_file,
            snapshot: AppSnapshot {
                sources: Vec::new(),
                download: None,
                owner_running: false,
                pending: 0,
                time: 0.0,
            },
            visible: Vec::new(),
            url_input: TextInput::default(),
            filter_input: TextInput::default(),
            sort_mode: SortMode::NameAsc,
            show_log: false,
            log_lines: Vec::new(),
            selected: None,
            confirm_id: None,
            message: None,
            message_is_error: false,
            mouse_enabled,
            focus: Focus::Table,
            should_quit: false,
            table_state: TableState::default(),
            table_area: Rect::default(),
            input_area: Rect::default(),
            buttons: Vec::new(),
        };
        state.refresh();
        state
    }

    fn set_info(&mut self, msg: impl Into<String>) {
        self.message = Some(msg.into());
        self.message_is_error = false;
    }

    fn set_error(&mut self, msg: impl Into<String>) {
        self.message = Some(msg.into());
        self.message_is_error = true;
    }

    fn rebuild_visible(&mut self) {
        self.visible = filter_and_sort_sources(
            &self.snapshot.sources,
            self.filter_input.as_str(),
            self.sort_mode,
        );
        let still_exists = self
            .selected
            .as_deref()
            .is_some_and(|id| self.visible.iter().any(|s| s.id == id));
        if !still_exists {
            self.selected = self.visible.first().map(|s| s.id.clone());
        }
        self.sync_table_state();
    }

    fn refresh(&mut self) {
        match snapshot(&self.repository) {
            Ok(snap) => {
                self.snapshot = snap;
                self.rebuild_visible();
            }
            Err(err) => {
                self.set_error(format!("Refresh failed: {err}"));
            }
        }
        if self.show_log {
            self.log_lines = read_log_tail(&self.log_file, LOG_TAIL_LINES);
        }
    }

    fn sync_table_state(&mut self) {
        let idx = self
            .selected
            .as_deref()
            .and_then(|id| self.visible.iter().position(|s| s.id == id));
        self.table_state.select(idx);
    }

    fn selected_index(&self) -> Option<usize> {
        self.selected
            .as_deref()
            .and_then(|id| self.visible.iter().position(|s| s.id == id))
    }

    fn move_selection(&mut self, delta: isize) {
        let len = self.visible.len();
        if len == 0 {
            self.selected = None;
            self.table_state.select(None);
            return;
        }
        let current = self.selected_index().unwrap_or(0);
        let next = (current as isize + delta).clamp(0, (len - 1) as isize) as usize;
        self.selected = Some(self.visible[next].id.clone());
        self.table_state.select(Some(next));
    }

    fn select_first(&mut self) {
        if let Some(first) = self.visible.first() {
            self.selected = Some(first.id.clone());
            self.table_state.select(Some(0));
        }
    }

    fn select_last(&mut self) {
        if let Some(last) = self.visible.last() {
            let idx = self.visible.len() - 1;
            self.selected = Some(last.id.clone());
            self.table_state.select(Some(idx));
        }
    }

    fn submit_url(&mut self) {
        let raw = self.url_input.as_str().to_string();
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
                let new_id = result.source.id();
                self.set_info(format!("{verb}: {new_id}{suffix}"));
                self.url_input.clear();
                self.selected = Some(new_id);
                self.focus = Focus::Table;
                self.refresh();
            }
            Err(err) => {
                self.set_error(err.to_string());
            }
        }
    }

    fn trigger_download(&mut self) {
        let Some(id) = self.selected.clone() else {
            return;
        };
        match apply_action(&self.repository, "download-now", &id) {
            Ok(()) => {
                self.set_info(format!("Download requested: {id}"));
                self.refresh();
            }
            Err(err) => {
                self.set_error(err.to_string());
            }
        }
    }

    fn ask_disable(&mut self) {
        if let Some(ref id) = self.selected
            && self.visible.iter().any(|s| s.id == *id && s.active)
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
                self.set_info(format!("Disabled: {id}"));
                self.refresh();
            }
            Err(err) => {
                self.set_error(err.to_string());
            }
        }
    }

    fn cycle_sort(&mut self) {
        self.sort_mode = self.sort_mode.next();
        self.rebuild_visible();
        self.set_info(format!("Sort: {}", self.sort_mode.label()));
    }

    fn toggle_log(&mut self) {
        self.show_log = !self.show_log;
        if self.show_log {
            self.log_lines = read_log_tail(&self.log_file, LOG_TAIL_LINES);
        }
    }

    fn toggle_mouse(&mut self, terminal: &mut Terminal<CrosstermBackend<Stdout>>) {
        self.mouse_enabled = !self.mouse_enabled;
        if self.mouse_enabled {
            let _ = execute!(terminal.backend_mut(), EnableMouseCapture);
        } else {
            let _ = execute!(terminal.backend_mut(), DisableMouseCapture);
        }
    }

    fn detach(&mut self) {
        match Command::new("tmux")
            .args(["-L", "vdl", "detach-client", "-s", "main"])
            .status()
        {
            Ok(status) if status.success() => {}
            Ok(status) => {
                self.set_error(format!("Could not detach: tmux exited with {status}"));
            }
            Err(err) => {
                self.set_error(format!("Could not detach: {err}"));
            }
        }
    }

    fn handle_button(
        &mut self,
        action: ButtonAction,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) {
        match action {
            ButtonAction::FocusUrl => {
                if self.focus == Focus::Url && !self.url_input.is_empty() {
                    self.submit_url();
                } else {
                    self.focus = Focus::Url;
                }
            }
            ButtonAction::ClearUrl => {
                self.url_input.clear();
                self.focus = Focus::Url;
            }
            ButtonAction::DownloadNow => self.trigger_download(),
            ButtonAction::Disable => self.ask_disable(),
            ButtonAction::CycleSort => self.cycle_sort(),
            ButtonAction::FocusFilter => self.focus = Focus::Filter,
            ButtonAction::ToggleLog => self.toggle_log(),
            ButtonAction::ToggleMouse => self.toggle_mouse(terminal),
            ButtonAction::Refresh => {
                self.refresh();
                self.set_info("Refreshed.");
            }
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
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('q') | KeyCode::Char('c'))
        {
            self.should_quit = true;
            return;
        }
        match key.code {
            KeyCode::F(2) => {
                self.toggle_mouse(terminal);
                return;
            }
            KeyCode::F(5) => {
                self.refresh();
                self.set_info("Refreshed.");
                return;
            }
            KeyCode::F(6) => {
                self.detach();
                return;
            }
            KeyCode::Tab | KeyCode::BackTab => {
                if self.focus == Focus::Confirm {
                    return;
                }
                self.focus = match self.focus {
                    Focus::Table => Focus::Url,
                    Focus::Url | Focus::Filter => Focus::Table,
                    Focus::Confirm => Focus::Confirm,
                };
                return;
            }
            _ => {}
        }

        match self.focus {
            Focus::Confirm => match key.code {
                KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => self.confirm_disable(),
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => self.cancel_disable(),
                _ => {}
            },
            Focus::Url => match key.code {
                KeyCode::Esc => self.focus = Focus::Table,
                KeyCode::Enter => self.submit_url(),
                KeyCode::Left => self.url_input.move_left(),
                KeyCode::Right => self.url_input.move_right(),
                KeyCode::Home => self.url_input.move_home(),
                KeyCode::End => self.url_input.move_end(),
                KeyCode::Backspace => self.url_input.backspace(),
                KeyCode::Delete => self.url_input.delete(),
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.url_input.clear();
                }
                KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.url_input.delete_word();
                }
                KeyCode::Char(c)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !key.modifiers.contains(KeyModifiers::ALT) =>
                {
                    self.url_input.insert_char(c);
                }
                _ => {}
            },
            Focus::Filter => match key.code {
                KeyCode::Esc => {
                    self.filter_input.clear();
                    self.rebuild_visible();
                    self.focus = Focus::Table;
                }
                KeyCode::Enter => {
                    self.focus = Focus::Table;
                }
                KeyCode::Left => self.filter_input.move_left(),
                KeyCode::Right => self.filter_input.move_right(),
                KeyCode::Home => self.filter_input.move_home(),
                KeyCode::End => self.filter_input.move_end(),
                KeyCode::Backspace => {
                    self.filter_input.backspace();
                    self.rebuild_visible();
                }
                KeyCode::Delete => {
                    self.filter_input.delete();
                    self.rebuild_visible();
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.filter_input.clear();
                    self.rebuild_visible();
                }
                KeyCode::Char(c)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !key.modifiers.contains(KeyModifiers::ALT) =>
                {
                    self.filter_input.insert_char(c);
                    self.rebuild_visible();
                }
                _ => {}
            },
            Focus::Table => match key.code {
                KeyCode::Char('1') => self.focus = Focus::Url,
                KeyCode::Char('2') => self.focus = Focus::Table,
                KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
                KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
                KeyCode::PageUp => self.move_selection(-10),
                KeyCode::PageDown => self.move_selection(10),
                KeyCode::Home | KeyCode::Char('g') => self.select_first(),
                KeyCode::End | KeyCode::Char('G') => self.select_last(),
                KeyCode::Char('q') => self.should_quit = true,
                KeyCode::Char('a') => self.focus = Focus::Url,
                KeyCode::Char('/') | KeyCode::Char('f') => self.focus = Focus::Filter,
                KeyCode::Char('s') => self.cycle_sort(),
                KeyCode::Char('l') => self.toggle_log(),
                KeyCode::Char('n') => self.trigger_download(),
                KeyCode::Char('d') => self.ask_disable(),
                KeyCode::Char('r') => {
                    self.refresh();
                    self.set_info("Refreshed.");
                }
                KeyCode::Esc if !self.filter_input.is_empty() => {
                    self.filter_input.clear();
                    self.rebuild_visible();
                    self.set_info("Filter cleared.");
                }
                _ => {}
            },
        }
    }

    fn handle_mouse(
        &mut self,
        mouse: MouseEvent,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) {
        match mouse.kind {
            MouseEventKind::ScrollUp => self.move_selection(-3),
            MouseEventKind::ScrollDown => self.move_selection(3),
            MouseEventKind::Up(MouseButton::Left) => {
                let col = mouse.column;
                let row = mouse.row;
                for btn in &self.buttons.clone() {
                    if rect_contains(btn.rect, col, row) {
                        self.handle_button(btn.action, terminal);
                        return;
                    }
                }
                if self.confirm_id.is_some() {
                    return;
                }
                if rect_contains(self.input_area, col, row) {
                    if self.focus != Focus::Filter {
                        self.focus = Focus::Url;
                    }
                    return;
                }
                if rect_contains(self.table_area, col, row) {
                    let first_row_y = self.table_area.y.saturating_add(2);
                    let bottom_y = self.table_area.y + self.table_area.height.saturating_sub(1);
                    if row >= first_row_y && row < bottom_y {
                        let offset = self.table_state.offset();
                        let clicked = offset + (row - first_row_y) as usize;
                        if let Some(src) = self.visible.get(clicked) {
                            self.selected = Some(src.id.clone());
                            self.table_state.select(Some(clicked));
                            self.focus = Focus::Table;
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

fn rect_contains(rect: Rect, col: u16, row: u16) -> bool {
    col >= rect.x
        && col < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}

fn centered_rect(percent_x: u16, height: u16, area: Rect) -> Rect {
    let width = (area.width * percent_x / 100).clamp(36, area.width);
    let h = height.min(area.height);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    Rect {
        x,
        y,
        width,
        height: h,
    }
}

fn draw_ui(frame: &mut ratatui::Frame<'_>, state: &mut TuiState) {
    state.buttons.clear();
    let area = frame.area();

    let mut constraints = vec![
        Constraint::Length(3), // ¹vdl / URL or Filter input panel
        Constraint::Min(5),    // ²sources panel
    ];
    if state.show_log {
        constraints.push(Constraint::Length((LOG_TAIL_LINES + 2) as u16));
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    render_top_panel(frame, chunks[0], state);
    render_sources_panel(frame, chunks[1], state);
    if state.show_log {
        render_log_panel(frame, chunks[2], state);
    }
    if state.confirm_id.is_some() {
        render_confirm_modal(frame, area, state);
    }
}

fn render_top_panel(frame: &mut ratatui::Frame<'_>, area: Rect, state: &mut TuiState) {
    state.input_area = area;
    let is_filter = state.focus == Focus::Filter;
    let is_url = state.focus == Focus::Url;
    let border_color = if is_filter {
        Color::Yellow
    } else if is_url {
        BORDER_FOCUSED
    } else {
        BORDER_UNFOCUSED
    };

    let mut top = BorderPills::new(area.x, area.y, border_color, true);
    top.push_pill(
        vec![
            Span::styled(
                "¹",
                Style::default()
                    .fg(HOTKEY_COLOR)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                if is_filter { "filter" } else { "vdl" },
                Style::default()
                    .fg(TEXT_PRIMARY)
                    .add_modifier(Modifier::BOLD),
            ),
        ],
        Some(ButtonAction::FocusUrl),
        &mut state.buttons,
    );

    if is_filter {
        top.push_key_label("ENTER", "browse".to_string(), None, &mut state.buttons);
        top.push_key_label("ESC", "clear".to_string(), None, &mut state.buttons);
    } else {
        top.push_hotkey_word(
            "a",
            "dd",
            is_url,
            ButtonAction::FocusUrl,
            &mut state.buttons,
        );
        if !state.url_input.is_empty() {
            top.push_key_label(
                "Ctrl-U",
                "clear".to_string(),
                Some(ButtonAction::ClearUrl),
                &mut state.buttons,
            );
        }
        top.push_hotkey_word(
            "l",
            "og",
            state.show_log,
            ButtonAction::ToggleLog,
            &mut state.buttons,
        );
    }

    let total = state.snapshot.sources.len();
    let active = state.snapshot.sources.iter().filter(|s| s.active).count();
    let status_spans = if let Some(ref busy) = state.snapshot.download {
        vec![
            Span::styled(
                format!("⬇ {} ", terminal_text(busy)),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("· {} pending", state.snapshot.pending),
                Style::default().fg(MUTED),
            ),
        ]
    } else if state.snapshot.owner_running {
        vec![
            Span::styled("● ", Style::default().fg(Color::Rgb(0x73, 0xda, 0xca))),
            Span::styled("ready ", Style::default().fg(TEXT_PRIMARY)),
            Span::styled(
                format!(
                    "· {active}/{total} active · {} pending",
                    state.snapshot.pending
                ),
                Style::default().fg(MUTED),
            ),
        ]
    } else {
        vec![
            Span::styled("■ ", Style::default().fg(HOTKEY_COLOR)),
            Span::styled("stopped ", Style::default().fg(HOTKEY_COLOR)),
            Span::styled(
                format!("· {active}/{total} active"),
                Style::default().fg(MUTED),
            ),
        ]
    };

    let mut block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .title_top(top.finish())
        .title_top(right_pill(status_spans, border_color, true).alignment(Alignment::Right));

    // Bottom border of top panel shows normalized URL preview or status message
    if !is_filter && !state.url_input.is_empty() {
        let preview_span = match normalize_url(state.url_input.as_str()) {
            Ok(clean) => Span::styled(
                format!("→ {}", terminal_text(&clean)),
                Style::default().fg(Color::Rgb(0x73, 0xda, 0xca)),
            ),
            Err(err) => Span::styled(
                terminal_text(&err.to_string()),
                Style::default().fg(HOTKEY_COLOR),
            ),
        };
        block = block.title_bottom(right_pill(vec![preview_span], border_color, false));
    } else if let Some(ref msg) = state.message {
        let style = if state.message_is_error {
            Style::default().fg(HOTKEY_COLOR)
        } else {
            Style::default().fg(MSG_COLOR)
        };
        block = block.title_bottom(
            right_pill(
                vec![Span::styled(terminal_text(msg), style)],
                border_color,
                false,
            )
            .alignment(Alignment::Right),
        );
    }

    let content = if is_filter {
        let mut line = vec![Span::styled("/ ", Style::default().fg(Color::Yellow))];
        line.extend(state.filter_input.render_with_cursor(true).spans);
        Line::from(line)
    } else if state.url_input.is_empty() && !is_url {
        Line::from(vec![
            Span::styled("URL: ", Style::default().fg(MUTED)),
            Span::styled(
                "press 'a' or click to paste a profile/channel URL (query stripped)",
                Style::default().fg(MUTED),
            ),
        ])
    } else {
        let mut line = vec![Span::styled(
            "URL: ",
            Style::default().fg(if is_url { BORDER_FOCUSED } else { MUTED }),
        )];
        line.extend(state.url_input.render_with_cursor(is_url).spans);
        Line::from(line)
    };

    frame.render_widget(Paragraph::new(content).block(block), area);
}

fn render_sources_panel(frame: &mut ratatui::Frame<'_>, area: Rect, state: &mut TuiState) {
    state.table_area = area;
    let focused = state.focus == Focus::Table;
    let border_color = if focused {
        BORDER_FOCUSED
    } else {
        BORDER_UNFOCUSED
    };

    let visible_len = state.visible.len();
    let total_len = state.snapshot.sources.len();
    let pos = state.selected_index().map_or(0, |i| i + 1);

    let mut top = BorderPills::new(area.x, area.y, border_color, true);
    top.push_pill(
        vec![
            Span::styled(
                "²",
                Style::default()
                    .fg(HOTKEY_COLOR)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "sources",
                Style::default()
                    .fg(TEXT_PRIMARY)
                    .add_modifier(Modifier::BOLD),
            ),
        ],
        None,
        &mut state.buttons,
    );
    top.push_hotkey_word(
        "f",
        "ilter",
        !state.filter_input.is_empty() || state.focus == Focus::Filter,
        ButtonAction::FocusFilter,
        &mut state.buttons,
    );
    top.push_pill(
        vec![
            Span::styled(
                "s",
                Style::default()
                    .fg(HOTKEY_COLOR)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("ort ", Style::default().fg(TEXT_PRIMARY)),
            Span::styled(
                format!("< {} >", state.sort_mode.label()),
                Style::default()
                    .fg(BORDER_FOCUSED)
                    .add_modifier(Modifier::BOLD),
            ),
        ],
        Some(ButtonAction::CycleSort),
        &mut state.buttons,
    );
    top.push_hotkey_word(
        "n",
        "ow",
        false,
        ButtonAction::DownloadNow,
        &mut state.buttons,
    );
    top.push_hotkey_word(
        "d",
        "isable",
        false,
        ButtonAction::Disable,
        &mut state.buttons,
    );

    let bottom_y = area.y + area.height.saturating_sub(1);
    let mut bottom = BorderPills::new(area.x, bottom_y, border_color, false);
    bottom.push_key_label("↑↓", "select".to_string(), None, &mut state.buttons);
    bottom.push_key_label(
        "F2",
        format!("mouse {}", if state.mouse_enabled { "on" } else { "off" }),
        Some(ButtonAction::ToggleMouse),
        &mut state.buttons,
    );
    bottom.push_key_label(
        "F5",
        "refresh".to_string(),
        Some(ButtonAction::Refresh),
        &mut state.buttons,
    );
    bottom.push_key_label(
        "F6",
        "detach".to_string(),
        Some(ButtonAction::Detach),
        &mut state.buttons,
    );
    bottom.push_hotkey_word("q", "uit", false, ButtonAction::Quit, &mut state.buttons);

    let count_text = if visible_len == 0 {
        format!("0/{total_len}")
    } else if state.filter_input.is_empty() {
        format!("{pos}/{visible_len}")
    } else {
        format!("{pos}/{visible_len} of {total_len}")
    };

    let mut block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .title_top(top.finish())
        .title_bottom(bottom.finish())
        .title_bottom(
            right_pill(
                vec![Span::styled(count_text, Style::default().fg(TEXT_PRIMARY))],
                border_color,
                false,
            )
            .alignment(Alignment::Right),
        );

    if !state.filter_input.is_empty() {
        block = block.title_top(
            right_pill(
                vec![
                    Span::styled("filter: ", Style::default().fg(MUTED)),
                    Span::styled(
                        terminal_text(state.filter_input.as_str()),
                        Style::default().fg(Color::Yellow),
                    ),
                ],
                border_color,
                true,
            )
            .alignment(Alignment::Right),
        );
    }

    if state.visible.is_empty() {
        let empty_msg = if state.filter_input.is_empty() {
            "No sources yet. Press 'a' to add a URL."
        } else {
            "No sources match the current filter. Press Esc to clear."
        };
        frame.render_widget(
            Paragraph::new(empty_msg)
                .style(Style::default().fg(MUTED))
                .block(block),
            area,
        );
        return;
    }

    let wide = area.width >= 72;
    let header_cells: Vec<&str> = if wide {
        vec!["", "#", "SERVICE", "ACCOUNT", "LAST ATTEMPT", "STATE"]
    } else {
        vec!["", "#", "SERVICE", "ACCOUNT", "STATE"]
    };
    let header = Row::new(header_cells).style(Style::default().fg(MUTED));

    let selected_id = state.selected.as_deref();
    let rows: Vec<Row<'_>> = state
        .visible
        .iter()
        .enumerate()
        .map(|(idx, row)| {
            let is_selected = selected_id == Some(row.id.as_str());
            let marker = if is_selected {
                Cell::from("▸").style(
                    Style::default()
                        .fg(BORDER_FOCUSED)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Cell::from(" ")
            };
            let account_style = if row.active {
                Style::default().fg(TEXT_PRIMARY)
            } else {
                Style::default().fg(MUTED)
            };
            let mut cells = vec![
                marker,
                Cell::from((idx + 1).to_string()).style(Style::default().fg(MUTED)),
                Cell::from(terminal_text(&row.service)).style(service_style(&row.service)),
                Cell::from(terminal_text(&row.account)).style(account_style),
            ];
            if wide {
                cells.push(
                    Cell::from(format_age(row.last_check, None)).style(Style::default().fg(MUTED)),
                );
            }
            cells.push(Cell::from(row.state.clone()).style(state_style(&row.state)));
            Row::new(cells)
        })
        .collect();

    let widths: Vec<Constraint> = if wide {
        vec![
            Constraint::Length(2),
            Constraint::Length(4),
            Constraint::Length(12),
            Constraint::Min(12),
            Constraint::Length(14),
            Constraint::Length(12),
        ]
    } else {
        vec![
            Constraint::Length(2),
            Constraint::Length(4),
            Constraint::Length(10),
            Constraint::Min(10),
            Constraint::Length(12),
        ]
    };

    let table = Table::new(rows, widths)
        .header(header)
        .block(block)
        .row_highlight_style(
            Style::default()
                .bg(ROW_HIGHLIGHT_BG)
                .add_modifier(Modifier::BOLD),
        );

    frame.render_stateful_widget(table, area, &mut state.table_state);

    let mut scrollbar_state =
        ScrollbarState::new(visible_len).position(state.selected_index().unwrap_or(0));
    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(Some("↑"))
        .end_symbol(Some("↓"))
        .track_symbol(Some("│"))
        .thumb_symbol("█")
        .style(Style::default().fg(MUTED));
    let scroll_area = Rect {
        x: area.x,
        y: area.y.saturating_add(1),
        width: area.width,
        height: area.height.saturating_sub(2),
    };
    frame.render_stateful_widget(scrollbar, scroll_area, &mut scrollbar_state);
}

fn render_log_panel(frame: &mut ratatui::Frame<'_>, area: Rect, state: &mut TuiState) {
    let border_color = BORDER_UNFOCUSED;
    let mut top = BorderPills::new(area.x, area.y, border_color, true);
    top.push_pill(
        vec![
            Span::styled(
                "³",
                Style::default()
                    .fg(HOTKEY_COLOR)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "log",
                Style::default()
                    .fg(TEXT_PRIMARY)
                    .add_modifier(Modifier::BOLD),
            ),
        ],
        None,
        &mut state.buttons,
    );
    top.push_key_label(
        "l",
        "hide".to_string(),
        Some(ButtonAction::ToggleLog),
        &mut state.buttons,
    );

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .title_top(top.finish())
        .title_top(
            right_pill(
                vec![Span::styled(
                    state.log_file.display().to_string(),
                    Style::default().fg(MUTED),
                )],
                border_color,
                true,
            )
            .alignment(Alignment::Right),
        );

    let lines: Vec<Line<'_>> = state
        .log_lines
        .iter()
        .map(|l| Line::from(Span::styled(l.as_str(), Style::default().fg(MUTED))))
        .collect();
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_confirm_modal(frame: &mut ratatui::Frame<'_>, area: Rect, state: &mut TuiState) {
    let popup = centered_rect(58, 7, area);
    frame.render_widget(Clear, popup);

    let border_color = Color::Yellow;
    let mut top = BorderPills::new(popup.x, popup.y, border_color, true);
    top.push_pill(
        vec![Span::styled(
            "confirm disable",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )],
        None,
        &mut state.buttons,
    );

    let bottom_y = popup.y + popup.height.saturating_sub(1);
    let mut bottom = BorderPills::new(popup.x, bottom_y, border_color, false);
    bottom.push_key_label(
        "ENTER/y",
        "confirm".to_string(),
        Some(ButtonAction::ConfirmDisable),
        &mut state.buttons,
    );
    bottom.push_key_label(
        "ESC/n",
        "cancel".to_string(),
        Some(ButtonAction::CancelDisable),
        &mut state.buttons,
    );

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .title_top(top.finish())
        .title_bottom(bottom.finish());
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let cid = state.confirm_id.as_deref().unwrap_or("");
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(inner);

    frame.render_widget(
        Paragraph::new(format!("Disable {}?", terminal_text(cid)))
            .style(
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
            .alignment(Alignment::Center),
        rows[0],
    );
    frame.render_widget(
        Paragraph::new("A running download will finish; media and archives are kept.")
            .style(Style::default().fg(MUTED))
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
        rows[1],
    );
}

pub fn run_tui(config: Config, mouse_override: Option<bool>) -> Result<(), String> {
    let repository = SourceRepository::new(&config.download_root).map_err(|e| e.to_string())?;
    let mut state = TuiState::new(repository, config.log_file, mouse_override);

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
                Ok(Event::Mouse(mouse)) => state.handle_mouse(mouse, &mut terminal),
                Ok(Event::Paste(text)) => match state.focus {
                    Focus::Url => state.url_input.insert_str(&text),
                    Focus::Filter => {
                        state.filter_input.insert_str(&text);
                        state.rebuild_visible();
                    }
                    _ => {}
                },
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(account: &str, last_check: Option<f64>) -> SourceSnapshot {
        SourceSnapshot {
            id: format!("tiktok/{account}"),
            service: "TikTok".to_string(),
            account: account.to_string(),
            url: format!("https://www.tiktok.com/@{account}"),
            active: true,
            state: "active".to_string(),
            download_requested: false,
            last_check,
        }
    }

    #[test]
    fn text_input_cursor_editing() {
        let mut input = TextInput::default();
        input.insert_str("https://example.test/abc");
        input.move_left();
        input.move_left();
        input.insert_char('X');
        assert_eq!(input.as_str(), "https://example.test/aXbc");
        input.backspace();
        assert_eq!(input.as_str(), "https://example.test/abc");
        input.move_end();
        input.delete_word();
        assert_eq!(input.as_str(), "https://example.test/");
    }

    #[test]
    fn filter_and_sort_modes() {
        let rows = vec![
            sample("zulu", Some(100.0)),
            sample("alpha", Some(300.0)),
            sample("beta", Some(200.0)),
        ];
        let by_name = filter_and_sort_sources(&rows, "", SortMode::NameAsc);
        assert_eq!(
            by_name
                .iter()
                .map(|r| r.account.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "beta", "zulu"]
        );
        let newest = filter_and_sort_sources(&rows, "", SortMode::NewestFirst);
        assert_eq!(
            newest
                .iter()
                .map(|r| r.account.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "beta", "zulu"]
        );
        let oldest = filter_and_sort_sources(&rows, "", SortMode::OldestFirst);
        assert_eq!(
            oldest
                .iter()
                .map(|r| r.account.as_str())
                .collect::<Vec<_>>(),
            vec!["zulu", "beta", "alpha"]
        );
        let filtered = filter_and_sort_sources(&rows, "lu", SortMode::NameAsc);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].account, "zulu");
    }
}
