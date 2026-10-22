//! What the screen draws, as pure functions of what it shows: the
//! conversation's lines, the viewport at the bottom, and the detail view.
//!
//! Lines are wrapped here rather than by ratatui, so the screen knows how many
//! rows an entry takes before it scrolls them into place. The wrap measures
//! what ratatui draws and nothing else: a grapheme holding a control character,
//! or one with no width, is left out, as ratatui leaves it out, and a tab is
//! expanded to spaces first. So a row is never wider than the screen, and an
//! entry never takes more rows than were made for it.

use std::collections::VecDeque;

use ratatui::buffer::{Buffer, CellWidth};
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;

use super::editor::{Editor, TAB_STOP};
use super::status::Shown;
use super::transcript::{Entry, Kind};

/// The most rows the input box takes; a longer message scrolls inside it.
pub(super) const BOX_ROWS: usize = 8;

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const PROMPT: &str = "› ";
const INDENT: &str = "  ";
/// The cells [`PROMPT`] and [`INDENT`] take.
const MARGIN: usize = 2;

/// How the screen is styled: in color, or with `NO_COLOR` set, with
/// emphasis alone.
#[derive(Debug, Clone, Copy)]
pub(super) struct Theme {
    color: bool,
}

impl Theme {
    pub(super) fn from_env() -> Self {
        Self {
            color: std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty()),
        }
    }

    #[cfg(test)]
    pub(super) fn plain() -> Self {
        Self { color: false }
    }

    fn accent(self) -> Style {
        if self.color {
            Style::new().fg(Color::Cyan)
        } else {
            Style::new().add_modifier(Modifier::BOLD)
        }
    }

    fn error(self) -> Style {
        if self.color {
            Style::new().fg(Color::Red)
        } else {
            Style::new().add_modifier(Modifier::BOLD)
        }
    }

    fn dim(self) -> Style {
        Style::new().add_modifier(Modifier::DIM)
    }
}

/// `text` as rows at most `width` cells wide: each of its lines broken after
/// the last space that fits, or anywhere in a word too long for a row.
pub(super) fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    for line in text.split('\n') {
        wrap_line(&expand_tabs(line), width, &mut rows);
    }
    rows
}

fn wrap_line(line: &str, width: usize, rows: &mut Vec<String>) {
    let mut row = String::new();
    let mut row_width = 0;
    // Where the row may be broken: just after its last space, as a byte
    // offset and a width.
    let mut space: Option<(usize, usize)> = None;
    for (grapheme, grapheme_width) in visible(line) {
        // A grapheme wider than a row has one to itself.
        if row_width > 0 && row_width + grapheme_width > width {
            match space {
                Some((at, at_width)) if at < row.len() => {
                    let rest = row.split_off(at);
                    rows.push(std::mem::replace(&mut row, rest));
                    row_width -= at_width;
                }
                _ => {
                    rows.push(std::mem::take(&mut row));
                    row_width = 0;
                }
            }
            space = None;
            // What carried over, with this, may still not fit.
            if row_width + grapheme_width > width && row_width > 0 {
                rows.push(std::mem::take(&mut row));
                row_width = 0;
            }
            // A space that ends a row is not carried to the next.
            if grapheme == " " && row_width == 0 {
                continue;
            }
        }
        row.push_str(grapheme);
        row_width += grapheme_width;
        if grapheme == " " {
            space = Some((row.len(), row_width));
        }
    }
    rows.push(row);
}

/// The graphemes ratatui draws, with their widths.
fn visible(text: &str) -> impl Iterator<Item = (&str, usize)> {
    text.graphemes(true)
        .map(|grapheme| (grapheme, grapheme_cells(grapheme)))
        .filter(|(_, width)| *width > 0)
}

/// The cells one grapheme takes as ratatui draws it: none for one it leaves
/// out, one holding a control character.
fn grapheme_cells(grapheme: &str) -> usize {
    if grapheme.contains(char::is_control) {
        0
    } else {
        usize::from(grapheme.cell_width())
    }
}

