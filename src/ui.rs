use std::io::{self, Stdout};

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::{Frame, Terminal};

use crate::completion;
use crate::database::Schema;
use crate::engine;
use crate::error::Error;
use crate::highlight::highlight;
use crate::printer::{OutputFormat, render};

type Backend = CrosstermBackend<Stdout>;

/// A single executed query together with its rendered result.
struct Cell {
    query: String,
    result: Result<String, Error>,
}

/// Interactive terminal state: history cells plus the current input line.
struct App<'a> {
    cells: Vec<Cell>,
    input: String,
    cursor: usize,
    input_scroll: u16,
    viewport_top: Option<usize>,
    last_top: usize,
    candidates: Vec<completion::Completion>,
    candidate_cache: completion::CandidateCache,
    selected: usize,
    show_popup: bool,
    history: Vec<String>,
    history_index: Option<usize>,
    draft: String,
    rendered_lines: Vec<Line<'static>>,
    rendered_cells: usize,
    schema: &'a mut Schema,
}

/// Run the interactive REPL until the user quits.
pub fn run(schema: &mut Schema) -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let result = run_app(&mut terminal, schema);
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    result
}

fn run_app(terminal: &mut Terminal<Backend>, schema: &mut Schema) -> io::Result<()> {
    let mut app = App {
        cells: Vec::new(),
        input: String::new(),
        cursor: 0,
        input_scroll: 0,
        viewport_top: None,
        last_top: 0,
        candidates: Vec::new(),
        candidate_cache: completion::CandidateCache::new(),
        selected: 0,
        show_popup: false,
        history: Vec::new(),
        history_index: None,
        draft: String::new(),
        rendered_lines: Vec::new(),
        rendered_cells: 0,
        schema,
    };
    loop {
        terminal.draw(|frame| draw(frame, &mut app))?;
        match event::read()? {
            Event::Key(key) => {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                if app.handle_key(key) {
                    break;
                }
            }
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => app.scroll(-3),
                MouseEventKind::ScrollDown => app.scroll(3),
                _ => {}
            },
            Event::Resize(_, _) => {}
            _ => {}
        }
    }
    Ok(())
}

fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let history_height = area.height.saturating_sub(2);
    let history_area = Rect::new(area.x, area.y, area.width, history_height);
    let input_area = Rect::new(area.x, area.y + history_height, area.width, 1);
    let bar_area = Rect::new(area.x, area.y + history_height + 1, area.width, 1);

    // Rebuild the highlighted history lines only when a new cell arrives;
    // keystrokes just clone the cached lines.
    if app.rendered_cells != app.cells.len() {
        app.rendered_lines = app.history_lines();
        app.rendered_cells = app.cells.len();
    }
    let lines = app.rendered_lines.clone();
    let total = lines.len();
    let viewport = history_height as usize;
    let bottom = total.saturating_sub(viewport);
    let top = app.viewport_top.unwrap_or(bottom).min(bottom);
    app.last_top = top;
    app.viewport_top = app.viewport_top.map(|t| t.min(bottom));
    if app.viewport_top.is_some_and(|t| t >= bottom) {
        app.viewport_top = None;
    }

    frame.render_widget(Paragraph::new(lines).scroll((top as u16, 0)), history_area);

    let help = Span::styled(
        "Enter: run   Tab: complete   Esc: close/clear   Ctrl+C: quit   Up/Down: history   Wheel: scroll",
        Style::default().fg(Color::DarkGray),
    );
    frame.render_widget(Paragraph::new(help), bar_area);

    let prompt = "sheetql> ";
    app.adjust_input_scroll(prompt, area.width);
    let mut spans = vec![Span::styled(
        prompt.to_string(),
        Style::default().add_modifier(Modifier::BOLD),
    )];
    spans.extend(
        highlight(&app.input)
            .into_iter()
            .flat_map(|line| line.spans),
    );
    frame.render_widget(
        Paragraph::new(Line::from(spans)).scroll((0, app.input_scroll)),
        input_area,
    );

    let prompt_width = unicode_width::UnicodeWidthStr::width(prompt) as u16;
    let before = &app.input[..app.cursor];
    let cursor_x =
        prompt_width + unicode_width::UnicodeWidthStr::width(before) as u16 - app.input_scroll;
    let cursor_x = cursor_x.min(area.width.saturating_sub(1));
    let cursor_y = input_area.y;
    frame.set_cursor_position((input_area.x + cursor_x, cursor_y));

    if app.show_popup && !app.candidates.is_empty() {
        let mut state = ListState::default();
        state.select(Some(app.selected));
        let items: Vec<ListItem> = app
            .candidates
            .iter()
            .map(|candidate| {
                ListItem::new(candidate.label.clone()).style(style_for(candidate.kind))
            })
            .collect();
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title("Complete"))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
            .highlight_symbol("> ");
        let popup = popup_rect(area, app.candidates.len(), cursor_x, cursor_y);
        frame.render_widget(Clear, popup);
        frame.render_stateful_widget(list, popup, &mut state);
    }
}

