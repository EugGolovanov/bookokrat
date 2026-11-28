use super::{App, FocusedPanel, PopupWindow};
use crate::website::save_website;
use crate::widget::website_prompt::{WebsiteIntent, WebsitePrompt};
use crossterm::event::KeyEvent;
use std::sync::mpsc::{self, TryRecvError};

impl App {
    pub(super) fn open_website_prompt(&mut self) {
        let FocusedPanel::Main(panel) = self.focused_panel else {
            return;
        };
        if self.website_job.is_some() {
            self.show_info("An article is still saving to the library");
            return;
        }
        self.previous_main_panel = panel;
        self.website_prompt = Some(WebsitePrompt::default());
        self.focused_panel = FocusedPanel::Popup(PopupWindow::Website);
        self.key_sequence.clear();
    }

    fn close_website_prompt(&mut self) {
        self.website_prompt = None;
        self.key_sequence.clear();
        self.close_popup_to_previous();
    }

    pub(super) fn website_key(&mut self, key: KeyEvent) {
        let Some(prompt) = self.website_prompt.as_mut() else {
            return;
        };
        let Some(intent) = prompt.key(key, self.website_job.is_some(), &mut self.key_sequence)
        else {
            return;
        };
        self.website_intent(intent);
    }

    pub(super) fn website_click(&mut self, col: u16, row: u16) {
        if let Some(intent) = self
            .website_prompt
            .as_ref()
            .and_then(|prompt| prompt.click(col, row, self.website_job.is_some()))
        {
            self.website_intent(intent);
        }
    }

    fn website_intent(&mut self, intent: WebsiteIntent) {
        self.key_sequence.clear();
        let open = match intent {
            WebsiteIntent::Open => true,
            WebsiteIntent::Save => false,
            WebsiteIntent::Close => {
                self.close_website_prompt();
                return;
            }
        };
        if self.website_job.is_some() {
            return;
        }
        let Some(prompt) = &mut self.website_prompt else {
            return;
        };
        let url = prompt.url().trim().to_owned();
        let dir = self.book_manager.library_directory().to_owned();
        let (tx, rx) = mpsc::channel();
        match std::thread::Builder::new()
            .name("website-import".into())
            .spawn(move || {
                let _ = tx.send((save_website(&url, &dir, None), open));
            }) {
            Ok(_) => {
                prompt.started_saving();
                self.website_job = Some(rx);
            }
            Err(e) => prompt.error = Some(format!("Cannot start import: {e}")),
        }
    }

    /// Applies a completed import without blocking the event loop.
    pub fn poll_website_import(&mut self) -> bool {
        let Some(job) = &self.website_job else {
            return false;
        };
        let (result, open) = match job.try_recv() {
            Ok(done) => done,
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => (
                Err(anyhow::anyhow!("Article import stopped unexpectedly")),
                false,
            ),
        };
        self.website_job = None;
        match result {
            Ok(path) => {
                self.book_manager.refresh_in_background();
                let visible = self.website_prompt.is_some();
                if visible {
                    self.close_website_prompt();
                }
                if visible
                    && open
                    && let Err(e) =
                        self.open_book_for_reading_by_path(&path.to_string_lossy(), None)
                {
                    self.show_error(format!(
                        "Saved {}, but could not open it: {e:#}",
                        path.display()
                    ));
                } else {
                    let name = path.file_name().unwrap_or(path.as_os_str());
                    self.show_info(format!("Saved article: {}", name.to_string_lossy()));
                }
            }
            Err(e) => {
                let message = format!("{e:#}");
                if let Some(prompt) = &mut self.website_prompt {
                    prompt.error = Some(message);
                } else {
                    self.show_error(format!("Could not save article: {message}"));
                }
            }
        }
        true
    }
}