/// The cells `text` takes as ratatui draws it.
fn cells(text: &str) -> usize {
    visible(text).map(|(_, width)| width).sum()
}

/// `line` with each tab replaced by spaces to the next stop.
fn expand_tabs(line: &str) -> std::borrow::Cow<'_, str> {
    if !line.contains('\t') {
        return line.into();
    }
    let mut expanded = String::with_capacity(line.len() + TAB_STOP);
    let mut column = 0;
    for grapheme in line.graphemes(true) {
        if grapheme == "\t" {
            let spaces = TAB_STOP - column % TAB_STOP;
            expanded.extend(std::iter::repeat_n(' ', spaces));
            column += spaces;
        } else {
            expanded.push_str(grapheme);
            if !grapheme.contains(char::is_control) {
                column += usize::from(grapheme.cell_width());
            }
        }
    }
    expanded.into()
}

/// `entry` as the lines it takes at `width`: in the conversation, or with
/// `dim`, set back from it, as the detail is when `-v` shows it there.
pub(super) fn entry_lines(
    entry: &Entry,
    width: u16,
    theme: Theme,
    dim: bool,
) -> Vec<Line<'static>> {
    let width = usize::from(width);
    let mut lines = Vec::new();
    let body = match entry.kind {
        Kind::User => {
            // A blank line sets each of the user's messages apart.
            lines.push(Line::default());
            let style = Style::new().add_modifier(Modifier::BOLD);
            for (i, row) in wrap(&entry.text, width.saturating_sub(MARGIN))
                .into_iter()
                .enumerate()
            {
                let prefix = if i == 0 {
                    Span::styled(PROMPT, theme.accent())
                } else {
                    Span::raw(INDENT)
                };
                lines.push(Line::from(vec![prefix, Span::styled(row, style)]));
            }
            return lines;
        }
        Kind::Agent => Style::new(),
        Kind::Note | Kind::Trace => theme.dim(),
        Kind::Error => theme.error(),
        Kind::Hint => theme.dim().add_modifier(Modifier::ITALIC),
        Kind::Detail => Style::new(),
    };
    let body = if dim { body.patch(theme.dim()) } else { body };
    let indent = if let Some(title) = &entry.title {
        let style = if dim {
            theme.dim()
        } else {
            theme.accent().add_modifier(Modifier::BOLD)
        };
        for row in wrap(title, width) {
            lines.push(Line::styled(row, style));
        }
        INDENT
    } else {
        ""
    };
    if entry.text.is_empty() {
        return lines;
    }
    for row in wrap(&entry.text, width.saturating_sub(indent.len())) {
        lines.push(Line::from(vec![Span::raw(indent), Span::styled(row, body)]));
    }
    lines
}

/// The input box laid out: its rows, and the row and column of the cursor in
/// them.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Layout {
    rows: Vec<String>,
    cursor: (usize, usize),
}

/// `editor`'s box laid out for a viewport `width` cells wide.
pub(super) fn layout(editor: &Editor, width: u16) -> Layout {
    let width = usize::from(width).saturating_sub(MARGIN);
    input_rows(editor.text(), editor.cursor(), width)
}

/// The input box's rows at `width` cells, after its prompt. Rows break at each
/// newline and wherever a row is full, so the cursor is placed where it is
/// drawn.
fn input_rows(text: &str, cursor: usize, width: usize) -> Layout {
    let width = width.max(1);
    let mut rows = vec![String::new()];
    let mut column = 0;
    let mut at = None;
    for (offset, grapheme) in text.grapheme_indices(true) {
        if offset == cursor {
            at = Some((rows.len() - 1, column));
        }
        if grapheme == "\n" {
            rows.push(String::new());
            column = 0;
            continue;
        }
        let grapheme_width = grapheme_cells(grapheme);
        if grapheme_width == 0 {
            continue;
        }
        if column + grapheme_width > width {
            rows.push(String::new());
            column = 0;
            if offset == cursor {
                at = Some((rows.len() - 1, 0));
            }
        }
        rows.last_mut().expect("never empty").push_str(grapheme);
        column += grapheme_width;
    }
    let cursor = at.unwrap_or_else(|| {
        // At the end; past a full row it starts the next.
        if column >= width {
            rows.push(String::new());
            (rows.len() - 1, 0)
        } else {
            (rows.len() - 1, column)
        }
    });
    Layout { rows, cursor }
}

