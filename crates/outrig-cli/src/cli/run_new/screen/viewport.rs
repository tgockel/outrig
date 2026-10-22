//! Where the screen draws: a few rows at the bottom of the conversation that
//! it owns, with the conversation scrolled into the terminal's history above
//! them.
//!
//! ratatui's inline viewport does nearly this, but it cannot change its
//! height, which the input box needs as a message grows, and a window made
//! narrower makes it clear the whole screen and move to its top row, wiping
//! the conversation in view. So the screen places the rows itself -- a
//! [`Panel`] it diffs each frame against the last -- and moves the
//! conversation above them with the terminal's scroll regions, as ratatui's
//! inline viewport does when it inserts lines before itself.
//!
//! Everything here is generic over ratatui's [`Backend`], so ratatui's test
//! backend, which keeps a scrollback of its own, tests it.

use ratatui::backend::{Backend, ClearType};
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect, Size};
use ratatui::text::Line;

/// A region of the screen and what was last drawn in it, so each frame
/// writes only the cells that changed.
#[derive(Debug)]
pub(super) struct Panel {
    drawn: Buffer,
}

impl Panel {
    /// A panel over `area`, whose cells the caller has cleared.
    pub(super) fn cleared(area: Rect) -> Self {
        Self {
            drawn: Buffer::empty(area),
        }
    }

    pub(super) fn area(&self) -> Rect {
        self.drawn.area
    }

    /// Draw what `render` puts in a fresh buffer over the panel, and place
    /// the cursor where it says, or hide it.
    pub(super) fn draw<B: Backend>(
        &mut self,
        backend: &mut B,
        render: impl FnOnce(&mut Buffer) -> Option<Position>,
    ) -> Result<Option<Position>, B::Error> {
        let mut next = Buffer::empty(self.drawn.area);
        let cursor = render(&mut next);
        backend.draw(self.drawn.diff_iter(&next))?;
        match cursor {
            Some(at) => {
                backend.set_cursor_position(at)?;
                backend.show_cursor()?;
            }
            None => {
                backend.hide_cursor()?;
                backend.set_cursor_position(self.drawn.area.as_position())?;
            }
        }
        self.drawn = next;
        Ok(cursor)
    }
}

/// The rows at the bottom of the conversation the screen draws in.
#[derive(Debug)]
pub(super) struct Viewport {
    screen: Size,
    panel: Panel,
    /// The cursor's row in the viewport when it was last drawn, which is how
    /// the viewport is found again after the terminal moved it.
    cursor_row: u16,
}

impl Viewport {
    /// Take `height` rows starting where the cursor is, scrolling the
    /// terminal up if there are not that many below it. A terminal that does
    /// not say where its cursor is gets the bottom rows.
    pub(super) fn open<B: Backend>(backend: &mut B, height: u16) -> Result<Self, B::Error> {
        let screen = backend.size()?;
        let height = fit(height, screen);
        // The query is answered from what reached the terminal.
        backend.flush()?;
        let top = match backend.get_cursor_position() {
            // A line in progress keeps its row.
            Ok(at) => make_room(backend, screen, at.y + u16::from(at.x > 0), height)?,
            Err(_) => {
                backend.append_lines(height)?;
                screen.height - height
            }
        };
        let mut viewport = Self {
            screen,
            panel: Panel::cleared(Rect::default()),
            cursor_row: 0,
        };
        viewport.relocate(backend, top, height)?;
        Ok(viewport)
    }

    pub(super) fn screen(&self) -> Size {
        self.screen
    }

    pub(super) fn area(&self) -> Rect {
        self.panel.area()
    }

    /// Draw a frame; see [`Panel::draw`].
    pub(super) fn draw<B: Backend>(
        &mut self,
        backend: &mut B,
        render: impl FnOnce(&mut Buffer) -> Option<Position>,
    ) -> Result<(), B::Error> {
        let top = self.area().y;
        let cursor = self.panel.draw(backend, render)?;
        self.cursor_row = cursor.map_or(0, |at| at.y - top);
        Ok(())
    }

