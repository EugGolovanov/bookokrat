use std::ops::Range;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::Span,
    widgets::{Block, Clear, Paragraph, Wrap},
};
use unicode_width::UnicodeWidthStr;

use crate::{
    assistant::{Mode, Update},
    keybindings::{action::Action, context::KeyContext, keymap},
    theme::Base16Palette,
    vendored::tui_textarea::TextArea,
};

pub(crate) enum Intent {
    Submit {
        mode: Mode,
        prompt: String,
        quote: Option<String>,
    },
    Cancel,
    Approve(bool),
}

fn unsafe_char(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{206f}' | '\u{feff}')
}

// Never let document, subprocess or pasted text write terminal controls. For
// approvals, also make look-alike/invisible Unicode and literal escapes distinct.
fn append_visible(out: &mut String, value: &str, approval: bool) {
    let mut plain = 0;
    for (index, c) in value.char_indices() {
        if c == '\n' || (!unsafe_char(c) && (!approval || (c.is_ascii() && c != '\\'))) {
            continue;
        }
        out.push_str(&value[plain..index]);
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\r' if !approval && value.as_bytes().get(index + 1) == Some(&b'\n') => (),
            '\r' => out.push_str("\\r"),
            _ => out.extend(c.escape_unicode()),
        }
        plain = index + c.len_utf8();
    }
    out.push_str(&value[plain..]);
}

/// Byte ranges into append-only text. Streaming reflows only the last visual
/// row plus new bytes. A resize is the only reason to reflow the whole log.
#[derive(Default)]
struct Wrapped {
    text: String,
    rows: Vec<Range<usize>>,
    width: u16,
    laid_out: usize,
}

impl Wrapped {
    fn layout(&mut self, width: u16) {
        if width != self.width {
            self.width = width;
            self.rows.clear();
        } else if !self.rows.is_empty() && self.laid_out == self.text.len() {
            return;
        }
        let mut start = self.rows.pop().map_or(0, |row| row.start);
        let mut byte = start;
        for line in self.text[start..].split('\n') {
            let span = Span::raw(line);
            let mut cells = 0;
            for grapheme in span.styled_graphemes(Style::default()) {
                let size = grapheme.symbol.width();
                if cells > 0 && cells + size > usize::from(width) {
                    self.rows.push(start..byte);
                    start = byte;
                    cells = 0;
                }
                byte += grapheme.symbol.len();
                cells += size;
            }
            self.rows.push(start..byte);
            byte += 1;
            start = byte;
        }
        self.laid_out = self.text.len();
    }

    fn line(&self, n: usize) -> &str {
        &self.text[self.rows[n].clone()]
    }
}

struct Approval {
    details: Wrapped,
    top: usize,
    reviewed: usize,
    visible_and_complete: bool,
    allow: bool,
}

#[derive(Default)]
struct Hints {
    send: String,
    quote: String,
    working: String,
    review: String,
    decision: String,
}

impl Hints {
    fn load() -> Self {
        let km = keymap();
        let label = |action| {
            km.describe_binding_display(KeyContext::Assistant, &action)
                .unwrap_or_else(|| "unbound".into())
        };
        Self {
            send: format!("{} send", label(Action::AssistantSubmit)),
            quote: format!(
                "Quote attached · {} remove",
                label(Action::AssistantRemoveQuote)
            ),
            working: format!("Working · {} cancel", label(Action::AssistantCancel)),
            review: format!(
                "Review every page: {} / {}",
                label(Action::ScrollPageUp),
                label(Action::ScrollPageDown)
            ),
            decision: format!(
                "Deny (default): {} or {}\nApprove: {}",
                label(Action::AssistantSubmit),
                label(Action::AssistantDeny),
                label(Action::AssistantApprove)
            ),
        }
    }
}

