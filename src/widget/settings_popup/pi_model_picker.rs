use crate::keybindings::action::Action;
use crate::settings::RuntimeSettings;
use crate::theme::Base16Palette;
use ratatui::{
    buffer::Buffer,
    layout::{Position, Rect},
    style::{Modifier, Style},
    widgets::{Paragraph, Widget, Wrap},
};
use std::sync::mpsc::{self, Receiver, TryRecvError};

pub(super) const PI_DEFAULT: &str = "Pi default";

enum Catalog {
    Loading(Receiver<Result<Vec<String>, String>>),
    Ready(Vec<String>),
    Failed(String),
}

pub(super) struct PiModelPicker {
    catalog: Catalog,
    selected: usize,
    offset: usize,
    rows: Rect,
}

impl PiModelPicker {
    pub(super) fn open() -> Self {
        let (tx, rx) = mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("pi-models".into())
            .spawn(move || {
                let _ = tx.send(crate::pi::available());
            });
        Self {
            catalog: match worker {
                Ok(_) => Catalog::Loading(rx),
                Err(error) => Catalog::Failed(error.to_string()),
            },
            selected: 0,
            offset: 0,
            rows: Rect::ZERO,
        }
    }

    pub(super) fn poll(&mut self, settings: &RuntimeSettings) -> bool {
        let Catalog::Loading(rx) = &self.catalog else {
            return false;
        };
        self.catalog = match rx.try_recv() {
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => Catalog::Failed("Model discovery stopped".into()),
            Ok(Err(message)) => Catalog::Failed(message),
            Ok(Ok(models)) => {
                if let Some(current) = settings.load().pi_model.as_deref()
                    && let Some(index) = models.iter().position(|model| model == current)
                {
                    self.selected = 1 + index;
                }
                Catalog::Ready(models)
            }
        };
        true
    }

    // Row zero is always available, even if Pi is missing or the saved model disappeared.
    fn last_index(&self) -> usize {
        match &self.catalog {
            Catalog::Loading(_) => 0,
            Catalog::Failed(_) => 1,
            Catalog::Ready(models) => models.len().max(1),
        }
    }

    pub(super) fn scroll(&mut self, amount: isize) {
        self.selected = self
            .selected
            .saturating_add_signed(amount)
            .min(self.last_index());
    }

    // True means return to Integrations; navigation and retry keep the picker open.
    pub(super) fn handle_action(&mut self, action: Action, settings: &RuntimeSettings) -> bool {
        let page = self.rows.height.max(1) as isize;
        match action {
            Action::MoveUp => self.scroll(-1),
            Action::MoveDown => self.scroll(1),
            Action::ScrollHalfUp => self.scroll(-((page / 2).max(1))),
            Action::ScrollHalfDown => self.scroll((page / 2).max(1)),
            Action::ScrollPageUp => self.scroll(-page),
            Action::ScrollPageDown => self.scroll(page),
            Action::GoTop => self.selected = 0,
            Action::GoBottom => self.selected = self.last_index(),
            Action::Select => return self.apply(settings),
            Action::Cancel => return true,
            _ => {}
        }
        false
    }

    fn apply(&mut self, settings: &RuntimeSettings) -> bool {
        let model = if self.selected == 0 {
            None
        } else if let Catalog::Ready(models) = &self.catalog
            && let Some(model) = models.get(self.selected - 1)
        {
            Some(model.as_str())
        } else {
            *self = Self::open();
            return false;
        };
        if settings.load().pi_model.as_deref() != model {
            settings.update(|s| s.pi_model = model.map(str::to_owned));
        }
        true
    }

    pub(super) fn click(&mut self, col: u16, row: u16, settings: &RuntimeSettings) -> bool {
        if self.rows.contains(Position::new(col, row)) {
            let index = self.offset + usize::from(row - self.rows.y);
            if index <= self.last_index() {
                self.selected = index;
                return self.apply(settings);
            }
        }
        false
    }

