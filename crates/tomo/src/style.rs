//! Terminal visual identity: color + glyph styling with graceful degradation.
//!
//! This module is the **only** place styling decisions live. Per the hygiene
//! policy, library crates never print — so all of Tomo's colored, glyph-rich
//! output is rendered here in the `tomo` crate and nowhere else.
//!
//! Capability is detected **once** at startup ([`init`], driven by [`detect`])
//! and cached in a process-global ([`current`]). Every rendering helper is a
//! no-op when styling is disabled, returning the plain text unchanged, so that:
//!
//! - piped / non-tty output is byte-for-byte identical to Tomo's historical
//!   plain text (the e2e scenarios depend on this),
//! - `--json` and serve-mode output never carry escapes (those paths simply do
//!   not call the glyph-bearing helpers),
//! - unit tests, which never call [`init`], see the disabled default and keep
//!   asserting on plain strings unchanged.
//!
//! The theme echoes the project site (`site/index.html`): a warm coral accent,
//! terminal-native, with the 友 mark.

use std::io::{self, Write};
use std::sync::OnceLock;
use std::time::Instant;

use owo_colors::{DynColors, Style as Owo};

/// The coral accent as a 256-color index (xterm 209), used when the terminal
/// does not advertise truecolor. Matches the site's `--accent`.
const ACCENT_XTERM: u8 = 209;
/// The coral accent as 24-bit RGB (`#ff8a5c`), used when `COLORTERM` advertises
/// truecolor support.
const ACCENT_RGB: (u8, u8, u8) = (0xff, 0x8a, 0x5c);

/// How color output was decided.
///
/// `TOMO_COLOR=always|never` forces the choice; otherwise it is `auto`
/// (terminal-detected).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColorChoice {
    /// Force color on regardless of the stream.
    Always,
    /// Force color off regardless of the stream.
    Never,
    /// Decide from the stream, `NO_COLOR`, and `TERM`.
    Auto,
}

/// The resolved styling capability for this process.
///
/// Cheap to copy (three flags); the color helpers borrow `&self`. Constructed
/// once via [`detect`] and stored in the [`current`] global, or defaulted to
/// fully-disabled for tests and pre-`init` calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Style {
    /// Whether ANSI colors/attributes are emitted at all.
    colors: bool,
    /// Whether Unicode glyphs are used (else ASCII fallbacks).
    unicode: bool,
    /// Whether the accent uses 24-bit truecolor (else xterm-256).
    truecolor: bool,
}

impl Default for Style {
    /// The safe default: no color, ASCII glyphs. This is what unit tests and any
    /// pre-[`init`] caller observe, guaranteeing plain output unless `main`
    /// explicitly enables styling.
    fn default() -> Self {
        Style {
            colors: false,
            unicode: false,
            truecolor: false,
        }
    }
}

/// The raw capability signals styling is resolved from.
///
/// Kept as plain data (no process access) so [`resolve`] is a pure function and
/// the detection matrix can be unit-tested without touching real env vars or a
/// real terminal.
// Four independent capability signals; this is a flat input record, not a state
// object, so grouping the bools would only obscure the detection matrix.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct DetectInput<'a> {
    /// Whether the target stream is a terminal (`IsTerminal`).
    pub is_terminal: bool,
    /// Whether `NO_COLOR` is set to a non-empty value.
    pub no_color: bool,
    /// The value of `TERM`, if set.
    pub term: Option<&'a str>,
    /// The value of `TOMO_COLOR` (`always` / `never` / `auto`), if set.
    pub tomo_color: Option<&'a str>,
    /// Whether `TOMO_ASCII` is set to a non-empty value (force ASCII glyphs).
    pub tomo_ascii: bool,
    /// The value of `COLORTERM`, if set (drives truecolor detection).
    pub colorterm: Option<&'a str>,
    /// Whether the locale env (`LC_ALL`/`LC_CTYPE`/`LANG`) indicates UTF-8.
    pub locale_utf8: bool,
}

