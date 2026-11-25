//! A wrapped, focus-following list above the input (also usable by `/copy select`).
//! Markers are supplied as text, so focus and selection never depend on color.
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};

#[derive(Debug, Default)]
pub(super) struct List {
    pub rows: Vec<String>,
    pub focus: usize,
    pub cursor: Option<(usize, usize)>,
}
impl List {
    /// Keep the focused row visible, even after a resize or a wrapped label.
    pub fn draw(&self, buf: &mut Buffer, area: Rect) -> Option<Position> {
        if area.width == 0 || area.height == 0 {
            return None;
        }
        let height = usize::from(area.height);
        let first = self
            .focus
            .saturating_sub(height - 1)
            .min(self.rows.len().saturating_sub(height));
        for (n, row) in self.rows.iter().skip(first).take(height).enumerate() {
            buf.set_stringn(
                area.x,
                area.y + n as u16,
                row,
                usize::from(area.width),
                ratatui::style::Style::default(),
            );
        }
        self.cursor
            .filter(|(row, _)| *row >= first && *row < first + height)
            .map(|(row, col)| {
                Position::new(
                    area.x + col.min(usize::from(area.width) - 1) as u16,
                    area.y + (row - first) as u16,
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn list_follows_focus_and_keeps_cursor_inside_a_resized_area() {
        let list = List {
            rows: (0..20).map(|n| format!("row {n}")).collect(),
            focus: 18,
            cursor: Some((18, 50)),
        };
        for width in [0, 1, 20] {
            for height in [0, 1, 3, 30] {
                let area = Rect::new(2, 3, width, height);
                let mut buf = Buffer::empty(area);
                if let Some(at) = list.draw(&mut buf, area) {
                    assert!(area.contains(at));
                }
            }
        }
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 3));
        list.draw(&mut buf, Rect::new(0, 0, 20, 3));
        assert_eq!(super::super::rows_of(&buf), ["row 16", "row 17", "row 18"]);
    }
}
