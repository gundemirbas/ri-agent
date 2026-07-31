#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellKind {
    Bash,
}

impl ShellKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Bash => "bash",
        }
    }

    pub fn prompt_char(self) -> char {
        match self {
            Self::Bash => '$',
        }
    }
}

pub fn discover_available_shells() -> Vec<ShellKind> {
    vec![ShellKind::Bash]
}