#[derive(Default)]
pub(crate) struct Assistant {
    editor: TextArea<'static>,
    quote: Option<Wrapped>,
    history: Wrapped,
    active: Option<Mode>,
    canceling: bool,
    streaming: bool,
    approval: Option<Approval>,
    // None follows incoming text. Manual scrolling stays anchored to the document.
    top: Option<usize>,
    show_log_end: bool,
    page: usize,
    max_top: usize,
    hints: Hints,
}

fn parse_prompt(draft: &str) -> (Mode, &str) {
    let prompt = draft.trim();
    match prompt.strip_prefix("/agent") {
        Some(rest) if rest.is_empty() || rest.starts_with(char::is_whitespace) => {
            (Mode::Agent, rest.trim_start())
        }
        _ => (Mode::Chat, prompt),
    }
}

impl Assistant {
    pub(crate) fn open(&mut self, quote: Option<String>) {
        self.hints = Hints::load();
        if !self.busy() && self.editor.is_empty() {
            self.quote = quote.filter(|q| !q.trim().is_empty()).map(|q| {
                let mut passage = Wrapped::default();
                if q.chars().any(|c| c != '\n' && unsafe_char(c)) {
                    append_visible(&mut passage.text, &q, false);
                } else {
                    passage.text = q;
                }
                passage
            });
            self.top = None;
        }
        if let Some(a) = &mut self.approval {
            a.visible_and_complete = false;
        }
    }

    pub(crate) fn busy(&self) -> bool {
        self.active.is_some()
    }
    pub(crate) fn reviewing(&self) -> bool {
        self.approval.is_some()
    }

    pub(crate) fn key(&mut self, key: KeyEvent) -> Option<Intent> {
        if self.busy() || key.kind == KeyEventKind::Release {
            return None;
        }
        match key.code {
            KeyCode::Char(c)
                if !unsafe_char(c) && (key.modifiers - KeyModifiers::SHIFT).is_empty() =>
            {
                self.editor.insert_char(c);
            }
            KeyCode::Left
            | KeyCode::Right
            | KeyCode::Up
            | KeyCode::Down
            | KeyCode::Home
            | KeyCode::End
            | KeyCode::Delete
            | KeyCode::Backspace
            | KeyCode::Enter => {
                self.editor.input(key);
            }
            _ => (),
        }
        None
    }

    pub(crate) fn paste(&mut self, value: &str) {
        if self.busy() || value.is_empty() {
            return;
        }
        if value.chars().any(|c| c != '\n' && unsafe_char(c)) {
            let mut clean = String::with_capacity(value.len());
            append_visible(&mut clean, value, false);
            self.editor.insert_str(clean);
        } else {
            self.editor.insert_str(value);
        }
    }

    fn entry(&mut self, label: &str) {
        if !self.history.text.is_empty() {
            self.history.text.push_str("\n\n");
        }
        self.history.text.push_str(label);
        self.streaming = false;
    }