/// How many rows the viewport takes: the status line, and the input box as
/// tall as what is typed, up to [`BOX_ROWS`], or one row once input has ended.
pub(super) fn viewport_height(input: Option<&Layout>) -> u16 {
    1 + input.map_or(1, |input| input.rows.len()).min(BOX_ROWS) as u16
}

/// Draw the viewport in `buf`: the status line, with a spinner while the
/// session is busy and `hint` at its right, and the input box below it, laid
/// out for `buf`'s width, or a note that input has ended when `input` is
/// `None`. Returns where the cursor goes.
pub(super) fn draw_viewport(
    buf: &mut Buffer,
    status: &Shown,
    frame: usize,
    hint: &str,
    input: Option<&Layout>,
    theme: Theme,
) -> Option<Position> {
    let area = buf.area;
    if area.height == 0 || area.width == 0 {
        return None;
    }
    let mut left = vec![];
    if status.busy {
        left.push(Span::styled(SPINNER[frame % SPINNER.len()], theme.accent()));
        left.push(Span::raw(" "));
    }
    left.push(Span::raw(status.text.as_str()));
    let left = Line::from(left);
    let hint_width = cells(hint);
    let fits = left.width() + 2 + hint_width <= usize::from(area.width);
    buf.set_line(area.x, area.y, &left, area.width);
    if fits && !hint.is_empty() {
        let x = area.x + area.width - hint_width as u16;
        buf.set_line(
            x,
            area.y,
            &Line::styled(hint, theme.dim()),
            hint_width as u16,
        );
    }

    let box_area = Rect {
        y: area.y + 1,
        height: area.height - 1,
        ..area
    };
    if box_area.height == 0 {
        return None;
    }
    let Some(Layout {
        rows,
        cursor: (row, column),
    }) = input
    else {
        buf.set_line(
            box_area.x,
            box_area.y,
            &Line::styled(
                "input has ended",
                theme.dim().add_modifier(Modifier::ITALIC),
            ),
            box_area.width,
        );
        return None;
    };
    let (row, column) = (*row, *column);
    let shown = usize::from(box_area.height);
    let first = row
        .saturating_sub(shown - 1)
        .min(rows.len().saturating_sub(shown));
    for (i, text) in rows.iter().skip(first).take(shown).enumerate() {
        let prefix = if first + i == 0 {
            Span::styled(PROMPT, theme.accent())
        } else {
            Span::raw(INDENT)
        };
        let line = Line::from(vec![prefix, Span::raw(text.as_str())]);
        buf.set_line(box_area.x, box_area.y + i as u16, &line, box_area.width);
    }
    let x = box_area.x + (MARGIN + column).min(usize::from(box_area.width) - 1) as u16;
    Some(Position::new(x, box_area.y + (row - first) as u16))
}