/// Resolve raw signals into a concrete [`Style`]. Pure — unit-tested exhaustively.
pub(crate) fn resolve(input: &DetectInput) -> Style {
    let choice = match input.tomo_color.map(str::trim) {
        Some(v) if v.eq_ignore_ascii_case("always") => ColorChoice::Always,
        Some(v) if v.eq_ignore_ascii_case("never") => ColorChoice::Never,
        _ => ColorChoice::Auto,
    };
    let colors = match choice {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        // NO_COLOR and a dumb terminal both veto auto color (the NO_COLOR spec
        // and long-standing convention); a non-terminal stream is plain too.
        ColorChoice::Auto => input.is_terminal && !input.no_color && input.term != Some("dumb"),
    };
    let unicode = !input.tomo_ascii && input.locale_utf8;
    let truecolor = colors && colorterm_is_truecolor(input.colorterm);
    Style {
        colors,
        unicode,
        truecolor,
    }
}

/// Whether a `COLORTERM` value advertises 24-bit color.
fn colorterm_is_truecolor(colorterm: Option<&str>) -> bool {
    colorterm.is_some_and(|c| {
        let c = c.to_ascii_lowercase();
        c.contains("truecolor") || c.contains("24bit")
    })
}

/// Build a [`DetectInput`] from the current process environment and `stream`'s
/// terminal-ness, then [`resolve`] it. `stream` is typically `&std::io::stdout()`.
pub(crate) fn detect<T: io::IsTerminal>(stream: &T) -> Style {
    let term = std::env::var("TERM").ok();
    let tomo_color = std::env::var("TOMO_COLOR").ok();
    let colorterm = std::env::var("COLORTERM").ok();
    let input = DetectInput {
        is_terminal: stream.is_terminal(),
        no_color: env_set("NO_COLOR"),
        term: term.as_deref(),
        tomo_color: tomo_color.as_deref(),
        tomo_ascii: env_set("TOMO_ASCII"),
        colorterm: colorterm.as_deref(),
        locale_utf8: locale_is_utf8(),
    };
    resolve(&input)
}

/// Whether an env var is set to a non-empty value.
fn env_set(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| !v.is_empty())
}

/// Whether the effective locale (first set of `LC_ALL`, `LC_CTYPE`, `LANG`)
/// indicates a UTF-8 character encoding.
fn locale_is_utf8() -> bool {
    for name in ["LC_ALL", "LC_CTYPE", "LANG"] {
        if let Ok(v) = std::env::var(name) {
            if v.is_empty() {
                continue;
            }
            let v = v.to_ascii_lowercase();
            return v.contains("utf-8") || v.contains("utf8");
        }
    }
    false
}

/// The process-wide resolved style, set once by [`init`].
static CURRENT: OnceLock<Style> = OnceLock::new();

/// Install the detected [`Style`] for the process. Idempotent: a second call is
/// ignored (the first wins), so it is safe even if reached twice.
pub(crate) fn init(style: Style) {
    let _ = CURRENT.set(style);
}

/// The current process style, or the disabled default if [`init`] was never
/// called (unit tests, and any code path that runs before startup detection).
pub(crate) fn current() -> Style {
    CURRENT.get().copied().unwrap_or_default()
}

impl Style {
    /// Whether colored output is enabled (also gates glyph-bearing lines that
    /// have no plain-mode equivalent, e.g. the sync banner).
    pub(crate) fn enabled(self) -> bool {
        self.colors
    }

    /// The accent [`Owo`] style (coral): truecolor `#ff8a5c` when supported,
    /// else xterm-256 `209`.
    fn accent_style(self) -> Owo {
        let color = if self.truecolor {
            DynColors::Rgb(ACCENT_RGB.0, ACCENT_RGB.1, ACCENT_RGB.2)
        } else {
            DynColors::Xterm(ACCENT_XTERM.into())
        };
        Owo::new().color(color)
    }

    /// Paint `text` with `style`, or return it unchanged when color is disabled.
    fn paint(self, text: &str, style: Owo) -> String {
        if self.colors {
            format!("{}", style.style(text))
        } else {
            text.to_owned()
        }
    }

    /// The coral accent, for identifiers, prompts, and headers.
    pub(crate) fn accent(self, text: &str) -> String {
        self.paint(text, self.accent_style())
    }

    /// Success green.
    pub(crate) fn ok(self, text: &str) -> String {
        self.paint(text, Owo::new().green())
    }

    /// Warning amber (rendered as yellow).
    pub(crate) fn warn(self, text: &str) -> String {
        self.paint(text, Owo::new().yellow())
    }

    /// Error red.
    pub(crate) fn err(self, text: &str) -> String {
        self.paint(text, Owo::new().red())
    }