    pub(crate) fn action(&mut self, action: Action) -> Option<Intent> {
        match action {
            Action::AssistantCancel if self.busy() && !self.canceling => {
                self.canceling = true;
                self.approval = None;
                return Some(Intent::Cancel);
            }
            Action::AssistantApprove | Action::AssistantDeny | Action::AssistantSubmit
                if self.approval.is_some() =>
            {
                let yes = action == Action::AssistantApprove;
                if yes
                    && !self
                        .approval
                        .as_ref()
                        .is_some_and(|a| a.allow && a.visible_and_complete)
                {
                    return None;
                }
                self.approval = None;
                self.entry(if yes {
                    "Action approved."
                } else {
                    "Action denied."
                });
                return Some(Intent::Approve(yes));
            }
            Action::ScrollPageUp
            | Action::ScrollPageDown
            | Action::ScrollUp
            | Action::ScrollDown => {
                let up = matches!(action, Action::ScrollPageUp | Action::ScrollUp);
                let step = if matches!(action, Action::ScrollPageUp | Action::ScrollPageDown) {
                    self.page.max(1)
                } else {
                    1
                };
                if let Some(a) = &mut self.approval {
                    a.top = if up {
                        a.top.saturating_sub(step)
                    } else {
                        a.top.saturating_add(step).min(a.reviewed)
                    };
                    a.visible_and_complete = false;
                } else {
                    let here = self.top.unwrap_or(self.max_top);
                    let next = if up {
                        here.saturating_sub(step)
                    } else {
                        here.saturating_add(step).min(self.max_top)
                    };
                    self.top = (next < self.max_top).then_some(next);
                }
            }
            Action::AssistantRemoveQuote if !self.busy() => self.quote = None,
            Action::AssistantSubmit if !self.busy() => {
                let draft = self.editor.lines().join("\n");
                let (mode, prompt) = parse_prompt(&draft);
                if prompt.is_empty() {
                    if mode == Mode::Agent {
                        self.entry("Add an instruction after /agent.");
                        self.show_log_end = true;
                    }
                    return None;
                }
                self.entry(if mode == Mode::Agent {
                    "You · Agent\n"
                } else {
                    "You · Chat\n"
                });
                self.history.text.push_str(prompt);
                let quote = self.quote.as_ref().map(|q| q.text.clone());
                if let Some(q) = &quote {
                    self.history.text.push_str("\nQuoted passage:\n");
                    self.history.text.push_str(q);
                }
                // Keep the original editor (including cursor) and attachment until
                // success. A launch, auth, or stream failure needs no reconstruction.
                self.active = Some(mode);
                self.top = None;
                self.show_log_end = false;
                let prompt = if prompt.len() == draft.len() {
                    draft
                } else {
                    prompt.to_owned()
                };
                return Some(Intent::Submit {
                    mode,
                    prompt,
                    quote,
                });
            }
            _ => (),
        }
        None
    }

    pub(crate) fn update(&mut self, event: Update) {
        match event {
            Update::Text(chunk) if self.busy() && !chunk.is_empty() => {
                if !self.streaming {
                    self.entry("Assistant\n");
                    self.streaming = true;
                }
                append_visible(&mut self.history.text, &chunk, false);
            }
            Update::Status(status) => {
                if status == "Approval withdrawn" {
                    self.approval = None;
                }
                self.entry("OMP · ");
                append_visible(&mut self.history.text, &status, false);
            }
            Update::Approval { description } if self.busy() && !self.canceling => {
                let allow = !description.trim().is_empty() && self.active == Some(Mode::Agent);
                let mut details = Wrapped::default();
                details.text.push_str(&self.hints.review);
                details.text.push_str("\n\nRequested action:\n");
                append_visible(&mut details.text, &description, true);
                details.text.push_str("\n\n");
                if !allow {
                    details.text.push_str("Approval unavailable.\n");
                }
                details.text.push_str(&self.hints.decision);
                self.approval = Some(Approval {
                    details,
                    top: 0,
                    reviewed: 0,
                    visible_and_complete: false,
                    allow,
                });
            }
            Update::Done | Update::Failed(_) => {
                if let Update::Failed(reason) = event {
                    self.entry("Request failed · ");
                    append_visible(&mut self.history.text, &reason, false);
                    self.show_log_end = true;
                } else if self.canceling {
                    self.entry("Request canceled.");
                } else {
                    self.editor = TextArea::default();
                    self.quote = None;
                }
                self.approval = None;
                self.active = None;
                self.canceling = false;
                self.streaming = false;
            }
            _ => (),
        }
    }