    /// Put `lines` into the conversation above the viewport: into the rows
    /// below it while there are any, moving it down, and then by scrolling
    /// the rows above it up, into the terminal's history.
    pub(super) fn insert<B: Backend>(
        &mut self,
        backend: &mut B,
        lines: &[Line<'_>],
    ) -> Result<(), B::Error> {
        let width = self.screen.width;
        // In chunks no taller than the screen, so a long message never needs
        // a buffer taller than a terminal could ask for.
        for chunk in lines.chunks(usize::from(self.screen.height.max(1))) {
            let height = chunk.len() as u16;
            let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
            for (y, line) in chunk.iter().enumerate() {
                buffer.set_line(0, y as u16, line, width);
            }
            let mut rows = 0;
            let area = self.area();
            let free = self.screen.height.saturating_sub(area.bottom());
            if free > 0 {
                let moved = height.min(free);
                backend.scroll_region_down(area.top()..area.bottom() + moved, moved)?;
                draw_rows(backend, &buffer, rows, area.top(), moved)?;
                self.panel.drawn.area.y += moved;
                rows += moved;
            }
            let top = self.area().top();
            // A region of one row is none to a terminal: it refuses
            // `ESC[1;1r`, and the scroll after it moves the whole screen. So
            // over a viewport one row from the top, the whole screen is
            // scrolled, a row at a time, with the viewport's rows cleared
            // first, to be drawn whole in the next frame.
            let whole = top == 1 && rows < height;
            if whole {
                backend.set_cursor_position(self.area().as_position())?;
                backend.clear_region(ClearType::AfterCursor)?;
                self.panel = Panel::cleared(self.area());
            }
            let region = if whole { self.screen.height } else { top };
            while rows < height && top > 0 {
                let scrolled = (height - rows).min(top);
                backend.scroll_region_up(0..region, scrolled)?;
                draw_rows(backend, &buffer, rows, top - scrolled, scrolled)?;
                rows += scrolled;
            }
        }
        Ok(())
    }

    /// Make the viewport `height` rows: taller into the rows below it, and
    /// past them by scrolling the conversation up; shorter by giving up its
    /// bottom rows.
    pub(super) fn set_height<B: Backend>(
        &mut self,
        backend: &mut B,
        height: u16,
    ) -> Result<(), B::Error> {
        let height = fit(height, self.screen);
        let area = self.area();
        if height == area.height {
            return Ok(());
        }
        let below = self.screen.height - area.top();
        let top = if height > below {
            let short = height - below;
            backend.scroll_region_up(0..area.top(), short)?;
            area.top() - short
        } else {
            area.top()
        };
        self.relocate(backend, top, height)
    }

    /// Find the viewport again after the terminal changed size, from where
    /// the cursor ended up: terminals move what is on screen as they resize,
    /// each its own way. A terminal that does not say where its cursor is
    /// gets the bottom rows.
    pub(super) fn resized<B: Backend>(&mut self, backend: &mut B) -> Result<(), B::Error> {
        self.screen = backend.size()?;
        let height = fit(self.area().height, self.screen);
        backend.flush()?;
        let top = match backend.get_cursor_position() {
            Ok(at) => at.y.saturating_sub(self.cursor_row),
            Err(_) => self.screen.height - height,
        };
        self.relocate(backend, top.min(self.screen.height - height), height)
    }

    /// Draw it all again: Ctrl-L, or after something else wrote over it.
    pub(super) fn redraw<B: Backend>(&mut self, backend: &mut B) -> Result<(), B::Error> {
        let area = self.area();
        self.relocate(backend, area.top(), area.height)
    }

    /// Clear the viewport's rows and leave the cursor at their top, where
    /// whatever is written next -- the shell's prompt, the session's closing
    /// lines -- starts.
    pub(super) fn clear<B: Backend>(&mut self, backend: &mut B) -> Result<(), B::Error> {
        let area = self.area();
        backend.set_cursor_position(area.as_position())?;
        backend.clear_region(ClearType::AfterCursor)?;
        backend.show_cursor()
    }

    /// Take `height` rows from `top`, clearing them and everything below,
    /// where nothing of the conversation is.
    fn relocate<B: Backend>(
        &mut self,
        backend: &mut B,
        top: u16,
        height: u16,
    ) -> Result<(), B::Error> {
        let area = Rect::new(0, top, self.screen.width, height);
        backend.set_cursor_position(area.as_position())?;
        backend.clear_region(ClearType::AfterCursor)?;
        self.panel = Panel::cleared(area);
        self.cursor_row = 0;
        Ok(())
    }
}

/// `height`, leaving one row of the screen above the viewport, so there is
/// always a row to scroll the conversation through.
fn fit(height: u16, screen: Size) -> u16 {
    height.min(screen.height.saturating_sub(1)).max(1)
}

/// The top row of `height` rows from `row`, scrolling the terminal up with
/// newlines, as a shell's output does, if they run past its bottom.
fn make_room<B: Backend>(
    backend: &mut B,
    screen: Size,
    row: u16,
    height: u16,
) -> Result<u16, B::Error> {
    let bottom = screen.height;
    if row + height <= bottom {
        return Ok(row);
    }
    backend.set_cursor_position(Position::new(0, bottom - 1))?;
    backend.append_lines(row + height - bottom)?;
    Ok(bottom - height)
}

/// Draw `count` rows of `buffer`, from its row `from`, at screen row `at`,
/// into rows the caller has cleared.
fn draw_rows<B: Backend>(
    backend: &mut B,
    buffer: &Buffer,
    from: u16,
    at: u16,
    count: u16,
) -> Result<(), B::Error> {
    let width = buffer.area.width;
    let area = Rect::new(0, at, width, count);
    let start = usize::from(from) * usize::from(width);
    let end = start + usize::from(count) * usize::from(width);
    let rows = Buffer {
        area,
        content: buffer.content[start..end].to_vec(),
    };
    backend.draw(Buffer::empty(area).diff_iter(&rows))
}

#[cfg(test)]
mod tests {
    use ratatui::backend::{CrosstermBackend, TestBackend};

