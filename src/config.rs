//! CLI option parsing.
//!
//! Hand-rolled (no arg-parsing dependency) since the surface is small. Options
//! precede a `--` separator; everything after `--` is the child command and its
//! arguments, passed through untouched.

use crate::scanner::DEFAULT_MAX_MATH_BYTES;

/// How to decide whether to emit inline graphics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphicsMode {
    /// Detect terminal support; fall back to raw LaTeX if unsupported.
    Auto,
    /// Always emit graphics.
    Force,
    /// Never emit graphics; pass LaTeX through verbatim.
    Off,
}

/// Parsed configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Also recognize inline `$...$` and `\(...\)`.
    pub inline: bool,
    /// Heuristically detect bare (delimiter-less) display LaTeX, e.g. Claude's
    /// equations. Best-effort; augments text with an image rather than replacing.
    pub detect_bare: bool,
    /// Em size in pixels before scaling.
    pub font_size: f64,
    /// DPI/size multiplier applied to `font_size`.
    pub scale: f64,
    /// Glyph color as RGB (0-255).
    pub color: [u8; 3],
    /// Disable the render cache.
    pub no_cache: bool,
    /// Safety-valve byte cap for an unterminated block.
    pub max_math_bytes: usize,
    /// Graphics emission policy.
    pub graphics: GraphicsMode,
    /// Emit the diagnostic self-test image and exit.
    pub selftest_image: bool,
    /// Tee the raw child output to this file (diagnostic; for characterizing a
    /// program's output stream, e.g. a TUI's cursor-control escapes).
    pub capture: Option<String>,
    /// The child command (args after `--`); empty means wrap `$SHELL`.
    pub command: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            inline: false,
            detect_bare: false,
            font_size: 40.0,
            scale: 1.0,
            color: [255, 255, 255],
            no_cache: false,
            max_math_bytes: DEFAULT_MAX_MATH_BYTES,
            graphics: GraphicsMode::Auto,
            selftest_image: false,
            capture: None,
            command: Vec::new(),
        }
    }
}

impl Config {
    /// Final em size in pixels = `font_size * scale`.
    pub fn font_px(&self) -> f64 {
        self.font_size * self.scale
    }
}

/// Result of parsing: run with a config, or short-circuit with a message.
pub enum ParseOutcome {
    Run(Config),
    /// Print this text to stdout and exit 0 (e.g. `--help`).
    Exit(String),
    /// Print this error to stderr and exit 2.
    Error(String),
}

pub const USAGE: &str = "\
mathterm — render LaTeX math inline in any Kitty-graphics terminal

USAGE:
    mathterm [OPTIONS] [-- <command> [args...]]

    mathterm -- claude            wrap a command
    mathterm -- python script.py
    mathterm                      no command: wrap your $SHELL

OPTIONS:
    --inline                Also render inline $...$ and \\(...\\)
    --detect-bare           Detect bare (delimiter-less) display LaTeX, e.g.
                            Claude's equations (best-effort; appends an image)
    --font-size <px>        Em size in pixels (default 40)
    --scale <f>             DPI/size multiplier (default 1.0)
    --color <hex|name>      Glyph color, e.g. #ffffff or white (default white)
    --no-cache              Disable the render cache
    --max-math-bytes <n>    Unterminated-block byte cap (default 4096)
    --no-graphics           Never emit images; pass LaTeX through verbatim
    --force-graphics        Always emit images (skip capability detection)
    --selftest-image        Emit a test image and exit (checks terminal support)
    --capture <file>        Tee the child's raw output to <file> (diagnostic)
    -h, --help              Show this help
";

/// Parse `args` (excluding argv[0]).
pub fn parse<I: IntoIterator<Item = String>>(args: I) -> ParseOutcome {
    let mut cfg = Config::default();
    let mut iter = args.into_iter().peekable();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--" => {
                cfg.command.extend(iter.by_ref());
                break;
            }
            "-h" | "--help" => return ParseOutcome::Exit(USAGE.to_string()),
            "--inline" => cfg.inline = true,
            "--detect-bare" => cfg.detect_bare = true,
            "--no-cache" => cfg.no_cache = true,
            "--no-graphics" => cfg.graphics = GraphicsMode::Off,
            "--force-graphics" => cfg.graphics = GraphicsMode::Force,
            "--selftest-image" => cfg.selftest_image = true,
            "--capture" => match parse_value(&mut iter, "--capture") {
                Ok(v) => cfg.capture = Some(v),
                Err(e) => return ParseOutcome::Error(e),
            },
            "--font-size" => match parse_value(&mut iter, "--font-size") {
                Ok(v) => match v.parse::<f64>() {
                    Ok(n) if n > 0.0 => cfg.font_size = n,
                    _ => return ParseOutcome::Error(format!("invalid --font-size: {v}")),
                },
                Err(e) => return ParseOutcome::Error(e),
            },
            "--scale" => match parse_value(&mut iter, "--scale") {
                Ok(v) => match v.parse::<f64>() {
                    Ok(n) if n > 0.0 => cfg.scale = n,
                    _ => return ParseOutcome::Error(format!("invalid --scale: {v}")),
                },
                Err(e) => return ParseOutcome::Error(e),
            },
            "--max-math-bytes" => match parse_value(&mut iter, "--max-math-bytes") {
                Ok(v) => match v.parse::<usize>() {
                    Ok(n) if n > 0 => cfg.max_math_bytes = n,
                    _ => return ParseOutcome::Error(format!("invalid --max-math-bytes: {v}")),
                },
                Err(e) => return ParseOutcome::Error(e),
            },
            "--color" => match parse_value(&mut iter, "--color") {
                Ok(v) => match parse_color(&v) {
                    Some(rgb) => cfg.color = rgb,
                    None => return ParseOutcome::Error(format!("invalid --color: {v}")),
                },
                Err(e) => return ParseOutcome::Error(e),
            },
            other if other.starts_with('-') => {
                return ParseOutcome::Error(format!("unknown option: {other}\n\n{USAGE}"));
            }
            // A bare (non-flag) token with no preceding `--` is taken as the
            // start of the command, for convenience (`mathterm claude`).
            other => {
                cfg.command.push(other.to_string());
                cfg.command.extend(iter.by_ref());
                break;
            }
        }
    }

    ParseOutcome::Run(cfg)
}