/// Draw the detail view in `buf`: a title row, then `lines` scrolled `scroll`
/// rows up from the bottom. Returns `scroll` as far as it can go, so scrolling
/// past the top stops there.
pub(super) fn draw_detail(
    buf: &mut Buffer,
    lines: &VecDeque<Line<'static>>,
    scroll: usize,
    dropped: u64,
) -> usize {
    let area = buf.area;
    if area.height == 0 {
        return 0;
    }
    let bar = Style::new().add_modifier(Modifier::REVERSED);
    buf.set_style(Rect { height: 1, ..area }, bar);
    let mut title = String::from(" detail");
    if dropped > 0 {
        title.push_str(&format!(" ({dropped} earliest not kept)"));
    }
    title.push_str(" · Esc closes · ↑ ↓ PgUp PgDn Home End scroll");
    buf.set_line(area.x, area.y, &Line::styled(title, bar), area.width);

    let rows = usize::from(area.height - 1);
    let scroll = scroll.min(lines.len().saturating_sub(rows));
    let end = lines.len() - scroll;
    let start = end.saturating_sub(rows);
    for (i, line) in lines.range(start..end).enumerate() {
        buf.set_line(area.x, area.y + 1 + i as u16, line, area.width);
    }
    scroll
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::run_new::screen::rows_of;

    /// Each row as ratatui would draw it, whatever its styling.
    fn texts_of(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn text_wraps_after_the_last_space_that_fits() {
        assert_eq!(wrap("the quick brown fox", 10), ["the quick ", "brown fox"]);
        assert_eq!(wrap("a  b", 2), ["a ", "b"]);
        assert_eq!(wrap("one\n\ntwo", 10), ["one", "", "two"]);
        assert_eq!(wrap("", 10), [""]);
    }

    #[test]
    fn a_word_longer_than_a_row_is_broken_anywhere() {
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(wrap("ab cdefghij", 4), ["ab ", "cdef", "ghij"]);
    }

    /// Rows are measured as ratatui draws: a wide character takes two cells
    /// and never straddles a row, and what ratatui leaves out takes none.
    #[test]
    fn rows_are_measured_as_drawn() {
        assert_eq!(wrap("日本語の", 5), ["日本", "語の"]);
        assert_eq!(wrap("a\u{7}b\u{200b}c", 2), ["ab", "c"]);
        assert_eq!(wrap("\tx", 8), ["    x"]);
        assert_eq!(wrap("ab\tx", 8), ["ab  x"]);
        for text in [
            "日本語の text\twith\u{7}tabs",
            "e\u{301}e\u{301}e\u{301}",
            "a b c d e",
        ] {
            for width in 1..12 {
                for row in wrap(text, width) {
                    assert!(cells(&row) <= width.max(2), "{row:?} at {width}");
                }
            }
        }
    }

    /// Drawn into a buffer as wide as it was wrapped for, no row of an entry
    /// runs past the edge: what is measured is what is drawn.
    #[test]
    fn a_drawn_entry_fits_its_rows() {
        let entry = Entry::new(Kind::Agent, "日本語 and\ttabs, wide and narrow, wrapped");
        for width in 3..20u16 {
            let lines = entry_lines(&entry, width, Theme::plain(), false);
            let mut buf = Buffer::empty(Rect::new(0, 0, width, lines.len() as u16));
            for (i, line) in lines.iter().enumerate() {
                let (end, _) = buf.set_line(0, i as u16, line, width);
                assert!(end <= width);
                assert_eq!(usize::from(end), line.width(), "{line:?} at {width}");
            }
        }
    }

    #[test]
    fn a_users_message_is_set_apart_and_marked() {
        let entry = Entry::new(Kind::User, "count the files\nby crate");
        assert_eq!(
            texts_of(&entry_lines(&entry, 40, Theme::plain(), false)),
            ["", "› count the files", "  by crate"]
        );
    }

    #[test]
    fn a_detail_entry_is_titled_and_indented() {
        let entry = Entry::titled(Kind::Detail, "python · execution 7", "x = 1\nprint(x)");
        assert_eq!(
            texts_of(&entry_lines(&entry, 40, Theme::plain(), false)),
            ["python · execution 7", "  x = 1", "  print(x)"]
        );
        let bare = Entry::titled(Kind::Detail, "round 2", "");
        assert_eq!(
            texts_of(&entry_lines(&bare, 40, Theme::plain(), false)),
            ["round 2"]
        );
    }

    #[test]
    fn the_input_box_places_the_cursor_where_it_is_drawn() {
        let laid = |rows: &[&str], cursor| Layout {
            rows: rows.iter().map(|row| row.to_string()).collect(),
            cursor,
        };
        assert_eq!(input_rows("", 0, 10), laid(&[""], (0, 0)));
        assert_eq!(input_rows("abc", 1, 10), laid(&["abc"], (0, 1)));
        assert_eq!(
            input_rows("abcdef", 6, 3),
            laid(&["abc", "def", ""], (2, 0))
        );
        assert_eq!(input_rows("ab\ncd", 3, 10), laid(&["ab", "cd"], (1, 0)));
        // A wide character that does not fit starts the next row, cursor too.
        assert_eq!(input_rows("ab日", 2, 3), laid(&["ab", "日"], (1, 0)));
    }

    #[test]
    fn the_viewport_shows_the_status_the_hint_and_the_box() {
        let mut editor = Editor::default();
        editor.paste("hello\nworld");
        let input = layout(&editor, 30);
        let height = viewport_height(Some(&input));
        assert_eq!(height, 3);
        let mut buf = Buffer::empty(Rect::new(0, 5, 30, height));
        let status = Shown {
            text: "ready".into(),
            busy: false,
        };
        let cursor = draw_viewport(
            &mut buf,
            &status,
            0,
            "Ctrl-O detail",
            Some(&input),
            Theme::plain(),
        );
        assert_eq!(
            rows_of(&buf),
            ["ready            Ctrl-O detail", "› hello", "  world"]
        );
        assert_eq!(cursor, Some(Position::new(7, 7)));

        // Too narrow for both, the status wins; a busy session spins.
        let mut buf = Buffer::empty(Rect::new(0, 0, 12, 2));
        let busy = Shown {
            text: "Python running".into(),
            busy: true,
        };
        draw_viewport(&mut buf, &busy, 1, "Ctrl-O detail", None, Theme::plain());
        assert_eq!(rows_of(&buf), ["⠙ Python run", "input has en"]);
    }

    /// A box taller than [`BOX_ROWS`] shows the rows around the cursor.
    #[test]
    fn a_tall_box_scrolls_to_the_cursor() {
        let mut editor = Editor::default();
        let text = (0..12)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        editor.paste(&text);
        let input = layout(&editor, 20);
        assert_eq!(viewport_height(Some(&input)), 1 + BOX_ROWS as u16);
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 1 + BOX_ROWS as u16));
        let status = Shown {
            text: String::new(),
            busy: false,
        };
        let cursor = draw_viewport(&mut buf, &status, 0, "", Some(&input), Theme::plain());
        let rows = rows_of(&buf);
        assert_eq!(rows[1], "  4");
        assert_eq!(rows[BOX_ROWS], "  11");
        assert_eq!(cursor, Some(Position::new(4, BOX_ROWS as u16)));
    }

    #[test]
    fn the_detail_view_follows_the_bottom_until_scrolled() {
        let lines: VecDeque<Line<'static>> =
            (0..10).map(|i| Line::raw(format!("line {i}"))).collect();
        let mut buf = Buffer::empty(Rect::new(0, 0, 60, 4));
        assert_eq!(draw_detail(&mut buf, &lines, 0, 0), 0);
        assert_eq!(&rows_of(&buf)[1..], ["line 7", "line 8", "line 9"]);

        let mut buf = Buffer::empty(Rect::new(0, 0, 60, 4));
        assert_eq!(draw_detail(&mut buf, &lines, 2, 3), 2);
        assert_eq!(&rows_of(&buf)[1..], ["line 5", "line 6", "line 7"]);
        assert!(rows_of(&buf)[0].contains("3 earliest not kept"));

        // Past the top it stops at the first line.
        let mut buf = Buffer::empty(Rect::new(0, 0, 60, 4));
        assert_eq!(draw_detail(&mut buf, &lines, 50, 0), 7);
        assert_eq!(rows_of(&buf)[1], "line 0");
    }
}