fn style_for(kind: completion::Kind) -> Style {
    let color = match kind {
        completion::Kind::Keyword => Color::Cyan,
        completion::Kind::Function => Color::Blue,
        completion::Kind::Table => Color::Yellow,
        completion::Kind::Column => Color::Green,
        completion::Kind::Database => Color::Magenta,
    };
    Style::default().fg(color)
}

/// Place the completion popup anchored on the cursor: horizontally aligned
/// with it, opening upward just above the input line (or below the cursor
/// when there is no room above).
fn popup_rect(area: Rect, candidate_count: usize, cursor_x: u16, cursor_y: u16) -> Rect {
    let height = (candidate_count as u16).min(8) + 2;
    let width = if area.width >= 24 {
        area.width.min(50)
    } else {
        area.width
    };
    let x = cursor_x
        .saturating_sub(width / 2)
        .min(area.x + area.width.saturating_sub(width));
    let y = if cursor_y >= height {
        cursor_y - height
    } else {
        cursor_y + 1
    };
    Rect {
        x,
        y,
        width,
        height,
    }
}

impl App<'_> {
    /// Handle a key press. Returns `true` when the user wants to quit.
    fn handle_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) && c == 'c' => {
                return true;
            }
            KeyCode::Char(c) => {
                self.input.insert(self.cursor, c);
                self.cursor += c.len_utf8();
                self.refresh_candidates();
            }
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    let len = self.input[..self.cursor].chars().last().unwrap().len_utf8();
                    self.input.remove(self.cursor - len);
                    self.cursor -= len;
                    self.refresh_candidates();
                }
            }
            KeyCode::Delete => {
                if self.cursor < self.input.len() {
                    self.input.remove(self.cursor);
                    self.refresh_candidates();
                }
            }
            KeyCode::Left => {
                if self.cursor > 0 {
                    self.cursor -= self.input[..self.cursor].chars().last().unwrap().len_utf8();
                    self.refresh_candidates();
                }
            }
            KeyCode::Right => {
                if self.cursor < self.input.len() {
                    self.cursor += self.input[self.cursor..].chars().next().unwrap().len_utf8();
                    self.refresh_candidates();
                }
            }
            KeyCode::Home => {
                self.cursor = 0;
                self.refresh_candidates();
            }
            KeyCode::End => {
                self.cursor = self.input.len();
                self.refresh_candidates();
            }
            KeyCode::Enter => {
                if self.show_popup {
                    self.apply_completion();
                }
                let query = self.input.trim().to_lowercase();
                if matches!(query.as_str(), "exit" | "quit") {
                    return true;
                }
                if !self.show_popup && !self.input.trim().is_empty() {
                    self.execute();
                }
            }
            KeyCode::Tab => {
                if self.show_popup {
                    self.apply_completion();
                }
            }
            KeyCode::Esc => {
                if self.show_popup {
                    self.close_popup();
                } else {
                    self.input.clear();
                    self.cursor = 0;
                    self.input_scroll = 0;
                }
            }
            KeyCode::Up => {
                if self.show_popup {
                    self.selected = self.selected.saturating_sub(1);
                } else {
                    self.history_prev();
                }
            }
            KeyCode::Down => {
                if self.show_popup {
                    self.selected =
                        (self.selected + 1).min(self.candidates.len().saturating_sub(1));
                } else {
                    self.history_next();
                }
            }
            _ => {}
        }
        false
    }

    fn refresh_candidates(&mut self) {
        let before = &self.input[..self.cursor];
        if before.trim_end().ends_with(';') {
            self.close_popup();
            return;
        }
        let candidates = self
            .candidate_cache
            .candidates(self.schema, before)
            .to_vec();
        self.selected = self.selected.min(candidates.len().saturating_sub(1));
        self.candidates = candidates;
        self.show_popup = !self.candidates.is_empty();
    }

    fn apply_completion(&mut self) {
        let Some(candidate) = self.candidates.get(self.selected).cloned() else {
            return;
        };
        let start = self.cursor - completion::trailing_token_len(&self.input[..self.cursor]);
        self.input
            .replace_range(start..self.cursor, &candidate.value);
        self.cursor = start + candidate.value.len();
        self.close_popup();
    }

    fn close_popup(&mut self) {
        self.candidates.clear();
        self.selected = 0;
        self.show_popup = false;
    }

    fn execute(&mut self) {
        let query = self.input.trim().to_string();
        if query.is_empty() {
            return;
        }
        let result = match engine::run_query(self.schema, &query) {
            Ok(result) => Ok(render(OutputFormat::Table, &result.columns, &result.rows)),
            Err(error) => Err(error),
        };
        self.cells.push(Cell {
            query: query.clone(),
            result,
        });
        if self.history.last().is_none_or(|last| last != &query) {
            self.history.push(query);
        }
        self.input.clear();
        self.cursor = 0;
        self.input_scroll = 0;
        self.history_index = None;
        self.draft.clear();
        self.viewport_top = None;
        self.close_popup();
    }

    /// Move to the previous entry of the input history, saving the current
    /// input as a draft on the first press.
    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        match self.history_index {
            None => {
                self.draft = self.input.clone();
                self.history_index = Some(self.history.len() - 1);
            }
            Some(i) if i > 0 => self.history_index = Some(i - 1),
            Some(_) => return,
        }
        self.load_history_entry();
    }

    /// Move to the next entry of the input history, restoring the draft once
    /// the newest entry is passed.
    fn history_next(&mut self) {
        match self.history_index {
            Some(i) if i + 1 < self.history.len() => {
                self.history_index = Some(i + 1);
                self.load_history_entry();
            }
            Some(_) => {
                self.history_index = None;
                self.input = std::mem::take(&mut self.draft);
                self.cursor = self.input.len();
                self.refresh_candidates();
            }
            None => {}
        }
    }

    fn load_history_entry(&mut self) {
        if let Some(i) = self.history_index {
            self.input = self.history[i].clone();
            self.cursor = self.input.len();
            self.refresh_candidates();
        }
    }

    /// Scroll the history area by `lines` rows. Reaching the bottom snaps
    /// back to following the newest output.
    fn scroll(&mut self, lines: isize) {
        let current = match self.viewport_top {
            Some(top) => top as isize,
            None => self.last_top as isize,
        };
        self.viewport_top = Some((current + lines).max(0) as usize);
    }

    /// Keep the visible slice of the input line around the cursor.
    fn adjust_input_scroll(&mut self, prompt: &str, area_width: u16) {
        let prompt_width = unicode_width::UnicodeWidthStr::width(prompt) as u16;
        let before_width = unicode_width::UnicodeWidthStr::width(&self.input[..self.cursor]) as u16;
        let cursor_x = prompt_width + before_width;
        if cursor_x < self.input_scroll {
            self.input_scroll = cursor_x;
        } else if cursor_x >= self.input_scroll + area_width {
            self.input_scroll = cursor_x - area_width + 1;
        }
    }

    fn history_lines(&self) -> Vec<Line<'static>> {
        let mut lines: Vec<Line<'static>> = Vec::new();
        for (i, cell) in self.cells.iter().enumerate() {
            lines.push(Line::from(Span::styled(
                format!("In [{i}]:"),
                Style::default().add_modifier(Modifier::BOLD),
            )));
            lines.extend(highlight(&cell.query));
            lines.push(Line::from(Span::styled(
                format!("Out [{i}]:"),
                Style::default().add_modifier(Modifier::BOLD),
            )));
            match &cell.result {
                Ok(text) => {
                    for result_line in text.lines() {
                        lines.push(Line::from(Span::raw(result_line.to_string())));
                    }
                }
                Err(error) => {
                    lines.push(Line::from(Span::styled(
                        error.to_string(),
                        Style::default().fg(Color::Red),
                    )));
                }
            }
            lines.push(Line::from(""));
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::{Database, Table};
    use crate::value::Value;

    fn make_schema() -> Schema {
        let mut schema = Schema::new();
        let mut database = Database::named("test");
        database.add_table(Table {
            name: "people".to_string(),
            columns: vec!["id".to_string(), "name".to_string()],
            rows: vec![vec![Value::Int(1), Value::Text("Alice".into())]],
        });
        schema.add_database(database);
        schema.set_current_database("test").unwrap();
        schema
    }

    fn make_app(schema: &mut Schema) -> App<'_> {
        App {
            cells: Vec::new(),
            input: String::new(),
            cursor: 0,
            input_scroll: 0,
            viewport_top: None,
            last_top: 0,
            candidates: Vec::new(),
            candidate_cache: completion::CandidateCache::new(),
            selected: 0,
            show_popup: false,
            history: Vec::new(),
            history_index: None,
            draft: String::new(),
            rendered_lines: Vec::new(),
            rendered_cells: 0,
            schema,
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn popup_rect_opens_above_the_input_line_when_there_is_room() {
        let area = Rect::new(0, 0, 100, 40);
        let popup = popup_rect(area, 5, 30, 39);
        assert_eq!(popup.x, 5);
        assert_eq!(popup.y, 32);
        assert_eq!(popup.width, 50);
        assert_eq!(popup.height, 7);
    }

    #[test]
    fn popup_rect_falls_below_the_cursor_when_no_room_above() {
        let area = Rect::new(0, 0, 100, 40);
        let popup = popup_rect(area, 5, 30, 0);
        assert_eq!(popup.y, 1);
    }

    #[test]
    fn popup_rect_clamps_horizontally_within_the_area() {
        let area = Rect::new(0, 0, 100, 40);
        // A cursor near the right edge right-aligns the popup.
        let popup = popup_rect(area, 5, 95, 39);
        assert_eq!(popup.x, 50);
        // Candidate count caps the popup height at 10 rows.
        let popup = popup_rect(area, 100, 30, 39);
        assert_eq!(popup.height, 10);
    }

    #[test]
    fn handle_key_types_and_backspaces_unicode_text() {
        let mut schema = make_schema();
        let mut app = make_app(&mut schema);
        for c in ['a', 'é', '中'] {
            assert!(!app.handle_key(key(KeyCode::Char(c))));
        }
        assert_eq!(app.input, "aé中");
        assert_eq!(app.cursor, app.input.len());
        assert!(!app.handle_key(key(KeyCode::Backspace)));
        assert_eq!(app.input, "aé");
        assert_eq!(app.cursor, 3);
    }

    #[test]
    fn handle_key_enter_executes_query_clears_input_and_records_history() {
        let mut schema = make_schema();
        let mut app = make_app(&mut schema);
        for c in "SELECT 1".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        assert!(!app.handle_key(key(KeyCode::Enter)));
        assert_eq!(app.cells.len(), 1);
        assert!(app.cells[0].result.is_ok());
        assert!(app.input.is_empty());
        assert_eq!(app.cursor, 0);
        assert_eq!(app.history, vec!["SELECT 1"]);
    }

    #[test]
    fn handle_key_exit_and_quit_return_true() {
        let mut schema = make_schema();
        let mut app = make_app(&mut schema);
        for c in "exit".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        assert!(app.handle_key(key(KeyCode::Enter)));
        let mut app = make_app(&mut schema);
        for c in "quit".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        assert!(app.handle_key(key(KeyCode::Enter)));
    }

#[test]
fn handle_key_escape_closes_the_popup_before_clearing_the_line() {
    let mut schema = make_schema();
    let mut app = make_app(&mut schema);
    // A partial keyword opens the completion popup; Esc closes it and leaves
    // the input untouched.
    app.input = "SEL".to_string();
    app.cursor = 3;
    app.refresh_candidates();
    assert!(app.show_popup);
    app.handle_key(key(KeyCode::Esc));
    assert!(!app.show_popup);
    assert_eq!(app.input, "SEL");

    // With no popup, Esc clears the line.
    app.handle_key(key(KeyCode::Esc));
    assert!(app.input.is_empty());
    assert_eq!(app.cursor, 0);
}

    #[test]
    fn history_navigation_saves_a_draft_and_restores_it() {
        let mut schema = make_schema();
        let mut app = make_app(&mut schema);
        for c in "SELECT 1".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Enter));
        // Up restores the entry into the input line.
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.input, "SELECT 1");
        // Down returns to the empty draft.
        app.handle_key(key(KeyCode::Down));
        assert!(app.input.is_empty());
        assert_eq!(app.history_index, None);
    }

    #[test]
    fn history_prev_does_nothing_when_there_is_no_history() {
        let mut schema = make_schema();
        let mut app = make_app(&mut schema);
        app.handle_key(key(KeyCode::Up));
        assert!(app.input.is_empty());
        assert_eq!(app.history_index, None);
    }

    #[test]
    fn execute_dedupes_consecutive_history_entries() {
        let mut schema = make_schema();
        let mut app = make_app(&mut schema);
        for _ in 0..2 {
            for c in "SELECT 1".chars() {
                app.handle_key(key(KeyCode::Char(c)));
            }
            app.handle_key(key(KeyCode::Enter));
        }
        assert_eq!(app.history, vec!["SELECT 1"]);
    }

    #[test]
    fn scroll_uses_last_top_as_fallback_and_clamps_at_zero() {
        let mut schema = make_schema();
        let mut app = make_app(&mut schema);
        app.last_top = 10;
        app.scroll(3);
        assert_eq!(app.viewport_top, Some(13));
        app.scroll(-5);
        assert_eq!(app.viewport_top, Some(8));
        app.scroll(-100);
        assert_eq!(app.viewport_top, Some(0));
    }

    #[test]
    fn adjust_input_scroll_keeps_the_cursor_visible() {
        let mut schema = make_schema();
        let mut app = make_app(&mut schema);
        for c in "abcdefghijklmnopqrstuvwxyz".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.adjust_input_scroll("sheetql> ", 20);
        assert_eq!(app.input_scroll, 16);
        // Moving to the start scrolls the prompt back into view.
        app.handle_key(key(KeyCode::Home));
        app.adjust_input_scroll("sheetql> ", 20);
        assert_eq!(app.input_scroll, 9);
    }
}
