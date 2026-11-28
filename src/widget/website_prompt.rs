use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Position, Rect},
    style::{Modifier, Style},
    text::Span,
    widgets::{Block, Clear, Paragraph, Wrap},
};

use crate::{
    inputs::KeySeq,
    keybindings::{
        action::Action, context::KeyContext, keymap, keymap::LookupResult,
        notation::key_event_to_input,
    },
    theme::current_theme,
    vendored::tui_textarea::TextArea,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WebsiteIntent {
    Open,
    Save,
    Close,
}

struct Button {
    compact: &'static str,
    label: String,
    width: u16,
    intent: WebsiteIntent,
    area: Rect,
}

impl Button {
    fn new(compact: &'static str, binding: &str, intent: WebsiteIntent) -> Self {
        let label = format!("{compact} ({binding})");
        let width = Span::raw(label.as_str()).width();
        Self {
            compact,
            label,
            width: u16::try_from(width).unwrap_or(u16::MAX),
            intent,
            area: Rect::default(),
        }
    }

    fn display(&self, available: u16) -> (&str, u16) {
        if self.width <= available {
            (&self.label, self.width)
        } else {
            (self.compact, available.min(self.compact.len() as u16))
        }
    }
}

pub(crate) struct WebsitePrompt {
    editor: TextArea<'static>,
    pub(crate) error: Option<String>,
    buttons: [Button; 3],
    hide: Button,
}

impl Default for WebsitePrompt {
    fn default() -> Self {
        let mut editor = TextArea::default();
        editor.set_placeholder_text("https://example.org/article");
        let km = keymap();
        let label = |action| {
            km.describe_binding_display(KeyContext::Website, &action)
                .unwrap_or_else(|| "unbound".into())
        };
        let close = label(Action::Cancel);
        Self {
            editor,
            error: None,
            buttons: [
                Button::new("[Save & open]", &label(Action::Select), WebsiteIntent::Open),
                Button::new(
                    "[Save only]",
                    &label(Action::SaveWebsite),
                    WebsiteIntent::Save,
                ),
                Button::new("[Close]", &close, WebsiteIntent::Close),
            ],
            hide: Button::new("[Hide]", &close, WebsiteIntent::Close),
        }
    }
}

fn url_char(c: char) -> bool {
    !c.is_control() && !matches!(c, '\u{2028}' | '\u{2029}')
}

impl WebsitePrompt {
    pub(crate) fn url(&self) -> &str {
        &self.editor.lines()[0]
    }

    pub(crate) fn paste(&mut self, text: &str) {
        let clean;
        let text = if text.chars().all(url_char) {
            text
        } else {
            clean = text.chars().filter(|&c| url_char(c)).collect::<String>();
            &clean
        };
        if !text.is_empty() && self.editor.insert_str(text) {
            self.error = None;
        }
    }

    pub(crate) fn key(
        &mut self,
        key: KeyEvent,
        busy: bool,
        seq: &mut KeySeq,
    ) -> Option<WebsiteIntent> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        let input = key_event_to_input(&key);
        let km = keymap();
        let found = if seq.is_empty() {
            km.lookup(KeyContext::Website, &[input])
        } else {
            seq.push(key);
            let keys: Vec<_> = seq.keys().iter().map(key_event_to_input).collect();
            match km.lookup(KeyContext::Website, &keys) {
                LookupResult::Prefix => return None,
                LookupResult::NoMatch => {
                    seq.clear();
                    km.lookup(KeyContext::Website, &[input])
                }
                action => action,
            }
        };
        match found {
            LookupResult::Found(action) => {
                seq.clear();
                return match action {
                    Action::Cancel => Some(WebsiteIntent::Close),
                    Action::Select if !busy => Some(WebsiteIntent::Open),
                    Action::SaveWebsite if !busy => Some(WebsiteIntent::Save),
                    _ => None,
                };
            }
            LookupResult::Prefix => {
                seq.push(key);
                return None;
            }
            LookupResult::NoMatch => (),
        }
        if !busy {
            let changed = match key.code {
                KeyCode::Char(c)
                    if (key.modifiers - KeyModifiers::SHIFT).is_empty() && url_char(c) =>
                {
                    self.editor.insert_char(c);
                    true
                }
                KeyCode::Left
                | KeyCode::Right
                | KeyCode::Home
                | KeyCode::End
                | KeyCode::Delete
                | KeyCode::Backspace => self.editor.input(key),
                _ => false,
            };
            if changed {
                self.error = None;
            }
        }
        None
    }

    pub(crate) fn click(&self, col: u16, row: u16, busy: bool) -> Option<WebsiteIntent> {
        let buttons = if busy {
            std::slice::from_ref(&self.hide)
        } else {
            self.buttons.as_slice()
        };
        buttons
            .iter()
            .find(|button| button.area.contains(Position::new(col, row)))
            .map(|button| button.intent)
    }

    pub(crate) fn started_saving(&mut self) {
        self.error = None;
        for button in &mut self.buttons {
            button.area = Rect::default();
        }
        self.hide.area = Rect::default();
    }

    pub(crate) fn render(&mut self, frame: &mut Frame, busy: bool) {
        let screen = frame.area();
        let theme = current_theme();
        let normal = Style::default().fg(theme.base_05).bg(theme.base_00);
        let width = screen.width.min(72);
        let borders = screen.height >= 4 && width >= 4;
        let border_space = if borders { 2 } else { 0 };
        let inner_width = width - border_space;
        let buttons = if busy {
            std::slice::from_mut(&mut self.hide)
        } else {
            self.buttons.as_mut_slice()
        };
        let mut offsets = [Rect::default(); 3];
        let (mut x, mut y) = (0, 0);
        for (button, offset) in buttons.iter_mut().zip(&mut offsets) {
            button.area = Rect::default();
            let (_, width) = button.display(inner_width);
            if x != 0 && x + width > inner_width {
                x = 0;
                y += 1;
            }
            *offset = Rect::new(x, y, width, 1);
            x += width + 1;
        }
        let button_rows = y + 1;
        let error_rows = if self.error.is_some() { 3 } else { 0 };
        let area = screen.centered(
            Constraint::Length(width),
            Constraint::Length(1 + u16::from(busy) + border_space + error_rows + button_rows),
        );
        frame.render_widget(Clear, area);
        let block = if borders {
            Block::bordered().title(" Website URL ")
        } else {
            Block::default()
        }
        .border_style(Style::default().fg(theme.popup_border_color()))
        .style(normal);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.is_empty() {
            return;
        }

        let mut body = inner;
        if !busy || inner.height >= 3 {
            self.editor.set_style(normal);
            self.editor.set_placeholder_style(normal.fg(theme.base_04));
            self.editor.set_cursor_line_style(normal);
            self.editor.set_cursor_style(if busy {
                normal
            } else {
                normal.add_modifier(Modifier::REVERSED)
            });
            frame.render_widget(&self.editor, Rect { height: 1, ..body });
            body.y += 1;
            body.height -= 1;
        }
        if busy {
            frame.render_widget(
                Span::styled("Saving website…", normal.fg(theme.base_0c)),
                Rect { height: 1, ..body },
            );
            body.y += 1;
            body.height -= 1;
        }
        if let Some(error) = &self.error {
            let height = error_rows
                .min(body.height.saturating_sub(button_rows).max(1))
                .min(body.height);
            frame.render_widget(
                Paragraph::new(error.as_str())
                    .style(normal.fg(theme.base_08))
                    .wrap(Wrap { trim: true }),
                Rect { height, ..body },
            );
            body.y += height;
            body.height -= height;
        }
        let style = normal.fg(theme.base_00).bg(theme.base_0c);
        for (button, offset) in buttons.iter_mut().zip(offsets) {
            if offset.y < body.height {
                button.area = Rect::new(body.x + offset.x, body.y + offset.y, offset.width, 1);
                let (label, _) = button.display(inner_width);
                frame.render_widget(Span::styled(label, style), button.area);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_input_keeps_literal_text_on_one_line() {
        let mut prompt = WebsitePrompt::default();
        let mut seq = KeySeq::default();
        prompt.paste("https://éxample.org/\r\n\t\x1b\u{85}\u{2028}\u{2029}");
        for c in "q?jk/gg".chars() {
            let key = KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
            assert_eq!(prompt.key(key, false, &mut seq), None);
        }
        for key in [
            KeyEvent::new(KeyCode::Char('m'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT),
            KeyEvent::new(KeyCode::Char('\n'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
        ] {
            assert_eq!(prompt.key(key, false, &mut seq), None);
        }
        prompt.key(KeyCode::Left.into(), false, &mut seq);
        prompt.key(KeyCode::Backspace.into(), false, &mut seq);
        prompt.paste("&");
        assert_eq!(prompt.editor.lines(), ["https://éxample.org/q?jk/&g"]);
    }

    #[test]
    fn busy_prompt_only_allows_close() {
        let mut prompt = WebsitePrompt::default();
        let mut seq = KeySeq::default();
        prompt.paste("https://example.org");
        for (key, intent) in [
            (KeyCode::Enter.into(), WebsiteIntent::Open),
            (
                KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
                WebsiteIntent::Save,
            ),
        ] {
            assert_eq!(prompt.key(key, false, &mut seq), Some(intent));
            assert_eq!(prompt.key(key, true, &mut seq), None);
        }
        prompt.key(KeyCode::Backspace.into(), true, &mut seq);
        assert_eq!(prompt.url(), "https://example.org");
        assert_eq!(
            prompt.key(KeyCode::Esc.into(), true, &mut seq),
            Some(WebsiteIntent::Close)
        );
    }

    #[test]
    fn compact_prompt_keeps_input_or_progress_visible() {
        use ratatui::{Terminal, backend::TestBackend};

        let mut prompt = WebsitePrompt::default();
        prompt.paste("url");
        let mut terminal = Terminal::new(TestBackend::new(8, 1)).unwrap();
        for (busy, expected) in [(false, "url"), (true, "Saving")] {
            terminal.draw(|f| prompt.render(f, busy)).unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            assert!(text.contains(expected), "{text:?}");
        }
    }
}
