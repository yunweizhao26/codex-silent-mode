use crate::conversation::{Conversation, Kind};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use unicode_width::UnicodeWidthChar;

#[derive(Default)]
pub struct View {
    pub expanded: bool,
    // Item + wrapped line, so streamed output below the viewport cannot move it.
    anchor: Option<(usize, usize)>,
    rows: Vec<(usize, usize, Line<'static>)>,
    cache_key: Option<(u64, u16, bool)>,
    pub height: usize,
}

impl View {
    fn rows(&mut self, conversation: &Conversation, width: u16) {
        let key = (conversation.revision, width, self.expanded);
        if self.cache_key == Some(key) {
            return;
        }
        self.cache_key = Some(key);
        self.rows.clear();
        for (index, entry) in conversation.entries.iter().enumerate() {
            if !self.expanded && entry.kind == Kind::Activity {
                continue;
            }
            let color = match entry.kind {
                Kind::User => Color::Cyan,
                Kind::Answer => Color::Green,
                Kind::Activity => Color::DarkGray,
                Kind::Notice => Color::Yellow,
            };
            self.rows.push((
                index,
                0,
                Line::styled(
                    clean(&entry.label),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
            ));
            for (number, text) in wrap(&entry.text, width as usize).into_iter().enumerate() {
                self.rows.push((index, number + 1, Line::raw(text)));
            }
            self.rows.push((index, usize::MAX, Line::raw("")));
        }
    }

    fn top(&self) -> usize {
        self.anchor
            .map(|anchor| {
                self.rows
                    .iter()
                    .position(|(item, line, _)| (*item, *line) >= anchor)
                    .unwrap_or(self.rows.len().saturating_sub(self.height))
            })
            .unwrap_or(self.rows.len().saturating_sub(self.height))
    }

    pub fn scroll(&mut self, by: isize) {
        let top = self
            .top()
            .saturating_add_signed(by)
            .min(self.rows.len().saturating_sub(self.height));
        self.anchor = self.rows.get(top).map(|(item, line, _)| (*item, *line));
    }

    pub fn home(&mut self) {
        self.anchor = Some((0, 0));
    }
    pub fn end(&mut self) {
        self.anchor = None;
    }
    pub fn toggle(&mut self) {
        self.expanded = !self.expanded;
    }

    pub fn draw(
        &mut self,
        frame: &mut Frame<'_>,
        conversation: &Conversation,
        status: &str,
        input: &str,
        cursor: usize,
        pending: Option<&str>,
    ) {
        let area = frame.area();
        let input_height = (input.lines().count().max(1) + 2).min(6) as u16;
        let regions = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(input_height),
        ])
        .split(area);
        let title = if self.expanded {
            " Codex Silent · activity shown "
        } else {
            " Codex Silent · answers only "
        };
        let block = Block::default().borders(Borders::ALL).title(title);
        let inner = block.inner(regions[0]);
        frame.render_widget(block, regions[0]);
        self.height = inner.height as usize;
        self.rows(conversation, inner.width);
        let top = self.top();
        let visible: Vec<Line<'_>> = self
            .rows
            .iter()
            .skip(top)
            .take(self.height)
            .map(|row| row.2.clone())
            .collect();
        frame.render_widget(Paragraph::new(visible), inner);
        let mode = if self.expanded { "hide" } else { "show" };
        let banner = format!(
            " {status} · {} activity items · Ctrl+O {mode} · PgUp/PgDn scroll · /help",
            conversation.hidden_count()
        );
        frame.render_widget(
            Paragraph::new(clean(&banner)).style(Style::default().fg(Color::DarkGray)),
            regions[1],
        );
        draw_input(frame, regions[2], input, cursor, pending.is_some());
    }
}