    pub(crate) fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        focused: bool,
        palette: &Base16Palette,
    ) {
        let (_, border, bg) = palette.get_panel_colors(focused);
        let normal = Style::default().fg(palette.base_05).bg(bg);
        let accent = Style::default().fg(palette.base_0c).bg(bg);
        let block = Block::bordered()
            .style(normal)
            .border_style(Style::default().fg(border))
            .title(if self.approval.is_some() {
                " Agent approval "
            } else {
                " Assistant "
            });
        let inner = block.inner(area);
        frame.render_widget(Clear, area);
        frame.render_widget(block, area);
        if let Some(a) = &mut self.approval {
            a.visible_and_complete = false;
            if inner.is_empty() {
                return;
            }
            if a.details.width != inner.width {
                a.top = 0;
                a.reviewed = 0;
            }
            a.details.layout(inner.width);
            self.page = usize::from(inner.height);
            a.top = a.top.min(a.details.rows.len().saturating_sub(self.page));
            let end = (a.top + self.page).min(a.details.rows.len());
            for row in a.top..end {
                draw_line(frame, inner, row - a.top, a.details.line(row), normal);
            }
            if focused && a.top <= a.reviewed {
                a.reviewed = a.reviewed.max(end);
                a.visible_and_complete = end == a.details.rows.len() && a.reviewed == end;
            }
            return;
        }
        if inner.is_empty() {
            return;
        }
        let busy = self.busy();
        let mut quote = self.quote.as_mut().filter(|_| !busy);
        let composer_height = if busy {
            0
        } else {
            self.editor
                .lines()
                .len()
                .clamp(1, 3)
                .min(usize::from(inner.height.saturating_sub(1))) as u16
        };
        let body_height = inner.height - composer_height - 1;
        self.page = usize::from(body_height);
        self.history.layout(inner.width);
        let log_len = if self.history.text.is_empty() {
            0
        } else {
            self.history.rows.len()
        };
        let rows = log_len
            + quote.as_deref_mut().map_or(0, |q| {
                q.layout(inner.width);
                1 + q.rows.len()
            });
        self.max_top = rows.saturating_sub(self.page);
        if std::mem::take(&mut self.show_log_end) {
            self.top = Some(log_len.saturating_sub(self.page));
        }
        let top = self.top.unwrap_or(self.max_top).min(self.max_top);
        if self.top.is_some() {
            self.top = Some(top);
        }
        if rows == 0 && body_height > 0 {
            frame.render_widget(
                Paragraph::new(
                    "Plain questions start fresh.\n/agent continues this book’s session.",
                )
                .wrap(Wrap { trim: true })
                .style(normal),
                Rect::new(inner.x, inner.y, inner.width, body_height),
            );
        }
        for row in top..(top + self.page).min(rows) {
            let (text, style) = if row < log_len {
                (self.history.line(row), normal)
            } else if row == log_len {
                ("Selected passage:", accent)
            } else if let Some(q) = &quote {
                (
                    q.line(row - log_len - 1),
                    Style::default().fg(palette.base_0a),
                )
            } else {
                break;
            };
            draw_line(frame, inner, row - top, text, style);
        }
        let hint = if self.canceling {
            "Canceling…"
        } else if busy {
            &self.hints.working
        } else if self.quote.is_some() {
            &self.hints.quote
        } else {
            &self.hints.send
        };
        draw_line(frame, inner, self.page, hint, accent);
        if composer_height > 0 {
            self.editor.set_style(normal);
            self.editor.set_cursor_line_style(normal);
            self.editor.set_cursor_style(if focused {
                normal.add_modifier(Modifier::REVERSED)
            } else {
                normal
            });
            frame.render_widget(
                &self.editor,
                Rect::new(
                    inner.x,
                    inner.y + body_height + 1,
                    inner.width,
                    composer_height,
                ),
            );
        }
    }
}

