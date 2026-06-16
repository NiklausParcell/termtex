//! Pure-Rust LaTeX renderer: parse -> layout -> standalone SVG -> PNG.
//!
//! Uses the RaTeX crate suite (`ratex-parser`/`-layout`/`-svg`) to produce a
//! self-contained SVG with embedded glyph outlines (KaTeX fonts compiled into
//! the binary via the `embed-fonts` feature), then rasterizes it with `resvg`.
//! No subprocess, no system fonts, ~2-5ms per equation.

use ratex_layout::{layout, to_display_list, LayoutOptions};
use ratex_svg::{render_to_svg, SvgOptions};
use ratex_types::math_style::MathStyle;

use super::{MathRenderer, RenderError, RenderedImage};

/// Renders math at a fixed pixel font size and glyph color.
pub struct RatexRenderer {
    font_px: f64,
    color: [u8; 3],
}

impl RatexRenderer {
    /// `font_px` is the em size in pixels; larger = crisper/bigger. Callers
    /// fold any DPI/scale multiplier into this value. Glyphs default to white so
    /// equations are visible on the dark backgrounds typical of terminals (they
    /// are rendered on a transparent background); use [`RatexRenderer::with_color`]
    /// to override for light themes.
    pub fn new(font_px: f64) -> Self {
        Self {
            font_px: font_px.max(1.0),
            color: [255, 255, 255],
        }
    }

    /// Set the glyph color from 0-255 RGB components.
    pub fn with_color(mut self, r: u8, g: u8, b: u8) -> Self {
        self.color = [r, g, b];
        self
    }

    /// Recolor every non-transparent pixel to the target color, preserving the
    /// alpha channel. RaTeX bakes glyph color into the layout (and hardcodes
    /// black for some glyph paths), so rather than fight its color model we
    /// recolor the raster: the glyph shape lives entirely in the alpha channel,
    /// so overwriting RGB keeps antialiasing perfect. Monochrome only, which is
    /// exactly right for terminal math.
    fn recolor(&self, pixmap: &mut resvg::tiny_skia::Pixmap) {
        let [cr, cg, cb] = self.color;
        for px in pixmap.pixels_mut() {
            let a = px.alpha();
            if a == 0 {
                continue;
            }
            // tiny-skia stores premultiplied RGBA; premultiply the target color
            // by the existing alpha to keep the invariant (channel <= alpha).
            let pm = |c: u8| ((c as u16 * a as u16 + 127) / 255) as u8;
            if let Some(repl) =
                resvg::tiny_skia::PremultipliedColorU8::from_rgba(pm(cr), pm(cg), pm(cb), a)
            {
                *px = repl;
            }
        }
    }
}

impl MathRenderer for RatexRenderer {
    fn render(&self, latex: &str, display: bool) -> Result<RenderedImage, RenderError> {
        let nodes =
            ratex_parser::parse(latex).map_err(|e| RenderError::Parse(format!("{e:?}")))?;

        let mut layout_opts = LayoutOptions::default();
        layout_opts.style = if display {
            MathStyle::Display
        } else {
            MathStyle::Text
        };
        let layout_box = layout(&nodes, &layout_opts);
        let display_list = to_display_list(&layout_box);

        let mut svg_opts = SvgOptions::default();
        svg_opts.font_size = self.font_px;
        svg_opts.embed_glyphs = true; // self-contained <path> using embedded fonts
        let svg = render_to_svg(&display_list, &svg_opts);

        let tree = resvg::usvg::Tree::from_str(&svg, &resvg::usvg::Options::default())
            .map_err(|e| RenderError::Raster(format!("usvg: {e}")))?;
        let size = tree.size();
        let width = (size.width().ceil() as u32).max(1);
        let height = (size.height().ceil() as u32).max(1);

        let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height)
            .ok_or_else(|| RenderError::Raster("pixmap allocation failed".into()))?;
        resvg::render(
            &tree,
            resvg::tiny_skia::Transform::identity(),
            &mut pixmap.as_mut(),
        );
        self.recolor(&mut pixmap);
        let png = pixmap
            .encode_png()
            .map_err(|e| RenderError::Raster(format!("png encode: {e}")))?;

        Ok(RenderedImage {
            png,
            width_px: width,
            height_px: height,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_common_equations_to_png() {
        let r = RatexRenderer::new(40.0);
        for (latex, display) in [
            ("x^2 + y^2 = z^2", true),
            ("\\frac{-b \\pm \\sqrt{b^2-4ac}}{2a}", true),
            ("\\sum_{i=1}^{n} i", true),
            ("\\alpha\\beta", false),
        ] {
            let img = r.render(latex, display).expect("render ok");
            assert!(img.width_px > 0 && img.height_px > 0);
            // PNG magic number.
            assert_eq!(&img.png[..8], &[137, 80, 78, 71, 13, 10, 26, 10], "valid PNG: {latex}");
        }
    }

    #[test]
    fn display_is_taller_than_inline_for_same_source() {
        let r = RatexRenderer::new(40.0);
        let d = r.render("\\sum_{i=1}^{n} i", true).unwrap();
        let i = r.render("\\sum_{i=1}^{n} i", false).unwrap();
        // Display style sets limits above/below, so it should be taller.
        assert!(d.height_px >= i.height_px);
    }

    /// Decode a PNG to (width, RGBA bytes) without pulling an image crate.
    fn decode_png_rgba(png: &[u8]) -> (u32, Vec<u8>) {
        let decoder = png::Decoder::new(png);
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).unwrap();
        assert_eq!(info.color_type, png::ColorType::Rgba);
        buf.truncate(info.buffer_size());
        (info.width, buf)
    }

    #[test]
    fn glyphs_are_recolored_white_by_default() {
        let r = RatexRenderer::new(40.0);
        let img = r.render("E=mc^2", true).unwrap();
        let (_w, rgba) = decode_png_rgba(&img.png);
        let mut white = 0;
        let mut other_opaque = 0;
        for px in rgba.chunks_exact(4) {
            if px[3] > 200 {
                if px[0] > 200 && px[1] > 200 && px[2] > 200 {
                    white += 1;
                } else {
                    other_opaque += 1;
                }
            }
        }
        assert!(white > 0, "should have opaque white glyph pixels");
        assert!(
            white > other_opaque * 4,
            "opaque pixels should be overwhelmingly white (white={white}, other={other_opaque})"
        );
    }

    #[test]
    fn with_color_overrides_glyph_color() {
        let r = RatexRenderer::new(40.0).with_color(255, 0, 0);
        let img = r.render("x", true).unwrap();
        let (_w, rgba) = decode_png_rgba(&img.png);
        let red = rgba
            .chunks_exact(4)
            .filter(|px| px[3] > 200 && px[0] > 200 && px[1] < 60 && px[2] < 60)
            .count();
        assert!(red > 0, "should have opaque red glyph pixels");
    }

    #[test]
    fn invalid_latex_is_an_error_not_a_panic() {
        let r = RatexRenderer::new(40.0);
        // Unbalanced brace should fail to parse rather than crash.
        let result = r.render("\\frac{1}{", true);
        assert!(result.is_err());
    }
}
