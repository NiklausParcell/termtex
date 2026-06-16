//! Math rendering pipeline.
//!
//! A [`MathRenderer`] turns LaTeX source into a rasterized [`RenderedImage`].
//! The concrete implementation is kept behind the trait so it can be swapped;
//! v1 uses [`RatexRenderer`], a pure-Rust LaTeX -> SVG -> PNG path (no external
//! processes, fonts embedded in the binary). [`CachingRenderer`] wraps any
//! renderer with an in-memory LRU so repeated equations are free.

mod ratex;

pub use ratex::RatexRenderer;

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::num::NonZeroUsize;
use std::sync::Mutex;

use lru::LruCache;

/// A rendered equation as PNG bytes plus its pixel dimensions.
#[derive(Debug, Clone)]
pub struct RenderedImage {
    pub png: Vec<u8>,
    pub width_px: u32,
    pub height_px: u32,
}

/// Why a render failed. Callers fall back to emitting the raw LaTeX verbatim.
#[derive(Debug)]
pub enum RenderError {
    /// The LaTeX source could not be parsed.
    Parse(String),
    /// Layout produced an SVG that could not be rasterized.
    Raster(String),
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RenderError::Parse(m) => write!(f, "parse error: {m}"),
            RenderError::Raster(m) => write!(f, "raster error: {m}"),
        }
    }
}

impl std::error::Error for RenderError {}

/// Renders LaTeX math to a raster image.
pub trait MathRenderer {
    /// Render `latex` (delimiters already stripped). `display` selects display
    /// style (true) versus inline/text style (false).
    fn render(&self, latex: &str, display: bool) -> Result<RenderedImage, RenderError>;
}

/// Wraps a renderer with an LRU cache keyed by `(latex, display)`. The wrapped
/// renderer's configuration (e.g. font size) is fixed for the cache's lifetime,
/// so it need not be part of the key.
pub struct CachingRenderer<R: MathRenderer> {
    inner: R,
    cache: Mutex<LruCache<u64, RenderedImage>>,
}

impl<R: MathRenderer> CachingRenderer<R> {
    pub fn new(inner: R, capacity: usize) -> Self {
        let cap = NonZeroUsize::new(capacity.max(1)).expect("capacity >= 1");
        Self {
            inner,
            cache: Mutex::new(LruCache::new(cap)),
        }
    }

    fn key(latex: &str, display: bool) -> u64 {
        let mut h = DefaultHasher::new();
        latex.hash(&mut h);
        display.hash(&mut h);
        h.finish()
    }
}

impl<R: MathRenderer> MathRenderer for CachingRenderer<R> {
    fn render(&self, latex: &str, display: bool) -> Result<RenderedImage, RenderError> {
        let key = Self::key(latex, display);
        if let Some(img) = self.cache.lock().unwrap().get(&key) {
            return Ok(img.clone());
        }
        let img = self.inner.render(latex, display)?;
        self.cache.lock().unwrap().put(key, img.clone());
        Ok(img)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A renderer that counts calls and returns a trivial 1x1 image.
    struct CountingRenderer {
        calls: AtomicUsize,
    }

    impl MathRenderer for CountingRenderer {
        fn render(&self, latex: &str, _display: bool) -> Result<RenderedImage, RenderError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if latex == "bad" {
                return Err(RenderError::Parse("nope".into()));
            }
            Ok(RenderedImage {
                png: latex.as_bytes().to_vec(),
                width_px: 1,
                height_px: 1,
            })
        }
    }

    #[test]
    fn cache_avoids_repeat_renders() {
        let inner = CountingRenderer {
            calls: AtomicUsize::new(0),
        };
        let r = CachingRenderer::new(inner, 16);
        let _ = r.render("x^2", true).unwrap();
        let _ = r.render("x^2", true).unwrap();
        let _ = r.render("x^2", true).unwrap();
        assert_eq!(r.inner.calls.load(Ordering::SeqCst), 1, "rendered once, cached after");
    }

    #[test]
    fn cache_distinguishes_display_flag_and_source() {
        let inner = CountingRenderer {
            calls: AtomicUsize::new(0),
        };
        let r = CachingRenderer::new(inner, 16);
        let _ = r.render("x", true).unwrap();
        let _ = r.render("x", false).unwrap(); // different display -> new render
        let _ = r.render("y", true).unwrap(); // different source -> new render
        assert_eq!(r.inner.calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn errors_are_not_cached() {
        let inner = CountingRenderer {
            calls: AtomicUsize::new(0),
        };
        let r = CachingRenderer::new(inner, 16);
        assert!(r.render("bad", true).is_err());
        assert!(r.render("bad", true).is_err());
        assert_eq!(r.inner.calls.load(Ordering::SeqCst), 2, "failed renders retry");
    }
}
