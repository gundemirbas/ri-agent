use ratatui_textarea::TextArea;

/// State owned by the shell input mode.
///
/// ri is Linux-only and always runs commands through `sh`, so there is no
/// shell selection — this struct only owns the shell-mode textarea.
pub struct ShellState {
    pub(crate) textarea: TextArea<'static>,
}

impl ShellState {
    pub fn new() -> Self {
        Self {
            textarea: Self::make_textarea(),
        }
    }

    pub(crate) fn make_textarea() -> TextArea<'static> {
        TextArea::default()
    }

    pub fn reset_textarea(&mut self) {
        self.textarea = Self::make_textarea();
    }

    pub fn input_is_empty(&self) -> bool {
        self.textarea
            .lines()
            .iter()
            .all(|line| line.trim().is_empty())
    }
}

impl Default for ShellState {
    fn default() -> Self {
        Self::new()
    }
}