fn draw_line(frame: &mut Frame, area: Rect, offset: usize, text: &str, style: Style) {
    frame.buffer_mut().set_stringn(
        area.x,
        area.y + offset as u16,
        text,
        usize::from(area.width),
        style,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keybindings::{
        defaults::default_keymap, keymap::LookupResult, notation::parse_key_binding,
    };
    use crate::test_utils::test_helpers::create_test_terminal;
    use std::collections::HashSet;

    #[test]
    fn assistant_default_bindings_drive_composer_and_review() {
        let map = default_keymap();
        let bindings = map.context(KeyContext::Assistant).unwrap();
        let mut covered = HashSet::new();
        // App dispatches focus and dismissal; see keybinding_actions.rs.
        for (notation, action) in [("<Tab>", Action::SwitchFocus), ("<Esc>", Action::Cancel)] {
            let key = parse_key_binding(notation).unwrap();
            assert_eq!(bindings.lookup(key.keys()), LookupResult::Found(action));
            covered.insert(key);
        }
        let mut press = |a: &mut Assistant, notation: &str| {
            let key = parse_key_binding(notation).unwrap();
            let LookupResult::Found(action) = bindings.lookup(key.keys()) else {
                panic!("unbound {notation}");
            };
            covered.insert(key);
            a.action(action)
        };
        let mut a = Assistant::default();
        a.open(Some("first quote".into()));
        a.paste("/agentic");
        a.open(Some("different selection".into()));
        assert_eq!(a.quote.as_ref().unwrap().text, "first quote");
        assert!(press(&mut a, "<C-x>").is_none());
        assert!(
            matches!(press(&mut a, "<CR>"), Some(Intent::Submit { mode: Mode::Chat, prompt, quote: None }) if prompt == "/agentic")
        );
        assert!(press(&mut a, "<CR>").is_none());
        assert!(matches!(press(&mut a, "<C-c>"), Some(Intent::Cancel)));
        a.update(Update::Failed("offline".into()));
        assert!(!a.busy());
        assert_eq!(a.editor.lines(), ["/agentic"]);

        let mut a = Assistant::default();
        a.open(Some("book passage".into()));
        a.paste("/agent\nCheck this.");
        assert!(
            matches!(press(&mut a, "<CR>"), Some(Intent::Submit { mode: Mode::Agent, prompt, quote: Some(q) }) if prompt == "Check this." && q == "book passage")
        );
        let mut term = create_test_terminal(32, 7);
        let mut draw = |a: &mut Assistant, focused| {
            term.draw(|f| {
                let area = f.area();
                a.render(f, area, focused, crate::theme::current_theme());
            })
            .unwrap();
        };
        a.update(Update::Approval {
            description: format!("{}tail\\\x1b\u{202e}", "line\n".repeat(20)),
        });
        assert!(press(&mut a, "<C-y>").is_none()); // Nothing drawn yet.
        draw(&mut a, true);
        assert!(
            a.approval
                .as_ref()
                .unwrap()
                .details
                .text
                .contains(r"tail\\\u{1b}\u{202e}")
        );
        assert!(press(&mut a, "<C-y>").is_none()); // Only the first page drawn.
        press(&mut a, "<PageDown>");
        draw(&mut a, true);
        assert!(a.approval.as_ref().unwrap().top > 0);
        press(&mut a, "<PageUp>");
        draw(&mut a, true);
        assert_eq!(a.approval.as_ref().unwrap().top, 0);
        for _ in 0..12 {
            press(&mut a, "<PageDown>");
            draw(&mut a, true);
            if a.approval.as_ref().unwrap().visible_and_complete {
                break;
            }
        }
        draw(&mut a, false);
        assert!(press(&mut a, "<C-y>").is_none()); // Losing focus disarms approval.
        draw(&mut a, true);
        assert!(matches!(
            press(&mut a, "<C-y>"),
            Some(Intent::Approve(true))
        ));
        for key in ["<CR>", "<C-n>"] {
            a.update(Update::Approval {
                description: "read file".into(),
            });
            draw(&mut a, true);
            assert!(matches!(press(&mut a, key), Some(Intent::Approve(false))));
        }
        a.update(Update::Done);
        assert!(!a.busy());
        assert!(a.editor.is_empty() && a.quote.is_none());

        assert_eq!(
            covered,
            bindings
                .all_bindings()
                .into_iter()
                .map(|(key, _)| key)
                .collect()
        );
    }
}