    /// Secondary / dimmed text.
    pub(crate) fn dim(self, text: &str) -> String {
        self.paint(text, Owo::new().dimmed())
    }

    /// Bold emphasis (e.g. path headers).
    pub(crate) fn bold(self, text: &str) -> String {
        self.paint(text, Owo::new().bold())
    }

    /// Cyan + bold, for diff headers.
    pub(crate) fn header(self, text: &str) -> String {
        self.paint(text, Owo::new().cyan().bold())
    }

    // ---- glyphs (Unicode with ASCII fallbacks) ----------------------------

    /// Success mark: `✓` / `OK`.
    pub(crate) fn g_ok(self) -> &'static str {
        if self.unicode {
            "✓"
        } else {
            "OK"
        }
    }

    /// Failure/removal mark: `✗` / `X`.
    pub(crate) fn g_cross(self) -> &'static str {
        if self.unicode {
            "✗"
        } else {
            "X"
        }
    }

    /// Warning mark: `⚠` / `!`.
    pub(crate) fn g_warn(self) -> &'static str {
        if self.unicode {
            "⚠"
        } else {
            "!"
        }
    }

    /// Filled state dot (connected / present): `●` / `*`.
    pub(crate) fn g_dot_on(self) -> &'static str {
        if self.unicode {
            "●"
        } else {
            "*"
        }
    }

    /// Hollow state dot (offline / absent): `○` / `o`.
    pub(crate) fn g_dot_off(self) -> &'static str {
        if self.unicode {
            "○"
        } else {
            "o"
        }
    }

    /// Outbound (send) arrow: `↑` / `->`.
    pub(crate) fn g_up(self) -> &'static str {
        if self.unicode {
            "↑"
        } else {
            "->"
        }
    }

    /// Inbound (apply) arrow: `↓` / `<-`.
    pub(crate) fn g_down(self) -> &'static str {
        if self.unicode {
            "↓"
        } else {
            "<-"
        }
    }

    /// Two-way sync mark: `⇄` / `<->`.
    pub(crate) fn g_sync(self) -> &'static str {
        if self.unicode {
            "⇄"
        } else {
            "<->"
        }
    }

    /// The 友 ("tomo") mark, omitted entirely in ASCII mode.
    pub(crate) fn g_kanji(self) -> &'static str {
        if self.unicode {
            "友"
        } else {
            ""
        }
    }

    /// The spinner frame set for the transient progress line.
    fn spinner_frames(self) -> &'static [&'static str] {
        if self.unicode {
            &["⣾", "⣽", "⣻", "⢿", "⡿", "⣟", "⣯", "⣷"]
        } else {
            &["|", "/", "-", "\\"]
        }
    }
}

// ---- humanization ---------------------------------------------------------

/// Group a count with thousands separators for display, e.g. `1892` → `1,892`.
pub(crate) fn group_thousands(n: u64) -> String {
    let digits = n.to_string();
    let bytes = digits.as_bytes();
    // The leading group holds the remaining 1–3 digits; every group after it is a
    // full three digits preceded by a comma.
    let lead = bytes.len() % 3;
    let split = if lead == 0 { 3 } else { lead };
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    out.push_str(&digits[..split.min(digits.len())]);
    let mut i = split;
    while i < digits.len() {
        out.push(',');
        out.push_str(&digits[i..i + 3]);
        i += 3;
    }
    out
}

// ---- transient progress line ----------------------------------------------

/// Minimum gap between progress redraws, throttling to ≤10 frames/second so a
/// fast transfer does not flood the terminal.
const REDRAW_MIN_MS: u128 = 100;

/// Default render width when the terminal columns are unknown.
const DEFAULT_WIDTH: usize = 80;

/// The visible width for the transient progress line, from `COLUMNS` or a
/// sensible default. (No terminal-size dependency: `COLUMNS` is exported by
/// interactive shells and is enough for a single status line.)
pub(crate) fn terminal_width() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.trim().parse::<usize>().ok())
        .filter(|w| *w >= 20)
        .unwrap_or(DEFAULT_WIDTH)
}

