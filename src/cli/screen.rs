//! An owned viewport on the normal terminal screen. Only transcript rows
//! scroll into the terminal's history; the composer is always repainted.

use super::{input, text};

#[derive(Default)]
pub struct Screen {
    output: Vec<String>,
    width: usize,
    footer_top: usize,
}

impl Screen {
    pub fn draw(
        &mut self,
        printed: &str,
        mut footer: Vec<String>,
        mut cursor: Option<(usize, usize)>,
        width: usize,
        height: usize,
    ) -> String {
        let width = width.max(1);
        let height = height.max(1);
        // Leave one column unused to avoid delayed autowrap at the edge.
        let room = width.saturating_sub(1).max(1);
        if self.width != width {
            self.output = self.output.iter().flat_map(|line| text::wrap(line, room)).collect();
        }
        for line in printed.split_terminator('\n') {
            self.output.extend(text::wrap(line.trim_end_matches('\r'), room));
        }
        // On tiny terminals, keep the cursor's row visible even if the
        // status, queued messages or completion menu cannot all fit.
        let limit = height.saturating_sub(1).max(1);
        if footer.len() > limit {
            let row = cursor.map_or(footer.len() - 1, |c| c.0);
            let first = (row + 1).saturating_sub(limit);
            footer = footer.into_iter().skip(first).take(limit).collect();
            cursor = cursor.map(|(row, col)| (row - first, col));
        }
        let top = height.saturating_sub(footer.len());
        let overflow = self.output.len().saturating_sub(top);
        let mut frame = String::new();
        if overflow > 0 {
            // Paint the outgoing transcript at the top before scrolling
            // the whole screen. Erase the old footer first so it can never
            // enter scrollback, even when a single result is very large.
            frame.push_str(&input::goto(self.footer_top.min(height - 1)));
            frame.push_str("\x1b[J");
            for line in self.output.drain(..overflow) {
                frame.push_str("\x1b[H\x1b[2K");
                frame.push_str(&line);
                frame.push_str("\x1b[0m");
                frame.push_str(&input::goto(height - 1));
                frame.push_str("\r\n");
            }
        }
        frame.push_str("\x1b[H\x1b[J");
        for (row, line) in self.output.iter().enumerate() {
            frame.push_str(&input::goto(row));
            frame.push_str(line);
            frame.push_str("\x1b[0m");
        }
        for (row, line) in footer.iter().enumerate() {
            frame.push_str(&input::goto(top + row));
            frame.push_str(&text::clip(line, room));
        }
        let (row, col) = cursor.map_or((self.output.len().min(height - 1), 0), |(row, col)| (top + row, col));
        frame.push_str(&format!("\x1b[{};{}H", row + 1, col.min(width - 1) + 1));
        self.width = width;
        self.footer_top = top;
        frame
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrolls_only_transcript_and_reanchors_on_resize() {
        let mut screen = Screen::default();
        let footer = vec!["rule".into(), "❯ draft".into(), "rule".into()];
        let frame = screen.draw("one\ntwo\nthree\n", footer.clone(), Some((1, 2)), 30, 5);
        assert!(frame.contains("one\x1b[0m\x1b[5;1H\r\n"));
        assert_eq!(screen.output, ["two", "three"]);
        assert!(frame.ends_with("\x1b[4;3H"));
        let frame = screen.draw("", footer, Some((1, 2)), 20, 8);
        assert!(frame.ends_with("\x1b[7;3H"));
        assert_eq!(screen.output, ["two", "three"]);
    }

    #[test]
    fn tiny_viewport_keeps_the_input_cursor() {
        let mut screen = Screen::default();
        let footer = (0..12).map(|i| format!("row {i}")).collect();
        let frame = screen.draw("", footer, Some((5, 2)), 10, 3);
        assert!(frame.contains("row 5"));
        assert!(!frame.contains("row 6"));
        assert!(frame.ends_with("\x1b[3;3H"));
    }
}
