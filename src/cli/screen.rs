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
    /// Removes the temporary footer and blank viewport padding. The next
    /// shell prompt follows the transcript, while native history stays put.
    pub fn finish(&mut self, width: usize, height: usize) -> String {
        while self.output.last().is_some_and(|line| text::strip_ansi(line).trim().is_empty()) {
            self.output.pop();
        }
        let mut frame = self.draw("", Vec::new(), None, width, height);
        if self.output.len() >= height.max(1) {
            // A completely full transcript needs one final row for the
            // shell, rather than overwriting its last line.
            frame.push_str("\r\n");
        }
        frame
    }

    fn append(&mut self, printed: &str, width: usize) {
        let room = width.saturating_sub(1).max(1);
        if self.width != width {
            self.output = self.output.iter().flat_map(|line| text::wrap(line, room)).collect();
        }
        for line in printed.split_terminator('\n') {
            self.output.extend(text::wrap(line.trim_end_matches('\r'), room));
        }
        self.width = width;
    }

    /// A temporary inspector covers the viewport without scrolling the
    /// conversation away. Output arriving meanwhile is drawn on return.
    pub fn overlay(
        &mut self, printed: &str, rows: Vec<String>, cursor: Option<(usize, usize)>, width: usize, height: usize,
    ) -> String {
        let width = width.max(1);
        let height = height.max(1);
        self.append(printed, width);
        let room = width.saturating_sub(1).max(1);
        let mut frame = String::from("\x1b[H\x1b[J");
        for (row, line) in rows.iter().take(height).enumerate() {
            frame.push_str(&input::goto(row));
            frame.push_str(&text::clip(line, room));
        }
        let (row, col) = cursor.unwrap_or((0, 0));
        frame.push_str(&format!("\x1b[{};{}H", row.min(height - 1) + 1, col.min(width - 1) + 1));
        // The overlay must be erased before any transcript scroll on return.
        self.footer_top = 0;
        frame
    }

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
        self.append(printed, width);
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
    fn inspector_preserves_transcript_and_output_arriving_while_open() {
        let mut screen = Screen::default();
        screen.draw("call one\nresult one\n", vec!["composer".into()], None, 30, 8);
        let frame = screen.overlay("call two\nresult two\n", vec!["Tool output".into(); 7], None, 30, 8);
        assert!(!frame.contains("call one"));
        assert!(!frame.contains("\r\n"));
        assert_eq!(screen.output.len(), 4);
        let frame = screen.draw("", vec!["composer".into()], None, 30, 8);
        assert!(frame.contains("call one"));
        assert!(frame.contains("result two"));
        assert!(!frame.contains("Tool output"));
    }

    #[test]
    fn exit_places_the_shell_after_content_and_removes_padding() {
        let mut screen = Screen::default();
        screen.draw("banner\n❯ exit\n\n", vec!["rule".into(), "❯ draft".into(), "rule".into()], Some((1, 2)), 80, 24);
        let frame = screen.finish(80, 24);
        assert!(frame.contains("❯ exit"));
        assert!(!frame.contains("draft"));
        assert!(!frame.contains("rule"));
        assert!(frame.ends_with("\x1b[3;1H"));
        assert!(!frame.ends_with("\x1b[24;1H\r\n"));
    }

    #[test]
    fn exit_from_a_full_screen_keeps_the_last_transcript_line() {
        let mut screen = Screen::default();
        screen.draw("one\ntwo\nthree\n", Vec::new(), None, 20, 3);
        let frame = screen.finish(20, 3);
        assert!(frame.contains("three"));
        assert!(frame.ends_with("\x1b[3;1H\r\n"));
    }

    #[test]
    fn exit_after_resize_reflows_before_placing_the_shell() {
        let mut screen = Screen::default();
        screen.draw("abcdefgh\n", vec!["footer".into()], None, 20, 6);
        let frame = screen.finish(5, 6);
        assert!(frame.contains("abcd"));
        assert!(frame.contains("efgh"));
        assert!(frame.ends_with("\x1b[3;1H"));
    }

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