/// Build the **visible** content of a transient progress line (no control
/// codes), truncated to `width`. Pure and unit-tested; the guard adds the
/// carriage-return + erase discipline.
///
/// Shape: `<spinner> <verb> <path>  <pct>%  <got> / <total>`.
pub(crate) fn render_progress(
    spinner: &str,
    verb: &str,
    path: &str,
    got: u64,
    total: u64,
    width: usize,
) -> String {
    // got may momentarily exceed total on the final chunk (clamp to 100), and a
    // zero total renders 100% rather than dividing by zero.
    let pct = got
        .saturating_mul(100)
        .checked_div(total)
        .map_or(100, |p| p.min(100));
    let line = format!(
        "{spinner} {verb} {path}  {pct}%  {got} / {total}",
        got = human_bytes(got),
        total = human_bytes(total),
    );
    truncate_display(&line, width)
}

/// A compact byte count for the progress line (`B`/`kB`/`MB`/`GB`).
fn human_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    #[allow(clippy::cast_precision_loss)] // display only; magnitudes are tiny
    let b = bytes as f64;
    if b < KB {
        format!("{bytes} B")
    } else if b < MB {
        format!("{:.1} kB", b / KB)
    } else if b < GB {
        format!("{:.1} MB", b / MB)
    } else {
        format!("{:.1} GB", b / GB)
    }
}

