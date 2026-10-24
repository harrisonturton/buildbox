//! Writing messages and the progress bar to the client's stderr: on a
//! terminal, the bar is cleared before anything else is written, and
//! redrawn beneath it.

use crate::{Kind, UiOptions, colored};

/// Moves to the start of the line above and clears it.
const CLEAR_LINE_ABOVE: &str = "\r\x1b[1A\x1b[K";

/// What has been written to the client's stderr, as needed to write more.
pub struct Screen {
    options: UiOptions,
    /// How many lines of progress bar are drawn at the bottom.
    drawn: usize,
    /// Output written that doesn't end a line yet, held back so the bar
    /// isn't drawn in the middle of a line.
    partial: String,
}

impl Screen {
    pub fn new(options: UiOptions) -> Screen {
        Screen {
            options,
            drawn: 0,
            partial: String::new(),
        }
    }

    pub fn options(&self) -> &UiOptions {
        &self.options
    }

    /// Change how output looks, clearing the progress bar drawn the old way.
    pub fn set_options(&mut self, options: UiOptions) -> String {
        let cleared = self.clear();
        self.options = options;
        cleared
    }

    /// An event, as Bazel prints them: `INFO: text`, with `bar` beneath.
    pub fn message(&mut self, kind: Kind, text: &str, bar: &[String]) -> String {
        let mut written = self.clear();
        let partial = std::mem::take(&mut self.partial);
        if !partial.is_empty() {
            written.push_str(&partial);
            written.push('\n');
        }
        written.push_str(&colored(&self.options, kind.color(), kind.prefix()));
        written.push_str(text);
        if !text.ends_with('\n') {
            written.push('\n');
        }
        written.push_str(&self.draw(bar));
        written
    }

    /// Output to show as it is, such as an action's stderr, with `bar`
    /// beneath. A line it doesn't finish is held back until one does.
    pub fn output(&mut self, text: &str, bar: &[String]) -> String {
        self.partial.push_str(text);
        let Some(end) = self.partial.rfind('\n') else {
            return String::new();
        };
        let lines: String = self.partial.drain(..=end).collect();
        let mut written = self.clear();
        written.push_str(&lines);
        written.push_str(&self.draw(bar));
        written
    }

    /// Redraw the progress bar in place. Without cursor control, nothing.
    pub fn redraw(&mut self, bar: &[String]) -> String {
        if !self.options.cursor_control {
            return String::new();
        }
        let mut written = self.clear();
        written.push_str(&self.draw(bar));
        written
    }

    /// Print the progress bar as lines of output, as Bazel does now and then
    /// without cursor control. With it, nothing: the bar is redrawn instead.
    pub fn print_bar(&mut self, bar: &[String]) -> String {
        if self.options.cursor_control || !self.options.show_progress || bar.is_empty() || !self.partial.is_empty() {
            return String::new();
        }
        bar.iter().map(|line| format!("{line}\n")).collect()
    }

    /// Clear the progress bar, and write any line held back, for the end
    /// of a command.
    pub fn finish(&mut self) -> String {
        let mut written = self.clear();
        written.push_str(&std::mem::take(&mut self.partial));
        written
    }

    fn clear(&mut self) -> String {
        let cleared = CLEAR_LINE_ABOVE.repeat(self.drawn);
        self.drawn = 0;
        cleared
    }

    fn draw(&mut self, bar: &[String]) -> String {
        if !self.options.cursor_control || !self.options.show_progress {
            return String::new();
        }
        self.drawn = bar.len();
        bar.iter().map(|line| format!("{line}\n")).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(cursor_control: bool) -> UiOptions {
        UiOptions {
            color: cursor_control,
            cursor_control,
            show_progress: true,
            columns: 80,
        }
    }

    fn bar(text: &str) -> Vec<String> {
        vec![text.to_string()]
    }

    #[test]
    fn messages_go_above_the_bar_on_a_terminal() {
        let mut screen = Screen::new(options(true));
        assert_eq!(screen.redraw(&bar("Loading: ")), "Loading: \n");
        assert_eq!(
            screen.message(Kind::Info, "Analyzed target //pkg:b.", &bar("[0 / 1] no actions running")),
            "\r\x1b[1A\x1b[K\x1b[32mINFO: \x1b[0mAnalyzed target //pkg:b.\n[0 / 1] no actions running\n"
        );
        assert_eq!(screen.finish(), "\r\x1b[1A\x1b[K");
    }

    #[test]
    fn without_cursor_control_the_bar_is_printed_as_lines() {
        let mut screen = Screen::new(options(false));
        assert_eq!(screen.redraw(&bar("Loading: ")), "");
        assert_eq!(screen.print_bar(&bar("Loading: ")), "Loading: \n");
        assert_eq!(screen.message(Kind::Error, "oops", &bar("x")), "ERROR: oops\n");
        assert_eq!(screen.finish(), "");
    }

    #[test]
    fn unfinished_lines_are_held_back() {
        let mut screen = Screen::new(options(true));
        screen.redraw(&bar("Loading: "));
        assert_eq!(screen.output("Target //pkg:b", &bar("Loading: ")), "");
        assert_eq!(screen.output(" up-to-date:\n  bazel-bin/b", &bar("Loading: ")), "\r\x1b[1A\x1b[KTarget //pkg:b up-to-date:\nLoading: \n");
        assert_eq!(screen.finish(), "\r\x1b[1A\x1b[K  bazel-bin/b");
    }
}