    use super::*;
    use crate::cli::run_new::screen::rows_of;

    /// A terminal showing `rows`, its cursor at the start of the row after
    /// the last of them.
    fn terminal(width: u16, height: u16, rows: &[&str]) -> TestBackend {
        let width = usize::from(width);
        let mut lines: Vec<String> = rows.iter().map(|row| format!("{row:width$}")).collect();
        lines.resize(usize::from(height), " ".repeat(width));
        let mut backend = TestBackend::with_lines(lines);
        assert_eq!(usize::from(backend.size().expect("sized").width), width);
        backend
            .set_cursor_position(Position::new(0, rows.len() as u16))
            .expect("placed");
        backend
    }

    fn screen(backend: &TestBackend) -> Vec<String> {
        rows_of(backend.buffer())
    }

    /// What the terminal scrolled into its history, oldest first.
    fn history(backend: &TestBackend) -> Vec<String> {
        rows_of(backend.scrollback())
    }

    fn fill<B: Backend>(viewport: &mut Viewport, backend: &mut B, text: &str) {
        viewport
            .draw(backend, |buf| {
                for y in 0..buf.area.height {
                    buf.set_string(
                        0,
                        buf.area.y + y,
                        format!("{text}{y}"),
                        ratatui::style::Style::new(),
                    );
                }
                None
            })
            .expect("drawn");
    }

    #[test]
    fn it_opens_below_what_is_on_screen() {
        let mut backend = terminal(10, 6, &["banner"]);
        let mut viewport = Viewport::open(&mut backend, 2).expect("opened");
        assert_eq!(viewport.area(), Rect::new(0, 1, 10, 2));
        fill(&mut viewport, &mut backend, "v");
        assert_eq!(screen(&backend), ["banner", "v0", "v1", "", "", ""]);
    }

    /// Opened with the cursor too near the bottom, it scrolls what is on
    /// screen up into the terminal's history to make room.
    #[test]
    fn it_scrolls_to_open_at_the_bottom() {
        let mut backend = terminal(10, 4, &["a", "b", "c"]);
        let mut viewport = Viewport::open(&mut backend, 2).expect("opened");
        assert_eq!(viewport.area(), Rect::new(0, 2, 10, 2));
        fill(&mut viewport, &mut backend, "v");
        assert_eq!(screen(&backend), ["b", "c", "v0", "v1"]);
        assert_eq!(history(&backend), ["a"]);
    }

    /// Lines go into the rows below the viewport first, pushing it down,
    /// then scroll the rows above it into the terminal's history, and the
    /// viewport is never drawn over.
    #[test]
    fn inserted_lines_go_above_it_and_into_history() {
        let mut backend = terminal(10, 5, &["banner"]);
        let mut viewport = Viewport::open(&mut backend, 2).expect("opened");
        fill(&mut viewport, &mut backend, "v");

        viewport
            .insert(&mut backend, &[Line::raw("one"), Line::raw("two")])
            .expect("inserted");
        assert_eq!(screen(&backend), ["banner", "one", "two", "v0", "v1"]);
        assert_eq!(viewport.area(), Rect::new(0, 3, 10, 2));

        viewport
            .insert(&mut backend, &[Line::raw("three")])
            .expect("inserted");
        assert_eq!(screen(&backend), ["one", "two", "three", "v0", "v1"]);
        assert_eq!(history(&backend), ["banner"]);
    }

    /// A message taller than the screen goes in whole, a screen at a time.
    #[test]
    fn a_message_taller_than_the_screen_goes_in_whole() {
        let mut backend = terminal(10, 4, &[]);
        let mut viewport = Viewport::open(&mut backend, 1).expect("opened");
        fill(&mut viewport, &mut backend, "v");
        let lines: Vec<Line<'_>> = (0..9).map(|i| Line::raw(format!("m{i}"))).collect();
        viewport.insert(&mut backend, &lines).expect("inserted");
        assert_eq!(screen(&backend), ["m6", "m7", "m8", "v0"]);
        assert_eq!(history(&backend), ["m0", "m1", "m2", "m3", "m4", "m5"]);
    }