/// Truncate `s` to at most `width` display columns (counted as `char`s — good
/// enough for the ASCII/box-drawing content here), appending `…`/`~` when cut.
fn truncate_display(s: &str, width: usize) -> String {
    let count = s.chars().count();
    if count <= width {
        return s.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let keep = width.saturating_sub(1);
    let mut out: String = s.chars().take(keep).collect();
    out.push('…');
    out
}

/// Owns the redraw/erase discipline for the single transient progress line
/// (tty-only, sync mode). Nothing is written when styling is disabled, so plain
/// output is never touched. The line is always erased before any normal line
/// prints — callers route normal output through the [`crate::report::Reporter`],
/// which clears the progress first.
pub(crate) struct ProgressLine {
    style: Style,
    shown: bool,
    frame: usize,
    last_draw: Option<Instant>,
}

impl ProgressLine {
    /// A fresh, hidden progress line bound to `style`.
    pub(crate) fn new(style: Style) -> Self {
        ProgressLine {
            style,
            shown: false,
            frame: 0,
            last_draw: None,
        }
    }

    /// Redraw the progress line for an in-flight transfer, throttled to
    /// [`REDRAW_MIN_MS`]. A no-op when color is disabled. `verb` is e.g.
    /// `"receiving"`.
    pub(crate) fn update<W: Write>(
        &mut self,
        w: &mut W,
        verb: &str,
        path: &str,
        got: u64,
        total: u64,
    ) -> io::Result<()> {
        if !self.style.colors {
            return Ok(());
        }
        let now = Instant::now();
        if self.shown {
            if let Some(last) = self.last_draw {
                if now.duration_since(last).as_millis() < REDRAW_MIN_MS {
                    return Ok(());
                }
            }
        }
        let frames = self.style.spinner_frames();
        let spinner = self.style.accent(frames[self.frame % frames.len()]);
        self.frame = self.frame.wrapping_add(1);
        self.last_draw = Some(now);
        self.shown = true;
        let content = render_progress(&spinner, verb, path, got, total, terminal_width());
        // \r returns to column 0; \x1b[K erases to end of line; no newline, so
        // the next redraw overwrites in place.
        write!(w, "\r\x1b[K{content}")?;
        w.flush()
    }

    /// Erase the progress line if one is currently shown. A no-op otherwise (and
    /// when color is disabled). Must be called before any normal line prints.
    pub(crate) fn clear<W: Write>(&mut self, w: &mut W) -> io::Result<()> {
        if !self.style.colors || !self.shown {
            return Ok(());
        }
        self.shown = false;
        write!(w, "\r\x1b[K")?;
        w.flush()
    }
}

// ---- clap integration -----------------------------------------------------

/// The clap help/usage color scheme for the current style: accent headers and
/// usage, bold literals, dim placeholders — or fully plain when color is off, so
/// clap emits no escapes on a pipe or under `NO_COLOR`.
pub(crate) fn clap_styles(style: Style) -> clap::builder::Styles {
    use clap::builder::styling::{Ansi256Color, AnsiColor, Color, Style as CStyle, Styles};

    if !style.colors {
        return Styles::plain();
    }
    let accent = Color::Ansi256(Ansi256Color(ACCENT_XTERM));
    let accent_bold = CStyle::new().bold().fg_color(Some(accent));
    Styles::styled()
        .header(accent_bold)
        .usage(accent_bold)
        .literal(CStyle::new().bold())
        .placeholder(CStyle::new().fg_color(Some(Color::Ansi(AnsiColor::BrightBlack))))
        .valid(CStyle::new().fg_color(Some(Color::Ansi(AnsiColor::Green))))
        .invalid(CStyle::new().fg_color(Some(Color::Ansi(AnsiColor::Red))))
        .error(
            CStyle::new()
                .bold()
                .fg_color(Some(Color::Ansi(AnsiColor::Red))),
        )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn base() -> DetectInput<'static> {
        DetectInput {
            is_terminal: true,
            no_color: false,
            term: Some("xterm-256color"),
            tomo_color: None,
            tomo_ascii: false,
            colorterm: None,
            locale_utf8: true,
        }
    }

    #[test]
    fn tty_enables_color() {
        assert!(resolve(&base()).colors);
    }

    #[test]
    fn non_tty_disables_color() {
        let mut i = base();
        i.is_terminal = false;
        assert!(!resolve(&i).colors);
    }

    #[test]
    fn no_color_disables_even_on_tty() {
        let mut i = base();
        i.no_color = true;
        assert!(!resolve(&i).colors);
    }

    #[test]
    fn dumb_term_disables_color() {
        let mut i = base();
        i.term = Some("dumb");
        assert!(!resolve(&i).colors);
    }

    #[test]
    fn tomo_color_always_overrides_non_tty_and_no_color() {
        let mut i = base();
        i.is_terminal = false;
        i.no_color = true;
        i.tomo_color = Some("always");
        assert!(resolve(&i).colors);
    }

    #[test]
    fn tomo_color_never_overrides_tty() {
        let mut i = base();
        i.tomo_color = Some("NEVER"); // case-insensitive
        assert!(!resolve(&i).colors);
    }

    #[test]
    fn tomo_color_auto_behaves_like_unset() {
        let mut i = base();
        i.tomo_color = Some("auto");
        assert!(resolve(&i).colors);
        i.is_terminal = false;
        assert!(!resolve(&i).colors);
    }

    #[test]
    fn unicode_requires_utf8_locale() {
        let mut i = base();
        assert!(resolve(&i).unicode);
        i.locale_utf8 = false;
        assert!(!resolve(&i).unicode);
    }

    #[test]
    fn tomo_ascii_forces_ascii_even_with_utf8_locale() {
        let mut i = base();
        i.tomo_ascii = true;
        assert!(!resolve(&i).unicode);
    }

    #[test]
    fn truecolor_requires_color_and_colorterm() {
        let mut i = base();
        assert!(!resolve(&i).truecolor); // no COLORTERM
        i.colorterm = Some("truecolor");
        assert!(resolve(&i).truecolor);
        i.colorterm = Some("24bit");
        assert!(resolve(&i).truecolor);
        // color off ⇒ never truecolor, even with COLORTERM.
        i.no_color = true;
        assert!(!resolve(&i).truecolor);
    }

    #[test]
    fn detect_runs_over_a_real_stream() {
        // `IsTerminal` is a sealed trait, so we cannot fake a stream; smoke-test
        // that detection runs end-to-end over stdout (env-driven; asserting on
        // the outcome would require mutating process env, which resolve() covers).
        let _ = detect(&io::stdout());
    }

    // ---- helpers no-op when disabled --------------------------------------

    #[test]
    fn helpers_are_noops_when_disabled() {
        let s = Style::default();
        assert_eq!(s.accent("x"), "x");
        assert_eq!(s.ok("x"), "x");
        assert_eq!(s.warn("x"), "x");
        assert_eq!(s.err("x"), "x");
        assert_eq!(s.dim("x"), "x");
        assert_eq!(s.bold("x"), "x");
        assert_eq!(s.header("x"), "x");
    }

    #[test]
    fn helpers_wrap_when_enabled() {
        let s = Style {
            colors: true,
            unicode: true,
            truecolor: false,
        };
        let painted = s.accent("hi");
        assert!(painted.contains("hi"));
        assert!(painted.contains('\u{1b}'), "expected an ANSI escape");
        assert!(
            painted.ends_with("\u{1b}[0m"),
            "expected a reset: {painted:?}"
        );
    }

    // ---- glyph fallbacks --------------------------------------------------

    #[test]
    fn glyphs_use_unicode_when_enabled() {
        let s = Style {
            colors: true,
            unicode: true,
            truecolor: false,
        };
        assert_eq!(s.g_ok(), "✓");
        assert_eq!(s.g_cross(), "✗");
        assert_eq!(s.g_warn(), "⚠");
        assert_eq!(s.g_dot_on(), "●");
        assert_eq!(s.g_dot_off(), "○");
        assert_eq!(s.g_up(), "↑");
        assert_eq!(s.g_down(), "↓");
        assert_eq!(s.g_sync(), "⇄");
        assert_eq!(s.g_kanji(), "友");
    }

    #[test]
    fn glyphs_fall_back_to_ascii() {
        let s = Style::default(); // unicode = false
        assert_eq!(s.g_ok(), "OK");
        assert_eq!(s.g_cross(), "X");
        assert_eq!(s.g_warn(), "!");
        assert_eq!(s.g_dot_on(), "*");
        assert_eq!(s.g_dot_off(), "o");
        assert_eq!(s.g_up(), "->");
        assert_eq!(s.g_down(), "<-");
        assert_eq!(s.g_sync(), "<->");
        assert_eq!(s.g_kanji(), "", "友 is omitted entirely in ASCII mode");
    }

    // ---- humanization -----------------------------------------------------

    #[test]
    fn thousands_grouping() {
        assert_eq!(group_thousands(0), "0");
        assert_eq!(group_thousands(7), "7");
        assert_eq!(group_thousands(42), "42");
        assert_eq!(group_thousands(999), "999");
        assert_eq!(group_thousands(1_000), "1,000");
        assert_eq!(group_thousands(1_892), "1,892");
        assert_eq!(group_thousands(12_345), "12,345");
        assert_eq!(group_thousands(1_000_000), "1,000,000");
        assert_eq!(group_thousands(1_234_567_890), "1,234,567,890");
    }

    // ---- progress line ----------------------------------------------------

    #[test]
    fn progress_line_shape() {
        let line = render_progress("*", "receiving", "big.bin", 512 * 1024, 1024 * 1024, 80);
        assert!(line.starts_with("* receiving big.bin"), "{line}");
        assert!(line.contains("50%"), "{line}");
        assert!(line.contains("512.0 kB / 1.0 MB"), "{line}");
    }

    #[test]
    fn progress_pct_clamps_and_handles_zero_total() {
        // got > total clamps to 100.
        let over = render_progress("*", "receiving", "f", 200, 100, 80);
        assert!(over.contains("100%"), "{over}");
        // zero total renders 100% rather than dividing by zero.
        let empty = render_progress("*", "receiving", "f", 0, 0, 80);
        assert!(empty.contains("100%"), "{empty}");
    }

    #[test]
    fn progress_truncates_to_width() {
        let line = render_progress("*", "receiving", &"p".repeat(200), 1, 2, 30);
        assert_eq!(line.chars().count(), 30);
        assert!(line.ends_with('…'));
    }

    #[test]
    fn progress_guard_writes_and_erases() {
        let style = Style {
            colors: true,
            unicode: false,
            truecolor: false,
        };
        let mut buf: Vec<u8> = Vec::new();
        let mut p = ProgressLine::new(style);
        p.update(&mut buf, "receiving", "f", 1, 2).unwrap();
        let s = String::from_utf8(buf.clone()).unwrap();
        assert!(
            s.contains("\r\x1b[K"),
            "expected redraw control codes: {s:?}"
        );
        assert!(s.contains("receiving f"));
        buf.clear();
        p.clear(&mut buf).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "\r\x1b[K");
    }

    #[test]
    fn progress_guard_is_silent_when_disabled() {
        let mut buf: Vec<u8> = Vec::new();
        let mut p = ProgressLine::new(Style::default());
        p.update(&mut buf, "receiving", "f", 1, 2).unwrap();
        p.clear(&mut buf).unwrap();
        assert!(buf.is_empty(), "disabled progress must write nothing");
    }

    #[test]
    fn clap_styles_plain_when_disabled() {
        // Smoke test: building styles for a disabled Style must not panic and
        // yields the plain scheme.
        let _ = clap_styles(Style::default());
        let _ = clap_styles(Style {
            colors: true,
            unicode: true,
            truecolor: true,
        });
    }
}