fn draw_input(frame: &mut Frame<'_>, area: Rect, input: &str, cursor: usize, answering: bool) {
    let title = if answering {
        " Response required · Enter to answer "
    } else {
        " Message · Enter send · Alt+Enter newline "
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let width = inner.width.max(1) as usize;
    // Use the same wrapping for input and cursor positioning, including Unicode.
    let rows = wrap(input, width);
    let prefix = wrap(&input[..cursor.min(input.len())], width);
    let row = prefix.len().saturating_sub(1);
    let column = prefix
        .last()
        .map(|line| line.chars().map(|c| c.width().unwrap_or(0)).sum::<usize>())
        .unwrap_or(0);
    let scroll = row.saturating_sub(inner.height.saturating_sub(1) as usize);
    let lines: Vec<Line<'_>> = rows
        .into_iter()
        .skip(scroll)
        .take(inner.height as usize)
        .map(Line::raw)
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
    if inner.width > 0 && inner.height > 0 {
        frame.set_cursor_position((
            inner.x + column.min(width - 1) as u16,
            inner.y + (row - scroll) as u16,
        ));
    }
}

/// Strip terminal control sequences before displaying external text.
pub fn clean(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.next() {
                Some('[') => {
                    for next in chars.by_ref() {
                        if ('@'..='~').contains(&next) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(next) = chars.next() {
                        if next == '\x07' {
                            break;
                        }
                        if next == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {}
            }
        } else if c == '\t' {
            result.push_str("    ");
        } else if c == '\n' || !c.is_control() {
            result.push(c);
        }
    }
    result
}

pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let text = clean(text);
    let mut rows = Vec::new();
    for line in text.split('\n') {
        let mut row = String::new();
        let mut columns = 0;
        for c in line.chars() {
            let len = c.width().unwrap_or(0);
            if columns + len > width && !row.is_empty() {
                rows.push(std::mem::take(&mut row));
                columns = 0;
            }
            row.push(c);
            columns += len;
        }
        rows.push(row);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};
    use serde_json::json;

    fn text(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    }

    #[test]
    fn actual_frame_hides_and_reveals_native_tool_output() {
        let mut c = Conversation::default();
        for item in [
            json!({"id":"q","type":"userMessage","content":[{"type":"text","text":"Run the test"}]}),
            json!({"id":"cmd","type":"commandExecution","command":"echo TOOL_MARKER","aggregatedOutput":"TOOL_MARKER"}),
            json!({"id":"a","type":"agentMessage","text":"All tests passed."}),
        ] {
            c.event(&json!({"method":"item/completed","params":{"item":item}}));
        }
        let mut v = View::default();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| v.draw(f, &c, "Ready", "", 0, None))
            .unwrap();
        assert!(text(&terminal).contains("Run the test"));
        assert!(text(&terminal).contains("All tests passed."));
        assert!(!text(&terminal).contains("TOOL_MARKER"));
        v.toggle();
        terminal
            .draw(|f| v.draw(f, &c, "Ready", "", 0, None))
            .unwrap();
        assert!(text(&terminal).contains("TOOL_MARKER"));
        v.toggle();
        terminal
            .draw(|f| v.draw(f, &c, "Ready", "", 0, None))
            .unwrap();
        assert!(!text(&terminal).contains("TOOL_MARKER"));
    }

    #[test]
    fn scrolling_stays_at_earlier_answers_while_activity_streams() {
        let mut c = Conversation::default();
        for i in 0..40 {
            c.notice(format!("answer {i}"));
        }
        let mut v = View::default();
        let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();
        v.home();
        terminal
            .draw(|f| v.draw(f, &c, "Working", "", 0, None))
            .unwrap();
        let before = text(&terminal);
        for i in 0..1000 {
            c.event(&json!({"method":"item/completed","params":{"item":{"type":"commandExecution","id":i.to_string(),"aggregatedOutput":"NOISE"}}}));
        }
        terminal
            .draw(|f| v.draw(f, &c, "Working", "", 0, None))
            .unwrap();
        assert!(before.contains("answer 0"));
        assert!(text(&terminal).contains("answer 0"));
        assert!(!text(&terminal).contains("NOISE"));
    }

    #[test]
    fn hostile_escape_sequences_never_reach_terminal() {
        assert_eq!(clean("a\x1b[2Jb\x1b]52;c;clipboard\x07c\x00"), "abc");
        assert_eq!(wrap("中文abc", 4), ["中文", "abc"]);
    }
}
