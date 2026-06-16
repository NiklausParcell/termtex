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

/// Renders math at a fixed pixel font size (user-units per em in the SVG).
pub struct RatexRenderer {
    font_px: f64,
}

impl RatexRenderer {
    /// `font_px` is the em size in pixels; larger = crisper/bigger. Callers
    /// fold any DPI/scale multiplier into this value.
    pub fn new(font_px: f64) -> Self {
        Self {
            font_px: font_px.max(1.0),
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

    #[test]
    fn invalid_latex_is_an_error_not_a_panic() {
        let r = RatexRenderer::new(40.0);
        // Unbalanced brace should fail to parse rather than crash.
        let result = r.render("\\frac{1}{", true);
        assert!(result.is_err());
    }
}