    pub(super) fn render(
        &mut self,
        buf: &mut Buffer,
        area: Rect,
        current: Option<&str>,
        palette: &Base16Palette,
    ) {
        let heading = Style::default()
            .fg(palette.base_06)
            .add_modifier(Modifier::BOLD);
        let normal = Style::default().fg(palette.base_05);
        let title = Rect::new(area.x, area.y, area.width, 1);
        let (x, y) = buf.set_stringn(
            title.x,
            title.y,
            "Pi reader model: ",
            title.width as usize,
            heading,
        );
        if x < title.right() {
            buf.set_stringn(
                x,
                y,
                current.unwrap_or(PI_DEFAULT),
                (title.right() - x) as usize,
                heading,
            );
        }
        let (status, color, height): (&str, _, u16) = match &self.catalog {
            Catalog::Loading(_) => ("Loading models from Pi…", palette.base_04, 1),
            Catalog::Failed(error) => (error.as_str(), palette.base_08, 3),
            Catalog::Ready(models) if models.is_empty() => {
                ("Pi reported no available models.", palette.base_04, 1)
            }
            Catalog::Ready(models) => {
                let focused = self.selected.checked_sub(1).map(|i| models[i].as_str());
                let width = focused.map_or(0, unicode_width::UnicodeWidthStr::width);
                if let Some(id) = focused
                    && width > usize::from(area.width.saturating_sub(2))
                {
                    // Long IDs can share their entire visible prefix. Wrap the focused
                    // ID above the list so the user can distinguish variants.
                    let height = width.div_ceil(usize::from(area.width.max(1)));
                    (id, palette.base_04, height.min(u16::MAX as usize) as u16)
                } else {
                    ("Choose a model for reader questions:", palette.base_04, 1)
                }
            }
        };
        let status = Paragraph::new(status)
            .style(Style::default().fg(color))
            .wrap(Wrap { trim: true });
        let status_height = height.min(area.height.saturating_sub(3));
        let status_area = Rect::new(area.x, title.bottom(), area.width, status_height);
        status.render(status_area, buf);
        self.rows = Rect::new(
            area.x,
            status_area.bottom(),
            area.width,
            area.bottom().saturating_sub(status_area.bottom()),
        );
        let visible = usize::from(self.rows.height);
        self.offset = self
            .offset
            .min(self.selected)
            .max((self.selected + 1).saturating_sub(visible));
        for (line, idx) in (self.offset..=self.last_index()).take(visible).enumerate() {
            let (label, active) = if idx == 0 {
                (PI_DEFAULT, current.is_none())
            } else if let Catalog::Ready(models) = &self.catalog
                && !models.is_empty()
            {
                let id = models[idx - 1].as_str();
                (id, current == Some(id))
            } else {
                ("Retry loading models", false)
            };
            let style = if idx == self.selected {
                Style::default().fg(palette.base_00).bg(palette.base_0d)
            } else {
                normal
            };
            let row = Rect::new(area.x, self.rows.y + line as u16, area.width, 1);
            buf.set_style(row, style);
            let mark = if active { "● " } else { "  " };
            let (x, y) = buf.set_stringn(row.x, row.y, mark, row.width as usize, style);
            if x < row.right() {
                buf.set_stringn(x, y, label, (row.right() - x) as usize, style);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Settings;

    #[test]
    fn catalog_completion_requests_one_redraw_without_changing_the_saved_model() {
        let settings = RuntimeSettings::in_memory(Settings {
            pi_model: Some("two/id".into()),
            ..Settings::default()
        });
        let (tx, rx) = mpsc::channel();
        let mut picker = PiModelPicker {
            catalog: Catalog::Loading(rx),
            selected: 0,
            offset: 0,
            rows: Rect::ZERO,
        };
        assert!(!picker.poll(&settings));
        tx.send(Ok(vec!["one/id".into(), "two/id".into()])).unwrap();
        assert!(picker.poll(&settings));
        assert!(!picker.poll(&settings));
        assert_eq!(picker.selected, 2);
        picker.handle_action(Action::MoveUp, &settings);
        assert_eq!(settings.load().pi_model.as_deref(), Some("two/id"));
        assert!(picker.handle_action(Action::Select, &settings));
        assert_eq!(settings.load().pi_model.as_deref(), Some("one/id"));
        picker.handle_action(Action::GoTop, &settings);
        assert!(picker.handle_action(Action::Select, &settings));
        assert!(settings.load().pi_model.is_none());
    }

    #[test]
    fn empty_or_failed_discovery_still_offers_retry_and_default() {
        for result in [Ok(Vec::new()), Err("no Pi".into())] {
            let settings = RuntimeSettings::in_memory(Settings {
                pi_model: Some("missing/id".into()),
                ..Settings::default()
            });
            let (tx, rx) = mpsc::channel();
            let mut picker = PiModelPicker {
                catalog: Catalog::Loading(rx),
                selected: 0,
                offset: 0,
                rows: Rect::new(2, 3, 40, 6),
            };
            tx.send(result).unwrap();
            assert!(picker.poll(&settings));
            assert_eq!(picker.last_index(), 1);
            assert_eq!(settings.load().pi_model.as_deref(), Some("missing/id"));
            assert!(picker.click(3, 3, &settings));
            assert!(settings.load().pi_model.is_none());
        }
    }
}