fn parse_value<I: Iterator<Item = String>>(iter: &mut I, flag: &str) -> Result<String, String> {
    iter.next().ok_or_else(|| format!("{flag} requires a value"))
}

/// Parse `#rrggbb`, `#rgb`, or a few common color names.
fn parse_color(s: &str) -> Option<[u8; 3]> {
    match s.to_lowercase().as_str() {
        "white" => return Some([255, 255, 255]),
        "black" => return Some([0, 0, 0]),
        "red" => return Some([255, 0, 0]),
        "green" => return Some([0, 255, 0]),
        "blue" => return Some([0, 0, 255]),
        "yellow" => return Some([255, 255, 0]),
        "cyan" => return Some([0, 255, 255]),
        "magenta" => return Some([255, 0, 255]),
        "gray" | "grey" => return Some([128, 128, 128]),
        _ => {}
    }
    let hex = s.strip_prefix('#')?;
    match hex.len() {
        6 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            Some([r, g, b])
        }
        3 => {
            let r = u8::from_str_radix(&hex[0..1], 16).ok()? * 17;
            let g = u8::from_str_radix(&hex[1..2], 16).ok()? * 17;
            let b = u8::from_str_radix(&hex[2..3], 16).ok()? * 17;
            Some([r, g, b])
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_args(args: &[&str]) -> Config {
        match parse(args.iter().map(|s| s.to_string())) {
            ParseOutcome::Run(c) => c,
            ParseOutcome::Exit(m) | ParseOutcome::Error(m) => panic!("unexpected: {m}"),
        }
    }

    #[test]
    fn defaults() {
        let c = parse_args(&[]);
        assert!(!c.inline);
        assert_eq!(c.font_size, 40.0);
        assert_eq!(c.color, [255, 255, 255]);
        assert_eq!(c.graphics, GraphicsMode::Auto);
        assert!(c.command.is_empty());
    }

    #[test]
    fn command_after_separator() {
        let c = parse_args(&["--inline", "--", "claude", "--flag"]);
        assert!(c.inline);
        assert_eq!(c.command, vec!["claude", "--flag"]);
    }

    #[test]
    fn flags_after_separator_belong_to_child() {
        // `--font-size` after `--` is the child's arg, not ours.
        let c = parse_args(&["--", "prog", "--font-size", "9"]);
        assert_eq!(c.font_size, 40.0);
        assert_eq!(c.command, vec!["prog", "--font-size", "9"]);
    }

    #[test]
    fn bare_command_without_separator() {
        let c = parse_args(&["python", "x.py"]);
        assert_eq!(c.command, vec!["python", "x.py"]);
    }

    #[test]
    fn numeric_and_color_options() {
        let c = parse_args(&["--font-size", "28", "--scale", "2", "--color", "#ff8800"]);
        assert_eq!(c.font_size, 28.0);
        assert_eq!(c.scale, 2.0);
        assert_eq!(c.font_px(), 56.0);
        assert_eq!(c.color, [255, 136, 0]);
    }

    #[test]
    fn color_names_and_short_hex() {
        assert_eq!(parse_color("white"), Some([255, 255, 255]));
        assert_eq!(parse_color("#fff"), Some([255, 255, 255]));
        assert_eq!(parse_color("#f00"), Some([255, 0, 0]));
        assert_eq!(parse_color("nonsense"), None);
    }

    #[test]
    fn help_short_circuits() {
        assert!(matches!(
            parse(["--help".to_string()]),
            ParseOutcome::Exit(_)
        ));
    }

    #[test]
    fn unknown_flag_errors() {
        assert!(matches!(
            parse(["--bogus".to_string()]),
            ParseOutcome::Error(_)
        ));
    }

    #[test]
    fn invalid_numeric_errors() {
        assert!(matches!(
            parse(["--font-size".to_string(), "huge".to_string()]),
            ParseOutcome::Error(_)
        ));
        assert!(matches!(
            parse(["--scale".to_string(), "-1".to_string()]),
            ParseOutcome::Error(_)
        ));
    }
}
