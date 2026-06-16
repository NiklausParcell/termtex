//! Reserved bottom-strip rendering for self-repainting TUIs.
//!
//! Inline image injection corrupts a TUI that owns and repaints the screen
//! (e.g. interactive Claude Code). Instead, give the child a terminal that is
//! `strip_rows` rows shorter, confine its scrolling to the top region with a
//! scroll region (`DECSTBM`), and let mathterm own the bottom strip — disjoint
//! regions, so the two never collide.
//!
//! When an equation is detected, we save the cursor (`DECSC`), jump to the
//! strip, clear it, draw the latest equation image, and restore the cursor
//! (`DECRC`) — so the child's cursor model is untouched and its repaints keep
//! working.

use std::io::{self, Write};

use crate::kitty;

/// State for the reserved strip. `real_rows` is the true terminal height; the
/// child is told it has `real_rows - strip_rows`.
#[derive(Clone)]
pub struct Strip {
    real_rows: u16,
    real_cols: u16,
    strip_rows: u16,
    cell: Option<(u32, u32)>,
}

impl Strip {
    pub fn new(real_rows: u16, real_cols: u16, strip_rows: u16, cell: Option<(u32, u32)>) -> Self {
        // Leave at least one content row for the child.
        let strip_rows = strip_rows.min(real_rows.saturating_sub(1)).max(1);
        Strip {
            real_rows,
            real_cols,
            strip_rows,
            cell,
        }
    }

    /// Rows the child should believe it has.
    pub fn child_rows(&self) -> u16 {
        self.real_rows - self.strip_rows
    }

    /// 1-based row where the strip begins.
    fn strip_top(&self) -> u16 {
        self.child_rows() + 1
    }

    /// Install the scroll region (top content area only) and clear the strip.
    /// The child then scrolls within rows `1..=child_rows` and never touches the
    /// strip.
    pub fn setup(&self, out: &mut impl Write) -> io::Result<()> {
        // Scroll region = content area; cursor home into it.
        write!(out, "\x1b[1;{}r", self.child_rows())?;
        write!(out, "\x1b[1;1H")?;
        self.clear_strip(out)?;
        out.flush()
    }

    /// Reset the scroll region and clear the strip on exit.
    pub fn teardown(&self, out: &mut impl Write) -> io::Result<()> {
        // Save/restore so we don't disturb a final cursor position.
        write!(out, "\x1b7")?; // DECSC
        write!(out, "\x1b[r")?; // reset scroll region (full screen)
        self.clear_strip(out)?;
        write!(out, "\x1b8")?; // DECRC
        out.flush()
    }

    fn clear_strip(&self, out: &mut impl Write) -> io::Result<()> {
        // Move to the strip top and erase from there to the end of the screen.
        write!(out, "\x1b[{};1H", self.strip_top())?;
        write!(out, "\x1b[0J")?; // erase to end of display
        Ok(())
    }

    /// Draw the latest equation image into the strip, preserving the child's
    /// cursor position (DECSC/DECRC around our drawing).
    pub fn draw(&self, out: &mut impl Write, png: &[u8], img_w: u32, img_h: u32) -> io::Result<()> {
        let (cols, rows) = self.fit(img_w, img_h);
        write!(out, "\x1b7")?; // save child cursor
        write!(out, "\x1b[{};1H", self.strip_top())?;
        write!(out, "\x1b[0J")?; // clear strip
        write!(out, "\x1b[2m── equation ──\x1b[0m")?; // dim label
        write!(out, "\x1b[{};1H", self.strip_top() + 1)?; // next strip row
        kitty::emit_png(out, png, cols, rows)?;
        write!(out, "\x1b8")?; // restore child cursor
        out.flush()
    }

    /// Fit the image into the strip: at most `strip_rows - 1` rows (one row is
    /// the label) and `real_cols` columns, preserving aspect.
    fn fit(&self, img_w: u32, img_h: u32) -> (Option<u32>, Option<u32>) {
        let (cell_w, cell_h) = match self.cell {
            Some(c) if c.0 > 0 && c.1 > 0 => c,
            _ => return (None, None),
        };
        let max_rows = (self.strip_rows.saturating_sub(1)).max(1) as u32;
        let max_cols = self.real_cols.max(1) as u32;
        let nat_rows = img_h.div_ceil(cell_h).max(1);
        let nat_cols = img_w.div_ceil(cell_w).max(1);

        // Scale factor so the image fits within both bounds.
        let row_scale = max_rows as f64 / nat_rows as f64;
        let col_scale = max_cols as f64 / nat_cols as f64;
        let scale = row_scale.min(col_scale).min(1.0);
        let cols = ((nat_cols as f64 * scale) as u32).max(1);
        let rows = ((nat_rows as f64 * scale) as u32).max(1);
        (Some(cols), Some(rows))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_rows_reserves_the_strip() {
        let s = Strip::new(40, 100, 8, None);
        assert_eq!(s.child_rows(), 32);
        assert_eq!(s.strip_top(), 33);
    }

    #[test]
    fn strip_capped_to_leave_a_content_row() {
        let s = Strip::new(5, 80, 99, None);
        assert!(s.child_rows() >= 1);
    }

    #[test]
    fn setup_emits_scroll_region() {
        let s = Strip::new(40, 100, 8, None);
        let mut out = Vec::new();
        s.setup(&mut out).unwrap();
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("\x1b[1;32r"), "scroll region to content area");
    }

    #[test]
    fn teardown_resets_scroll_region() {
        let s = Strip::new(40, 100, 8, None);
        let mut out = Vec::new();
        s.teardown(&mut out).unwrap();
        assert!(String::from_utf8_lossy(&out).contains("\x1b[r"));
    }

    #[test]
    fn fit_scales_within_strip_bounds() {
        // cell 10x20, strip 8 rows -> max 7 image rows, 100 cols.
        let s = Strip::new(40, 100, 8, Some((10, 20)));
        // image 2000x400 px = 200 cols x 20 rows naturally; must scale to fit.
        let (cols, rows) = s.fit(2000, 400);
        assert!(rows.unwrap() <= 7);
        assert!(cols.unwrap() <= 100);
    }
}
