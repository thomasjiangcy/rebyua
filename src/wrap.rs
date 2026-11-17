//! Soft-wrapping of diff line text into terminal rows.
//!
//! Layout (row counts) and rendering (row contents) both go through
//! [`break_rows`], so a line always occupies exactly the rows it was measured at.

use std::borrow::Cow;
use std::ops::Range;

use ratatui::style::Style;
use ratatui::text::Span;
use unicode_width::UnicodeWidthChar;

/// Tabs are drawn as this many spaces so their width is predictable.
pub const TAB_WIDTH: usize = 4;

pub fn expand_tabs(text: &str) -> Cow<'_, str> {
    if text.contains('\t') {
        Cow::Owned(text.replace('\t', &" ".repeat(TAB_WIDTH)))
    } else {
        Cow::Borrowed(text)
    }
}

fn char_width(c: char) -> usize {
    c.width().unwrap_or(0)
}

/// Splits `text` into byte ranges that each fit in `width` columns.
///
/// Prefers breaking after whitespace; a word wider than `width` is broken
/// mid-word. Whitespace that overflows a row stays at the end of that row
/// (it is invisible, so it gets clipped instead of starting the next row).
/// Always returns at least one range, and every row holds at least one char.
pub fn break_rows(text: &str, width: usize) -> Vec<Range<usize>> {
    let mut rows = Vec::new();
    if width == 0 {
        rows.push(0..text.len());
        return rows;
    }

    let mut row_start = 0;
    let mut row_width = 0;
    // Byte offset just after the most recent whitespace in the current row,
    // and the row width up to that point.
    let mut last_break: Option<(usize, usize)> = None;

    for (idx, c) in text.char_indices() {
        let cw = char_width(c);
        let end = idx + c.len_utf8();
        if row_width > 0 && row_width + cw > width {
            if c.is_whitespace() {
                rows.push(row_start..end);
                row_start = end;
                row_width = 0;
                last_break = None;
                continue;
            }
            if let Some((break_at, break_width)) = last_break.take() {
                rows.push(row_start..break_at);
                row_start = break_at;
                row_width -= break_width;
            }
            if row_width > 0 && row_width + cw > width {
                rows.push(row_start..idx);
                row_start = idx;
                row_width = 0;
            }
        }
        row_width += cw;
        if c.is_whitespace() {
            last_break = Some((end, row_width));
        }
    }

    if row_start < text.len() || rows.is_empty() {
        rows.push(row_start..text.len());
    }
    rows
}

pub fn row_count(text: &str, width: usize) -> usize {
    break_rows(&expand_tabs(text), width).len()
}

/// Cuts styled segments (whose concatenated text was broken into `rows`)
/// into one span list per row.
pub fn split_segments(
    segments: &[(String, Style)],
    rows: &[Range<usize>],
) -> Vec<Vec<Span<'static>>> {
    let mut out: Vec<Vec<Span<'static>>> = rows.iter().map(|_| Vec::new()).collect();
    let mut seg_start = 0;
    for (text, style) in segments {
        let seg_end = seg_start + text.len();
        for (row_idx, row) in rows.iter().enumerate() {
            let start = row.start.max(seg_start);
            let end = row.end.min(seg_end);
            if start < end {
                out[row_idx].push(Span::styled(
                    text[start - seg_start..end - seg_start].to_string(),
                    *style,
                ));
            }
        }
        seg_start = seg_end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(text: &str, width: usize) -> Vec<&str> {
        break_rows(text, width)
            .into_iter()
            .map(|range| &text[range])
            .collect()
    }

    #[test]
    fn short_line_is_one_row() {
        assert_eq!(rows("hello", 10), vec!["hello"]);
        assert_eq!(rows("", 10), vec![""]);
    }

    #[test]
    fn line_exactly_filling_width_is_one_row() {
        assert_eq!(rows("abcde", 5), vec!["abcde"]);
        assert_eq!(rows("abcd ", 5), vec!["abcd "]);
    }

    #[test]
    fn breaks_after_last_space_that_fits() {
        assert_eq!(
            rows("the quick brown fox", 10),
            vec!["the quick ", "brown fox"]
        );
    }

    #[test]
    fn overflowing_space_hangs_on_previous_row() {
        assert_eq!(rows("abcde fgh", 5), vec!["abcde ", "fgh"]);
    }

    #[test]
    fn long_word_breaks_mid_word() {
        assert_eq!(rows("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        assert_eq!(rows("ab abcdefghij", 4), vec!["ab ", "abcd", "efgh", "ij"]);
    }

    #[test]
    fn wide_chars_count_two_columns() {
        assert_eq!(rows("你好世界", 5), vec!["你好", "世界"]);
        assert_eq!(rows("a你", 2), vec!["a", "你"]);
    }

    #[test]
    fn char_wider_than_width_still_makes_progress() {
        assert_eq!(rows("你你", 1), vec!["你", "你"]);
    }

    #[test]
    fn zero_width_disables_wrapping() {
        assert_eq!(rows("a long line", 0), vec!["a long line"]);
    }

    #[test]
    fn row_count_expands_tabs() {
        assert_eq!(row_count("\tab", 6), 1);
        assert_eq!(row_count("\tabc", 6), 2);
    }

    #[test]
    fn split_segments_follows_row_boundaries() {
        let red = Style::default().fg(ratatui::style::Color::Red);
        let segments = vec![
            ("the qu".to_string(), red),
            ("ick brown".to_string(), Style::default()),
        ];
        let text: String = segments.iter().map(|(t, _)| t.as_str()).collect();
        let split = split_segments(&segments, &break_rows(&text, 10));
        let as_text: Vec<Vec<(&str, Style)>> = split
            .iter()
            .map(|row| row.iter().map(|s| (s.content.as_ref(), s.style)).collect())
            .collect();
        assert_eq!(
            as_text,
            vec![
                vec![("the qu", red), ("ick ", Style::default())],
                vec![("brown", Style::default())],
            ]
        );
    }
}
