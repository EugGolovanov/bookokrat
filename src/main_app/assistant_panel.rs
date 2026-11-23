use super::{App, FocusedPanel, MainPanel, PopupWindow};
use crate::assistant::{Request, Runtime, Update};
use crate::keybindings::{
    action::Action, context::KeyContext, keymap::LookupResult, notation::key_event_to_input,
};
use crate::theme::current_theme;
use crate::widget::assistant::{Assistant, Intent};
use crossterm::event::{KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::Span,
};
use std::path::{Path, PathBuf};

#[derive(Default)]
pub(super) struct ReaderAssistant {
    book: Option<PathBuf>,
    pub(super) sidebar: bool,
    return_focus: Option<MainPanel>,
    pub(super) widget: Assistant,
    runtime: Runtime,
}

impl ReaderAssistant {
    pub(super) fn change_book(&mut self, path: Option<&Path>) {
        if self.book.as_deref() != path {
            self.runtime.leave_book();
            self.book = path.map(Path::to_owned);
            self.sidebar = false;
            self.return_focus = None;
            self.widget = Assistant::default();
        }
    }
}

// Both tabs occupy the bottom border, so adding the assistant never moves a TOC row.
fn tabs(area: Rect) -> [(Rect, &'static str); 2] {
    let width = area.width.saturating_sub(2);
    let (contents, assistant) = if width >= 20 {
        (" Contents ", " Assistant")
    } else {
        ("TOC", " Ask")
    };
    let left = (contents.len() as u16).min(width);
    let x = area.x.saturating_add(1);
    let y = area.bottom().saturating_sub(1);
    [
        (Rect::new(x, y, left, 1), contents),
        (
            Rect::new(
                x.saturating_add(left),
                y,
                (assistant.len() as u16).min(width - left),
                1,
            ),
            assistant,
        ),
    ]
}

impl App {
    pub(super) fn reader_selection(&self) -> Option<String> {
        #[cfg(feature = "pdf")]
        if let Some(reader) = &self.pdf_reader {
            return reader.get_selected_text();
        }
        self.text_reader.get_selected_text()
    }

    pub(super) fn assistant_overlay(&self) -> bool {
        self.focused_panel == FocusedPanel::Popup(PopupWindow::Assistant)
    }

    pub(super) fn assistant_visible(&self) -> bool {
        self.assistant_overlay()
            || (self.assistant.sidebar
                && !self.zen_mode
                && !self.navigation_panel.is_in_book_mode())
    }

    pub(super) fn assistant_focused(&self) -> bool {
        self.assistant_overlay()
            || (self.assistant_visible() && self.is_main_panel(MainPanel::NavigationList))
    }

    fn open_assistant(&mut self) {
        let FocusedPanel::Main(previous) = self.focused_panel else {
            return;
        };
        if self.assistant.book.is_none()
            || (!self.zen_mode && self.navigation_panel.is_in_book_mode())
        {
            return;
        }
        let quote = self.reader_selection().filter(|s| !s.trim().is_empty());
        self.assistant.widget.open(quote);
        self.assistant.return_focus = Some(previous);
        self.key_sequence.clear();
        if self.zen_mode {
            self.focused_panel = FocusedPanel::Popup(PopupWindow::Assistant);
        } else {
            self.assistant.sidebar = true;
            self.set_main_panel_focus(MainPanel::NavigationList);
        }
    }

    fn close_assistant(&mut self) {
        if self.assistant_overlay() {
            self.set_main_panel_focus(MainPanel::Content);
        } else {
            self.assistant.sidebar = false;
            self.set_main_panel_focus(self.assistant.return_focus.unwrap_or(MainPanel::Content));
        }
        self.key_sequence.clear();
    }

    pub(super) fn focus_navigation(&mut self) {
        if self.assistant_visible() {
            self.open_assistant();
        } else {
            self.set_main_panel_focus(MainPanel::NavigationList);
        }
    }

    pub(super) fn toggle_assistant(&mut self) {
        if self.assistant_focused() {
            self.close_assistant();
        } else {
            self.open_assistant();
        }
    }

    pub(super) fn assistant_key(&mut self, key: KeyEvent) {
        let input = key_event_to_input(&key);
        let found = if self.key_sequence.is_empty() {
            crate::keybindings::keymap().lookup(KeyContext::Assistant, &[input])
        } else {
            let mut seq: Vec<_> = self
                .key_sequence
                .keys()
                .iter()
                .map(key_event_to_input)
                .collect();
            seq.push(input);
            crate::keybindings::keymap().lookup(KeyContext::Assistant, &seq)
        };
        let intent = match found {
            LookupResult::Found(action) => {
                self.key_sequence.clear();
                match action {
                    Action::ToggleAssistant | Action::Cancel => {
                        self.close_assistant();
                        return;
                    }
                    Action::SwitchFocus => {
                        if self.assistant_overlay() {
                            self.close_assistant();
                        } else {
                            self.set_main_panel_focus(MainPanel::Content);
                        }
                        return;
                    }
                    action => self.assistant.widget.action(action),
                }
            }
            LookupResult::Prefix => {
                self.key_sequence.push(key);
                return;
            }
            LookupResult::NoMatch => {
                self.key_sequence.clear();
                self.assistant.widget.key(key)
            }
        };
        match intent {
            Some(Intent::Submit {
                mode,
                prompt,
                quote,
            }) => {
                if let Some(book) = self.assistant.book.clone()
                    && let Err(error) = self.assistant.runtime.submit(Request {
                        book,
                        mode,
                        pi_model: match mode {
                            crate::assistant::Mode::Chat => self.settings.load().pi_model.clone(),
                            crate::assistant::Mode::Agent => None,
                        },
                        prompt,
                        quote,
                    })
                {
                    self.assistant.widget.update(Update::Failed(error));
                }
            }
            Some(Intent::Cancel) => self.assistant.runtime.cancel(),
            Some(Intent::Approve(yes)) => self.assistant.runtime.approve(yes),
            None => {}
        }
    }

    pub(super) fn poll_assistant(&mut self) -> bool {
        let mut changed = false;
        // Bound work per tick even if the model is producing faster than the TUI can draw.
        for _ in 0..256 {
            let Some(update) = self.assistant.runtime.poll() else {
                break;
            };
            if matches!(update, Update::Approval { .. }) && !self.assistant_focused() {
                self.show_info("Assistant needs your approval. Open the Assistant tab to review.");
                changed = true;
            }
            self.assistant.widget.update(update);
            changed |= self.assistant_visible();
        }
        changed
    }

    pub(super) fn assistant_overlay_area(&self) -> Rect {
        let screen = self.terminal_size;
        let width = screen
            .width
            .saturating_sub(4)
            .min(84)
            .max(screen.width.min(30));
        let height = screen.height.saturating_sub(4).max(screen.height.min(12));
        Rect::new(
            screen.x + (screen.width - width) / 2,
            screen.y + (screen.height - height) / 2,
            width,
            height,
        )
    }

    pub(super) fn render_navigation(&mut self, frame: &mut Frame, area: Rect) {
        let focused = self.is_main_panel(MainPanel::NavigationList);
        let palette = current_theme();
        if self.assistant_visible() {
            self.assistant.widget.render(frame, area, focused, palette);
        } else {
            self.navigation_panel
                .render(frame, area, focused, palette, &self.book_manager);
        }
        if self.navigation_panel.is_in_book_mode() || area.height < 2 {
            return;
        }
        let active = usize::from(self.assistant.sidebar);
        for (index, (tab, label)) in tabs(area).into_iter().enumerate() {
            let style = if index == active {
                Style::default()
                    .fg(palette.base_0d)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
            } else {
                Style::default().fg(palette.base_04)
            };
            let style = if index == 1 && self.assistant.widget.busy() {
                style.fg(palette.base_0a)
            } else {
                style
            };
            frame.render_widget(Span::styled(label, style), tab);
        }
    }

    pub(super) fn handle_assistant_mouse(&mut self, event: MouseEvent) -> bool {
        let point = (event.column, event.row).into();
        let down = event.kind == MouseEventKind::Down(MouseButton::Left);
        if self.assistant_overlay() {
            if down && !self.assistant_overlay_area().contains(point) {
                self.close_assistant();
            }
            return true;
        }
        if self.zen_mode
            || self.has_active_popup()
            || self.resizing_nav_panel
            || self.navigation_panel.is_in_book_mode()
            || event.column >= self.nav_panel_width().saturating_sub(1)
        {
            return false;
        }
        let area = self.get_navigation_panel_area();
        if !area.contains(point) {
            return false;
        }
        if down {
            let [(contents, _), (assistant, _)] = tabs(area);
            if assistant.contains(point) {
                if !self.assistant_focused() {
                    self.open_assistant();
                }
                return true;
            }
            if contents.contains(point) {
                self.assistant.sidebar = false;
                self.key_sequence.clear();
                self.set_main_panel_focus(MainPanel::NavigationList);
                return true;
            }
            if self.assistant.sidebar && !self.assistant_focused() {
                self.open_assistant();
            }
        }
        self.assistant.sidebar
    }

    pub(super) fn scroll_assistant(&mut self, amount: i32, column: u16) -> bool {
        if !self.assistant_overlay()
            && (self.has_active_popup()
                || !self.assistant_visible()
                || column >= self.nav_panel_width())
        {
            return false;
        }
        self.assistant.widget.action(if amount > 0 {
            Action::ScrollPageDown
        } else {
            Action::ScrollPageUp
        });
        true
    }

    pub(super) fn assistant_help(&self) -> String {
        let key = |action| Self::key_for(KeyContext::Assistant, action);
        if self.assistant.widget.reviewing() {
            return format!(
                "{}: Deny | {}: Approve once | {}: Next page",
                key(Action::AssistantSubmit),
                key(Action::AssistantApprove),
                key(Action::ScrollPageDown)
            );
        }
        if self.assistant.widget.busy() {
            return format!(
                "{}: Cancel request | {}: Hide assistant",
                key(Action::AssistantCancel),
                key(Action::Cancel)
            );
        }
        format!(
            "{}: Send | {}: Cancel request | {}: Remove quote | {}: Contents",
            key(Action::AssistantSubmit),
            key(Action::AssistantCancel),
            key(Action::AssistantRemoveQuote),
            key(Action::Cancel)
        )
    }
}
