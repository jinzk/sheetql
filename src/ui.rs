use std::io::{self, Stdout};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::{Frame, Terminal};

use crate::database::Schema;
use crate::engine;
use crate::highlight::highlight;
use crate::printer::{render, OutputFormat};

type Backend = CrosstermBackend<Stdout>;

/// A single executed query together with its rendered result.
struct Cell {
    query: String,
    result: Result<String, String>,
}

/// Interactive terminal state: history cells plus the current input line.
struct App<'a> {
    cells: Vec<Cell>,
    input: String,
    cursor: usize,
    input_scroll: u16,
    viewport_top: Option<usize>,
    last_top: usize,
    viewport_height: usize,
    schema: &'a mut Schema,
}

/// Run the interactive REPL until the user quits.
pub fn run(schema: &mut Schema) -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let result = run_app(&mut terminal, schema);
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
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
        viewport_height: 0,
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

    let lines = app.history_lines();
    let total = lines.len();
    let viewport = history_height as usize;
    let bottom = total.saturating_sub(viewport);
    let top = app.viewport_top.unwrap_or(bottom).min(bottom);
    app.last_top = top;
    app.viewport_height = viewport;

    frame.render_widget(Paragraph::new(lines).scroll((top as u16, 0)), history_area);

    let help = Span::styled(
        "Enter: run   Esc: clear   Ctrl+C: quit   Up/Down: scroll   PageUp/PageDown: page",
        Style::default().fg(Color::DarkGray),
    );
    frame.render_widget(Paragraph::new(help), bar_area);

    let prompt = "sheetql> ";
    app.adjust_input_scroll(prompt, area.width);
    let mut spans = vec![Span::styled(
        prompt.to_string(),
        Style::default().add_modifier(Modifier::BOLD),
    )];
    spans.extend(highlight(&app.input).into_iter().flat_map(|line| line.spans));
    frame.render_widget(
        Paragraph::new(Line::from(spans)).scroll((0, app.input_scroll)),
        input_area,
    );

    let prompt_width = unicode_width::UnicodeWidthStr::width(prompt) as u16;
    let before = &app.input[..app.cursor];
    let cursor_x = prompt_width
        + unicode_width::UnicodeWidthStr::width(before) as u16
        - app.input_scroll;
    let cursor_x = cursor_x.min(area.width.saturating_sub(1));
    frame.set_cursor_position((input_area.x + cursor_x, input_area.y));
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
            }
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    let len = self.input[..self.cursor]
                        .chars()
                        .last()
                        .unwrap()
                        .len_utf8();
                    self.input.remove(self.cursor - len);
                    self.cursor -= len;
                }
            }
            KeyCode::Delete => {
                if self.cursor < self.input.len() {
                    self.input.remove(self.cursor);
                }
            }
            KeyCode::Left => {
                if self.cursor > 0 {
                    self.cursor -= self.input[..self.cursor]
                        .chars()
                        .last()
                        .unwrap()
                        .len_utf8();
                }
            }
            KeyCode::Right => {
                if self.cursor < self.input.len() {
                    self.cursor +=
                        self.input[self.cursor..].chars().next().unwrap().len_utf8();
                }
            }
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.input.len(),
            KeyCode::Enter => {
                let query = self.input.trim();
                if matches!(query, "exit" | "quit") {
                    return true;
                }
                self.execute();
            }
            KeyCode::Esc => {
                self.input.clear();
                self.cursor = 0;
                self.input_scroll = 0;
            }
            KeyCode::Up => self.scroll(-1),
            KeyCode::Down => self.scroll(1),
            KeyCode::PageUp => self.scroll(-(self.viewport_height as isize)),
            KeyCode::PageDown => self.scroll(self.viewport_height as isize),
            _ => {}
        }
        false
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
        self.cells.push(Cell { query, result });
        self.input.clear();
        self.cursor = 0;
        self.input_scroll = 0;
        self.follow_bottom();
    }

    fn scroll(&mut self, lines: isize) {
        let current = match self.viewport_top {
            Some(top) => top as isize,
            None => self.last_top as isize,
        };
        self.viewport_top = Some((current + lines).max(0) as usize);
    }

    fn follow_bottom(&mut self) {
        self.viewport_top = None;
    }

    /// Keep the visible slice of the input line around the cursor.
    fn adjust_input_scroll(&mut self, prompt: &str, area_width: u16) {
        let prompt_width = unicode_width::UnicodeWidthStr::width(prompt) as u16;
        let before_width = unicode_width::UnicodeWidthStr::width(&self.input[..self.cursor])
            as u16;
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
                        error.clone(),
                        Style::default().fg(Color::Red),
                    )));
                }
            }
            lines.push(Line::from(""));
        }
        lines
    }
}