    #[test]
    fn it_grows_into_free_rows_then_by_scrolling_and_shrinks_in_place() {
        let mut backend = terminal(10, 5, &["a", "b"]);
        let mut viewport = Viewport::open(&mut backend, 1).expect("opened");
        assert_eq!(viewport.area(), Rect::new(0, 2, 10, 1));

        viewport.set_height(&mut backend, 3).expect("grown");
        assert_eq!(viewport.area(), Rect::new(0, 2, 10, 3));
        fill(&mut viewport, &mut backend, "v");
        assert_eq!(screen(&backend), ["a", "b", "v0", "v1", "v2"]);

        viewport.set_height(&mut backend, 4).expect("grown");
        assert_eq!(viewport.area(), Rect::new(0, 1, 10, 4));
        fill(&mut viewport, &mut backend, "w");
        assert_eq!(screen(&backend), ["b", "w0", "w1", "w2", "w3"]);
        assert_eq!(history(&backend), ["a"]);

        viewport.set_height(&mut backend, 2).expect("shrunk");
        fill(&mut viewport, &mut backend, "x");
        assert_eq!(screen(&backend), ["b", "x0", "x1", "", ""]);
    }

    /// One row of the screen is always left above it, to scroll through.
    #[test]
    fn it_never_takes_the_whole_screen() {
        let mut backend = terminal(10, 3, &[]);
        let mut viewport = Viewport::open(&mut backend, 9).expect("opened");
        assert_eq!(viewport.area().height, 2);
        viewport.set_height(&mut backend, 9).expect("grown");
        assert_eq!(viewport.area(), Rect::new(0, 0, 10, 2));
        fill(&mut viewport, &mut backend, "v");
        viewport
            .insert(&mut backend, &[Line::raw("m")])
            .expect("inserted");
        assert_eq!(screen(&backend), ["m", "v0", "v1"]);
    }

    /// With one row of the screen above it, lines go in through that row and
    /// the viewport stays whole -- as a terminal reads what is written, where
    /// the test backend would scroll a region of one row.
    #[test]
    fn a_viewport_one_row_from_the_top_stays_whole_as_lines_go_in() {
        let mut terminal = vt100::Parser::new(3, 10, 10);
        terminal.process(b"conv");
        let mut out = CrosstermBackend::new(&mut terminal);
        let mut viewport = Viewport {
            screen: Size::new(10, 3),
            panel: Panel::cleared(Rect::new(0, 1, 10, 2)),
            cursor_row: 0,
        };
        fill(&mut viewport, &mut out, "v");
        viewport
            .insert(&mut out, &[Line::raw("one"), Line::raw("two")])
            .expect("inserted");
        fill(&mut viewport, &mut out, "v");
        out.flush().expect("flushed");
        let rows: Vec<String> = terminal.screen().rows(0, 10).collect();
        assert_eq!(rows, ["two", "v0", "v1"]);
        terminal.screen_mut().set_scrollback(2);
        let history: Vec<String> = terminal.screen().rows(0, 10).take(2).collect();
        assert_eq!(history, ["conv", "one"]);
    }

    /// After a resize it is found again from where the cursor went.
    #[test]
    fn a_resize_finds_it_by_the_cursor() {
        let mut backend = terminal(10, 6, &["banner"]);
        let mut viewport = Viewport::open(&mut backend, 2).expect("opened");
        viewport
            .draw(&mut backend, |buf| Some(Position::new(3, buf.area.y + 1)))
            .expect("drawn");
        // The terminal grew, and moved what it showed down two rows.
        backend.resize(12, 8);
        backend
            .set_cursor_position(Position::new(3, 4))
            .expect("moved");
        viewport.resized(&mut backend).expect("found");
        assert_eq!(viewport.area(), Rect::new(0, 3, 12, 2));
        assert_eq!(viewport.screen(), Size::new(12, 8));
    }

    #[test]
    fn only_changed_cells_are_drawn() {
        let mut backend = terminal(10, 3, &[]);
        let mut viewport = Viewport::open(&mut backend, 1).expect("opened");
        fill(&mut viewport, &mut backend, "v");
        // Written behind the panel's back: a frame that changes nothing does
        // not draw over it.
        backend
            .draw([(0, 0, &ratatui::buffer::Cell::new("X"))].into_iter())
            .expect("drawn");
        fill(&mut viewport, &mut backend, "v");
        assert_eq!(screen(&backend)[0], "X0");
        viewport.redraw(&mut backend).expect("cleared");
        fill(&mut viewport, &mut backend, "v");
        assert_eq!(screen(&backend)[0], "v0");
    }
}
