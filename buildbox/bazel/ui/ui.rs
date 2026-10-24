//! Bazel's terminal output: messages such as `INFO: ...`, and on a
//! terminal, the progress bar Bazel redraws beneath them. Everything here
//! renders text; writing it to the client is the caller's job.

mod bar;
mod screen;
mod summary;

pub use bar::{Phase, progress_bar};
pub use screen::Screen;
pub use summary::{analyzed, processes, total_actions};

/// The client's terminal, as far as output cares.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Terminal {
    /// Whether stderr is a terminal.
    pub interactive: bool,
    /// Its width in columns, or 0 if unknown.
    pub columns: u32,
    /// The client's `TERM`.
    pub term: Option<String>,
}

impl Terminal {
    /// Whether the terminal understands colours and cursor movement.
    fn is_capable(&self) -> bool {
        self.interactive && self.term.as_deref() != Some("dumb")
    }
}

/// How output looks, from Bazel's `--color`, `--curses` and
/// `--[no]show_progress`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UiOptions {
    pub color: bool,
    /// Whether the progress bar is redrawn in place, rather than printed as
    /// a line now and then.
    pub cursor_control: bool,
    pub show_progress: bool,
    /// How wide lines may be when the progress bar is redrawn in place.
    pub columns: usize,
}

/// The width Bazel assumes for a terminal whose width it doesn't know.
const DEFAULT_COLUMNS: usize = 80;

impl UiOptions {
    /// The options for `terminal`, given the values of `--color` and
    /// `--curses` (`yes`, `no` or `auto`, the default) and
    /// `--[no]show_progress`.
    pub fn new(terminal: &Terminal, color: Option<&str>, curses: Option<&str>, show_progress: bool) -> UiOptions {
        let columns = match terminal.columns {
            0 => DEFAULT_COLUMNS,
            columns => columns as usize,
        };
        UiOptions {
            color: tristate(color, terminal.is_capable()),
            cursor_control: tristate(curses, terminal.is_capable()),
            show_progress,
            columns,
        }
    }

    /// Options for when nothing is known: no colour, no cursor control.
    pub fn plain() -> UiOptions {
        UiOptions::new(&Terminal::default(), None, None, true)
    }
}

/// A `yes`/`no`/`auto` option's value, `auto` being `detected`.
fn tristate(value: Option<&str>, detected: bool) -> bool {
    match value {
        Some("yes" | "true" | "1") => true,
        Some("no" | "false" | "0") => false,
        _ => detected,
    }
}

/// What kind of message an event is, which is how Bazel prefixes and
/// colours it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Info,
    Warning,
    Error,
}

impl Kind {
    fn prefix(self) -> &'static str {
        match self {
            Kind::Info => "INFO: ",
            Kind::Warning => "WARNING: ",
            Kind::Error => "ERROR: ",
        }
    }

    fn color(self) -> &'static str {
        match self {
            Kind::Info => GREEN,
            Kind::Warning => MAGENTA,
            Kind::Error => RED_BOLD,
        }
    }
}

const GREEN: &str = "\x1b[32m";
const MAGENTA: &str = "\x1b[35m";
const RED_BOLD: &str = "\x1b[31m\x1b[1m";
const RESET: &str = "\x1b[0m";

/// `text` in `color`, if colours are on.
fn colored(options: &UiOptions, color: &str, text: &str) -> String {
    match options.color {
        true => format!("{color}{text}{RESET}"),
        false => text.to_string(),
    }
}

/// A count as Bazel prints it, with commas between thousands.
pub fn format_count(count: u64) -> String {
    let digits = count.to_string();
    let mut text = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            text.push(',');
        }
        text.push(digit);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_have_commas_between_thousands() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1000), "1,000");
        assert_eq!(format_count(1234567), "1,234,567");
    }

    #[test]
    fn auto_follows_the_terminal() {
        let terminal = Terminal {
            interactive: true,
            columns: 120,
            term: Some("xterm-256color".to_string()),
        };
        let options = UiOptions::new(&terminal, None, Some("no"), true);
        assert!(options.color);
        assert!(!options.cursor_control);
        assert_eq!(options.columns, 120);

        let dumb = Terminal {
            term: Some("dumb".to_string()),
            ..terminal
        };
        assert!(!UiOptions::new(&dumb, None, None, true).color);
        assert!(UiOptions::new(&Terminal::default(), Some("yes"), None, true).color);
    }
}
