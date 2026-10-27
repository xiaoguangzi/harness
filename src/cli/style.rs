//! The SGR styles the terminal frontend paints text with
//! ([`Output::paint`](super::Output::paint)), by name.

pub const PLAIN: &str = "0";
pub const BOLD: &str = "1";
pub const DIM: &str = "2";
pub const RED: &str = "31";
pub const GREEN: &str = "32";
pub const YELLOW: &str = "33";
pub const CYAN: &str = "36";
pub const GRAY: &str = "90";
/// Bold magenta: the prompt's `❯`, the banner, the selected item.
pub const ACCENT: &str = "1;35";
/// Bold yellow: the mark of a question waiting for an answer.
pub const QUESTION: &str = "1;33";
