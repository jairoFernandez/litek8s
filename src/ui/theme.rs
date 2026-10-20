//! The UI's look, shared by every view: a palette (violet accent, green →
//! yellow → red for load and health), rounded boxes with their titles set
//! into the border, meters and sparklines with eighth-block precision,
//! status glyphs (● running, ◐ pending, ✕ failed, ○ completed) and key
//! chips.
//!
//! It adapts to the terminal: 24-bit color when `COLORTERM` says so, else
//! the 256- or 16-color palette; none with `NO_COLOR`; a light or dark
//! background (asked once with OSC 11, see `ui::probe_background`, or set
//! in ui.yaml); plain ASCII for fonts that draw box and block characters
//! badly (`ascii: true` in ui.yaml or `LITEK8S_UI_ASCII=1`), done as a last
//! pass over the frame so the views only ever draw one set of symbols.
//! Presets (`theme:` in ui.yaml): `default`, `high-contrast` (the
//! terminal's own bright colors, no gray text) and `mono` (bold, dim and
//! reverse only).

use std::sync::OnceLock;

use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType};
use serde::Deserialize;

/// Colors the terminal can show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Depth {
    True,
    X256,
    X16,
    None,
}

impl Depth {
    /// From the environment: `NO_COLOR` wins, then `COLORTERM`
    /// (truecolor/24bit), then `TERM` (*-256color; dumb).
    pub fn detect(get: impl Fn(&str) -> Option<String>) -> Depth {
        if get("NO_COLOR").is_some_and(|v| !v.is_empty()) {
            return Depth::None;
        }
        let colorterm = get("COLORTERM").unwrap_or_default().to_lowercase();
        if colorterm == "truecolor" || colorterm == "24bit" {
            return Depth::True;
        }
        let term = get("TERM").unwrap_or_default().to_lowercase();
        if term == "dumb" {
            return Depth::None;
        }
        if term.contains("256") || term.contains("direct") || term.starts_with("tmux") || term.starts_with("alacritty") || term.starts_with("xterm-kitty") || term == "wezterm" || term == "xterm-ghostty" {
            // tmux, kitty, alacritty and friends pass 24-bit through but not
            // all set COLORTERM over ssh: 256 is the safe guess.
            return Depth::X256;
        }
        Depth::X16
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Preset {
    #[default]
    Default,
    HighContrast,
    Mono,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Background {
    /// Asked of the terminal (OSC 11); colors that read on both until
    /// it answers, or when it does not.
    #[default]
    Auto,
    Dark,
    Light,
}

/// What is known about the terminal, found once at start.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Term {
    pub depth: Depth,
    pub ascii: bool,
    /// The background color it reported (OSC 11) or `COLORFGBG` implied.
    pub bg: Option<Bg>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Bg {
    /// The exact color (OSC 11): light or dark, and zebra rows a shade off it.
    Rgb(u8, u8, u8),
    Dark,
    Light,
}

impl Bg {
    pub fn light(self) -> bool {
        match self {
            Bg::Rgb(r, g, b) => luma(r, g, b) > 0.5,
            Bg::Dark => false,
            Bg::Light => true,
        }
    }
}

fn luma(r: u8, g: u8, b: u8) -> f32 {
    (0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32) / 255.0
}

impl Term {
    /// From the environment alone (no background yet).
    pub fn from_env() -> Term {
        let get = |k: &str| std::env::var(k).ok();
        let term = get("TERM").unwrap_or_default();
        Term {
            depth: Depth::detect(get),
            // The Linux console has no eighth blocks nor rounded corners.
            ascii: get("LITEK8S_UI_ASCII").is_some_and(|v| !v.is_empty() && v != "0") || term == "linux" || term == "vt100" || term == "vt220",
            bg: colorfgbg(get("COLORFGBG").as_deref()),
        }
    }
}

/// `COLORFGBG=15;0` (rxvt, Konsole, some iTerm2 profiles): its last field is
/// the background's palette index; 7 and 9-15 are light.
pub fn colorfgbg(v: Option<&str>) -> Option<Bg> {
    let bg: u8 = v?.rsplit(';').next()?.trim().parse().ok()?;
    Some(if bg == 7 || (9..=15).contains(&bg) { Bg::Light } else { Bg::Dark })
}

static TERM: OnceLock<Term> = OnceLock::new();

/// Set once at start (after asking the terminal for its background).
pub fn init(t: Term) {
    let _ = TERM.set(t);
}

/// What `init` recorded; tests and a UI that never started see the
/// environment's defaults with 16 colors (stable output).
pub fn term() -> Term {
    *TERM.get().unwrap_or(&Term { depth: Depth::X16, ascii: false, bg: None })
}

/// `#rrggbb` as three bytes, for the palettes below.
const fn hex(v: u32) -> (u8, u8, u8) {
    ((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

/// One palette in 24-bit color; the 256-color terminal gets the nearest
/// of its colors, the 16-color one its named colors (see `Theme::build`).
struct Palette {
    accent: (u8, u8, u8),
    /// The pill behind the logo and the current tab, with `on_pill` text.
    pill: (u8, u8, u8),
    on_pill: (u8, u8, u8),
    ok: (u8, u8, u8),
    warn: (u8, u8, u8),
    bad: (u8, u8, u8),
    done: (u8, u8, u8),
    info: (u8, u8, u8),
    dim: (u8, u8, u8),
    border: (u8, u8, u8),
    /// Background of the selected row and of key chips.
    selected: (u8, u8, u8),
    /// Text on the selected row (None: the row keeps its color).
    on_selected: Option<(u8, u8, u8)>,
    chip: Option<(u8, u8, u8)>,
    track: (u8, u8, u8),
    marked: (u8, u8, u8),
}

const DARK: Palette = Palette {
    accent: hex(0xa78bfa),
    pill: hex(0x7c3aed),
    on_pill: hex(0xffffff),
    ok: hex(0x4ade80),
    warn: hex(0xfbbf24),
    bad: hex(0xf87171),
    done: hex(0x94a3b8),
    info: hex(0x60a5fa),
    dim: hex(0x8b90a5),
    border: hex(0x585d75),
    selected: hex(0x372b5c),
    on_selected: None,
    chip: Some(hex(0x2b2740)),
    track: hex(0x474b5e),
    marked: hex(0xf472b6),
};

const LIGHT: Palette = Palette {
    accent: hex(0x6d28d9),
    pill: hex(0x7c3aed),
    on_pill: hex(0xffffff),
    ok: hex(0x15803d),
    warn: hex(0xb45309),
    bad: hex(0xdc2626),
    done: hex(0x64748b),
    info: hex(0x1d4ed8),
    dim: hex(0x686d80),
    border: hex(0xb3b6c6),
    selected: hex(0xe4dcff),
    on_selected: None,
    chip: Some(hex(0xede8ff)),
    track: hex(0xc9ccd8),
    marked: hex(0xbe185d),
};

/// Background unknown: mid tones that read on black and on white, and a
/// strong selection with its own text color.
const NEUTRAL: Palette = Palette {
    accent: hex(0x8b5cf6),
    pill: hex(0x7c3aed),
    on_pill: hex(0xffffff),
    ok: hex(0x22a35a),
    warn: hex(0xd18a06),
    bad: hex(0xe5484d),
    done: hex(0x8a8fa3),
    info: hex(0x3b82f6),
    dim: hex(0x8a8fa3),
    border: hex(0x7c8196),
    selected: hex(0x6d4fd8),
    on_selected: Some(hex(0xffffff)),
    chip: None,
    track: hex(0x8a8fa3),
    marked: hex(0xdb2777),
};

/// The styles every view draws with. `Copy`: views take `&Theme`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    pub depth: Depth,
    pub preset: Preset,
    pub ascii: bool,
    pub accent: Style,
    /// The logo and the current tab: a filled pill.
    pub pill: Style,
    pub ok: Style,
    pub warn: Style,
    pub bad: Style,
    pub done: Style,
    pub info: Style,
    /// Secondary text.
    pub dim: Style,
    pub border: Style,
    /// A box's title (in its top border).
    pub title: Style,
    /// Table column headers.
    pub header: Style,
    pub selected: Style,
    /// Every other row's background, when the terminal's is known.
    pub zebra: Option<Style>,
    pub marked: Style,
    /// A key in hints and help.
    pub key: Style,
    /// A key in the footer: a small chip.
    pub chip: Style,
    /// The empty part of a meter.
    pub track: Style,
    pub bold: Style,
    /// Gradient stops for load (low, mid, high).
    stops: [(u8, u8, u8); 3],
}

impl Default for Theme {
    fn default() -> Self {
        Theme::build(Preset::Default, term(), Background::Auto)
    }
}

impl Theme {
    pub fn build(preset: Preset, term: Term, background: Background) -> Theme {
        let depth = if preset == Preset::Mono { Depth::None } else { term.depth };
        let bg = match background {
            Background::Auto => term.bg,
            Background::Dark => Some(term.bg.filter(|b| !b.light()).unwrap_or(Bg::Dark)),
            Background::Light => Some(term.bg.filter(|b| b.light()).unwrap_or(Bg::Light)),
        };
        let p = match bg {
            Some(b) if b.light() => &LIGHT,
            Some(_) => &DARK,
            None => &NEUTRAL,
        };
        let bold = Style::new().add_modifier(Modifier::BOLD);
        let c = |rgb: (u8, u8, u8)| rgb_color(depth, rgb);
        let fg = |rgb: (u8, u8, u8)| match c(rgb) {
            Some(col) => Style::new().fg(col),
            None => Style::new(),
        };
        let stops = [p.ok, p.warn, p.bad];
        match (preset, depth) {
            (Preset::Mono, _) | (_, Depth::None) => Theme {
                depth: Depth::None,
                preset,
                ascii: term.ascii,
                accent: bold,
                pill: Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD),
                ok: Style::new(),
                warn: bold,
                bad: Style::new().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                done: Style::new().add_modifier(Modifier::DIM),
                info: Style::new(),
                dim: Style::new().add_modifier(Modifier::DIM),
                border: Style::new(),
                title: bold,
                header: bold,
                selected: Style::new().add_modifier(Modifier::REVERSED),
                zebra: None,
                marked: Style::new().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                key: bold,
                chip: Style::new().add_modifier(Modifier::REVERSED),
                track: Style::new().add_modifier(Modifier::DIM),
                bold,
                stops,
            },
            // The terminal's own palette, made for its background; no gray.
            (Preset::HighContrast, _) | (_, Depth::X16) => {
                let hc = preset == Preset::HighContrast;
                let named = |n: Color, bright: Color| Style::new().fg(if hc { bright } else { n });
                let gray = if hc { Style::new() } else { Style::new().fg(Color::DarkGray) };
                Theme {
                    depth,
                    preset,
                    ascii: term.ascii,
                    accent: named(Color::Magenta, Color::LightMagenta).add_modifier(Modifier::BOLD),
                    pill: Style::new().fg(Color::Magenta).add_modifier(Modifier::REVERSED | Modifier::BOLD),
                    ok: named(Color::Green, Color::LightGreen),
                    warn: named(Color::Yellow, Color::LightYellow),
                    bad: named(Color::Red, Color::LightRed).add_modifier(if hc { Modifier::BOLD } else { Modifier::empty() }),
                    done: gray,
                    info: named(Color::Blue, Color::LightBlue),
                    dim: gray,
                    border: if hc { Style::new() } else { gray },
                    title: bold,
                    header: bold,
                    selected: Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD),
                    zebra: None,
                    marked: named(Color::Cyan, Color::LightCyan).add_modifier(Modifier::BOLD),
                    key: named(Color::Magenta, Color::LightMagenta).add_modifier(Modifier::BOLD),
                    chip: named(Color::Magenta, Color::LightMagenta).add_modifier(Modifier::BOLD | Modifier::REVERSED),
                    track: gray,
                    bold,
                    stops,
                }
            }
            (Preset::Default, _) => {
                let on = |bgc: (u8, u8, u8)| match c(bgc) {
                    Some(col) => Style::new().bg(col),
                    None => Style::new(),
                };
                let mut selected = on(p.selected).add_modifier(Modifier::BOLD);
                if let Some(f) = p.on_selected.and_then(c) {
                    selected = selected.fg(f);
                }
                let chip = match p.chip {
                    Some(bgc) => fg(p.accent).patch(on(bgc)).add_modifier(Modifier::BOLD),
                    None => fg(p.accent).add_modifier(Modifier::BOLD),
                };
                // A shade off the terminal's own background (5 % toward
                // the text), only when it told us which.
                let zebra = match bg {
                    Some(Bg::Rgb(r, g, b)) => {
                        let toward = if luma(r, g, b) > 0.5 { 0.0 } else { 255.0 };
                        let mix = |v: u8| (v as f32 + (toward - v as f32) * 0.05).round() as u8;
                        c((mix(r), mix(g), mix(b))).filter(|_| depth == Depth::True).map(|col| Style::new().bg(col))
                    }
                    _ => None,
                };
                Theme {
                    depth,
                    preset,
                    ascii: term.ascii,
                    accent: fg(p.accent).add_modifier(Modifier::BOLD),
                    pill: fg(p.on_pill).patch(on(p.pill)).add_modifier(Modifier::BOLD),
                    ok: fg(p.ok),
                    warn: fg(p.warn),
                    bad: fg(p.bad),
                    done: fg(p.done),
                    info: fg(p.info),
                    dim: fg(p.dim),
                    border: fg(p.border),
                    title: bold,
                    header: fg(p.dim).add_modifier(Modifier::BOLD),
                    selected,
                    zebra,
                    marked: fg(p.marked).add_modifier(Modifier::BOLD),
                    key: fg(p.accent).add_modifier(Modifier::BOLD),
                    chip,
                    track: fg(p.track),
                    bold,
                    stops,
                }
            }
        }
    }

    /// The theme ui.yaml asks for, in this terminal.
    pub fn from_config(c: &super::kube::uiconfig::UiConfig) -> Theme {
        let mut term = term();
        term.ascii |= c.ascii;
        Theme::build(c.theme, term, c.background).with_skin(&c.skin)
    }

    /// ui.yaml's `skin:` colors over the preset's (not with NO_COLOR or
    /// `mono`).
    pub fn with_skin(mut self, skin: &super::kube::uiconfig::Skin) -> Theme {
        if self.depth == Depth::None {
            return self;
        }
        let set = |s: &mut Style, v: &str| {
            if let Some(c) = super::kube::uiconfig::color(v) {
                *s = s.fg(c);
            }
        };
        set(&mut self.accent, &skin.accent);
        set(&mut self.ok, &skin.ok);
        set(&mut self.warn, &skin.warn);
        set(&mut self.bad, &skin.bad);
        set(&mut self.done, &skin.done);
        set(&mut self.dim, &skin.dim);
        set(&mut self.marked, &skin.marked);
        set(&mut self.key, &skin.key);
        if let Some(c) = super::kube::uiconfig::color(&skin.selected) {
            self.selected = Style::new().bg(c).add_modifier(Modifier::BOLD);
        }
        self
    }

    /// The style of a health or status.
    pub fn health(&self, h: Health) -> Style {
        match h {
            Health::Ok => self.ok,
            Health::Warn => self.warn,
            Health::Bad => self.bad,
            Health::Done => self.done,
        }
    }

    /// A colored glyph and word: `● Running`.
    pub fn status(&self, h: Health, word: &str) -> Span<'static> {
        Span::styled(format!("{} {word}", h.glyph()), self.health(h))
    }

    /// A rounded box with its title set into the top border: `╭─ title ──╮`.
    pub fn block<'a>(&self, title: impl Into<Line<'a>>) -> Block<'a> {
        self.framed(title.into(), self.border)
    }

    /// The same with a second title at the right: `╭─ cpu ── 6% of 4 ─╮`.
    pub fn block_with<'a>(&self, title: impl Into<Line<'a>>, right: Vec<Span<'a>>) -> Block<'a> {
        let b = self.framed(title.into(), self.border);
        if right.is_empty() {
            return b;
        }
        let mut spans = vec![Span::raw(" ")];
        spans.extend(right.into_iter().map(own_fg));
        spans.push(Span::raw(" "));
        spans.push(Span::styled("─", self.border));
        b.title_top(Line::from(spans).right_aligned())
    }

    /// A box that stands out (dialogs): its border in `style`'s color.
    pub fn dialog<'a>(&self, title: impl Into<Line<'a>>, style: Style) -> Block<'a> {
        let border = if self.depth == Depth::None { Style::new().add_modifier(Modifier::BOLD) } else { Style::new().fg(style.fg.unwrap_or(Color::Reset)) };
        self.framed(title.into(), border)
    }

    fn framed<'a>(&self, title: Line<'a>, border: Style) -> Block<'a> {
        let b = Block::bordered().border_type(BorderType::Rounded).border_style(border);
        if title.width() == 0 {
            return b;
        }
        let mut spans = vec![Span::styled("─", border), Span::raw(" ")];
        let style = title.style;
        spans.extend(title.spans.into_iter().map(|s| own_fg(if s.style == Style::default() && style == Style::default() { s.style(self.title) } else { s.patch_style(style) })));
        spans.push(Span::raw(" "));
        b.title_top(Line::from(spans))
    }

    /// The color for a load of `ratio` (0..1): green, yellow, red.
    pub fn level(&self, ratio: f64) -> Style {
        let r = ratio.clamp(0.0, 1.0);
        match self.depth {
            Depth::None => Style::new(),
            Depth::X16 => {
                if r < 0.6 {
                    self.ok
                } else if r < 0.85 {
                    self.warn
                } else {
                    self.bad
                }
            }
            _ if self.preset == Preset::HighContrast => {
                if r < 0.6 {
                    self.ok
                } else if r < 0.85 {
                    self.warn
                } else {
                    self.bad
                }
            }
            d => {
                let [a, b, c] = self.stops;
                let (from, to, t) = if r < 0.5 { (a, b, r / 0.5) } else { (b, c, (r - 0.5) / 0.5) };
                let mix = |x: u8, y: u8| (x as f64 + (y as f64 - x as f64) * t).round() as u8;
                rgb_color(d, (mix(from.0, to.0), mix(from.1, to.1), mix(from.2, to.2))).map(|col| Style::new().fg(col)).unwrap_or_default()
            }
        }
    }

    /// A meter `width` cells wide: the filled part in eighths of a cell,
    /// each cell colored by where it sits (green at the left, red at the
    /// right), the rest a dotted track.
    pub fn meter(&self, ratio: f64, width: usize) -> Vec<Span<'static>> {
        let r = if ratio.is_finite() { ratio.clamp(0.0, 1.0) } else { 0.0 };
        let eighths = (r * width as f64 * 8.0).round() as usize;
        let full = eighths / 8;
        let part = eighths % 8;
        let mut out: Vec<Span<'static>> = Vec::new();
        for i in 0..full.min(width) {
            out.push(Span::styled("█", self.level((i as f64 + 0.5) / width.max(1) as f64)));
        }
        let mut used = full.min(width);
        if part > 0 && used < width {
            out.push(Span::styled(EIGHTHS[part - 1], self.level((used as f64 + 0.5) / width.max(1) as f64)));
            used += 1;
        }
        if used < width {
            out.push(Span::styled("░".repeat(width - used), self.track));
        }
        merge(out)
    }

    /// A meter of stacked parts `width` cells wide, in eighths of a cell:
    /// each cell takes the part that covers most of it, the cell where the
    /// parts end shows the eighths they reach (a sliver at least when any
    /// part is there), the rest a dotted track.
    pub fn stacked(&self, parts: &[(f64, Style, &'static str)], width: usize) -> Vec<Span<'static>> {
        let total = width * 8;
        let mut ends = Vec::with_capacity(parts.len());
        let mut cum = 0.0f64;
        let mut prev = 0;
        for (share, _, glyph) in parts {
            let share = if share.is_finite() { share.max(0.0) } else { 0.0 };
            cum += share;
            // A solid part that is there gets an eighth at least.
            let floor = prev + usize::from(share > 0.0 && *glyph == "█");
            prev = ((cum * total as f64).round() as usize).max(floor).min(total);
            ends.push(prev);
        }
        let mut end = prev;
        if end == 0 && cum > 0.0 && width > 0 {
            end = 1;
        }
        let mut out: Vec<Span<'static>> = Vec::new();
        for c in 0..width {
            let (lo, hi) = (c * 8, c * 8 + 8);
            if end <= lo {
                out.push(Span::styled("░".repeat(width - c), self.track));
                break;
            }
            // The part covering most of this cell.
            let mut best = (0usize, 0usize);
            let mut start = 0;
            for (k, e) in ends.iter().enumerate() {
                let cover = (*e).min(hi).saturating_sub(start.max(lo));
                if cover > best.1 {
                    best = (k, cover);
                }
                start = *e;
            }
            let k = if best.1 == 0 { parts.iter().position(|p| p.0 > 0.0).unwrap_or(0) } else { best.0 };
            let (_, style, glyph) = parts[k];
            let sym = if end < hi && glyph == "█" { EIGHTHS[end - lo - 1] } else { glyph };
            out.push(Span::styled(sym, style));
        }
        merge(out)
    }

    /// An inline bar for a share (`pods ████▏ 4.8%`): eighths, one color,
    /// at least a sliver when the share is not zero.
    pub fn bar(&self, ratio: f64, width: usize, style: Style) -> Span<'static> {
        let r = if ratio.is_finite() { ratio.clamp(0.0, 1.0) } else { 0.0 };
        let mut eighths = (r * width as f64 * 8.0).round() as usize;
        if r > 0.0 && eighths == 0 {
            eighths = 1;
        }
        let mut s = "█".repeat(eighths / 8);
        if eighths % 8 > 0 {
            s.push_str(EIGHTHS[eighths % 8 - 1]);
        }
        let n = s.chars().count();
        if n < width {
            s.push_str(&" ".repeat(width - n));
        }
        Span::styled(s, style)
    }

    /// The last `width` values as ▁▂▃▄▅▆▇█ against `max`, each colored by
    /// its height; right-aligned (older samples scroll off the left).
    pub fn sparkline(&self, values: &[f64], max: f64, width: usize) -> Vec<Span<'static>> {
        let shown = &values[values.len().saturating_sub(width)..];
        let mut out = Vec::new();
        if shown.len() < width {
            out.push(Span::raw(" ".repeat(width - shown.len())));
        }
        for v in shown {
            let r = if max > 0.0 { (v / max).clamp(0.0, 1.0) } else { 0.0 };
            let i = (r * 7.0).round() as usize;
            out.push(Span::styled(SPARKS[i], self.level(r)));
        }
        merge(out)
    }

    /// A bar graph `height` rows tall of the last `width` values against
    /// `max` (eighths at the top of each column), colored by height like a
    /// meter: btop's history graphs, in block characters.
    pub fn graph(&self, values: &[f64], max: f64, width: usize, height: usize) -> Vec<Line<'static>> {
        let shown = &values[values.len().saturating_sub(width)..];
        let pad = width - shown.len();
        (0..height)
            .map(|row| {
                let b = height - 1 - row;
                let style = self.level((b as f64 + 0.5) / height.max(1) as f64);
                let mut s = " ".repeat(pad);
                for v in shown {
                    let r = if max > 0.0 { (v / max).clamp(0.0, 1.0) } else { 0.0 };
                    let mut level = (r * (height * 8) as f64).round() as usize;
                    if b == 0 && level == 0 {
                        level = 1;
                    }
                    let fill = level.saturating_sub(b * 8).min(8);
                    s.push_str(match fill {
                        0 => " ",
                        8 => "█",
                        n => SPARKS[n - 1],
                    });
                }
                Line::from(Span::styled(s, style))
            })
            .collect()
    }

    /// The footer's `key label` pairs as chips.
    pub fn hints(&self, keys: &[(&str, &str)]) -> Vec<Span<'static>> {
        let mut out = vec![Span::raw(" ")];
        for (k, v) in keys {
            out.push(Span::styled(format!(" {k} "), self.chip));
            out.push(Span::styled(format!(" {v}  "), self.dim));
        }
        out
    }
}

/// A title's text in its own color (or the terminal's), not the border's
/// it is drawn over.
fn own_fg(s: Span<'_>) -> Span<'_> {
    if s.style.fg.is_none() && s.style.add_modifier.intersection(Modifier::REVERSED).is_empty() {
        let st = s.style.fg(Color::Reset);
        s.style(st)
    } else {
        s
    }
}

/// Neighboring spans of the same style as one (fewer cells to diff).
fn merge(spans: Vec<Span<'static>>) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::with_capacity(spans.len());
    for s in spans {
        match out.last_mut() {
            Some(last) if last.style == s.style => last.content.to_mut().push_str(&s.content),
            _ => out.push(s),
        }
    }
    out
}

const EIGHTHS: [&str; 7] = ["▏", "▎", "▍", "▌", "▋", "▊", "▉"];
const SPARKS: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];

/// How a pod, container or object is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Health {
    #[default]
    Ok,
    /// On its way (pending, rolling out, not all ready).
    Warn,
    Bad,
    /// Done or idle (Completed, Succeeded, exited 0): fine, but not lively.
    Done,
}

impl Health {
    pub fn glyph(self) -> &'static str {
        match self {
            Health::Ok => "●",
            Health::Warn => "◐",
            Health::Bad => "✕",
            Health::Done => "○",
        }
    }

    /// A pod's STATUS as kubectl prints it.
    pub fn of_pod(status: &str, ready: &str) -> Health {
        match status {
            "Completed" | "Succeeded" => Health::Done,
            "Running" => match ready.split_once('/').and_then(|(a, b)| Some((a.trim().parse::<u32>().ok()?, b.trim().parse::<u32>().ok()?))) {
                Some((a, b)) if a < b => Health::Warn,
                _ => Health::Ok,
            },
            "Pending" | "ContainerCreating" | "PodInitializing" | "Terminating" | "" => Health::Warn,
            s if s.starts_with("Init:") && !s.contains("Err") && !s.contains("Crash") => Health::Warn,
            _ => Health::Bad,
        }
    }

    /// A Docker container's state (`running`, `exited`, …) and status
    /// (`Exited (1) 2 minutes ago`).
    pub fn of_container(state: &str, status: &str) -> Health {
        match state {
            "running" => {
                if status.contains("unhealthy") {
                    Health::Bad
                } else if status.contains("health: starting") {
                    Health::Warn
                } else {
                    Health::Ok
                }
            }
            "restarting" | "created" | "paused" | "removing" => Health::Warn,
            "dead" => Health::Bad,
            "exited" if status.contains("Exited (0)") || status.is_empty() => Health::Done,
            "exited" => Health::Bad,
            _ => Health::Done,
        }
    }
}

/// `rgb` in the terminal's colors: itself, the nearest of the 256, or
/// none (16 colors and none use named colors, chosen by the caller).
fn rgb_color(depth: Depth, (r, g, b): (u8, u8, u8)) -> Option<Color> {
    match depth {
        Depth::True => Some(Color::Rgb(r, g, b)),
        Depth::X256 => Some(Color::Indexed(nearest_256(r, g, b))),
        Depth::X16 => Some(nearest_16(r, g, b)),
        Depth::None => None,
    }
}

/// The nearest xterm-256 color: the 6x6x6 cube or the gray ramp.
pub fn nearest_256(r: u8, g: u8, b: u8) -> u8 {
    const STEPS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let idx = |v: u8| STEPS.iter().enumerate().min_by_key(|(_, s)| (**s as i32 - v as i32).abs()).map(|(i, _)| i).unwrap_or(0);
    let (ri, gi, bi) = (idx(r), idx(g), idx(b));
    let cube = (STEPS[ri], STEPS[gi], STEPS[bi]);
    let avg = (r as u32 + g as u32 + b as u32) / 3;
    let gi_ = if avg < 8 { 0 } else { ((avg - 8) / 10).min(23) } as u8;
    let gray = 8 + gi_ * 10;
    let d = |(x, y, z): (u8, u8, u8)| (x as i32 - r as i32).pow(2) + (y as i32 - g as i32).pow(2) + (z as i32 - b as i32).pow(2);
    if d((gray, gray, gray)) < d(cube) { 232 + gi_ } else { 16 + 36 * ri as u8 + 6 * gi as u8 + bi as u8 }
}

/// A color of the gradient on a 16-color terminal (only used by `level`'s
/// fallback paths through `rgb_color`).
fn nearest_16(r: u8, g: u8, b: u8) -> Color {
    let (r, g, b) = (r as i32, g as i32, b as i32);
    if r > 180 && g > 140 && b < 120 {
        Color::Yellow
    } else if r > g && r > b {
        Color::Red
    } else if g >= r && g > b {
        Color::Green
    } else if b > r && b > g && r > 120 {
        Color::Magenta
    } else if b > g {
        Color::Blue
    } else {
        Color::Gray
    }
}

/// The frame's symbols in ASCII (`ascii: true`), for fonts without box and
/// block characters. Done on the finished buffer: one set of symbols in
/// the views, nothing to keep in sync.
pub fn asciify(buf: &mut Buffer) {
    for cell in buf.content.iter_mut() {
        let s = cell.symbol();
        if s.is_ascii() {
            continue;
        }
        if let Some(a) = ascii_of(s) {
            cell.set_symbol(a);
        }
    }
}

fn ascii_of(s: &str) -> Option<&'static str> {
    Some(match s {
        "╭" | "╮" | "╰" | "╯" | "┌" | "┐" | "└" | "┘" | "├" | "┤" | "┬" | "┴" | "┼" => "+",
        "─" | "━" | "═" | "╌" => "-",
        "│" | "┃" | "║" | "╎" => "|",
        "█" | "▉" | "▊" | "▋" | "▌" => "#",
        "▍" | "▎" | "▏" => "|",
        "░" => ".",
        "▒" | "▓" => ":",
        "▁" | "▂" => "_",
        "▃" | "▄" => "-",
        "▅" | "▆" => "=",
        "▇" => "#",
        "●" => "*",
        "◐" => "~",
        "✕" | "✗" => "x",
        "○" => "o",
        "✓" => "v",
        "◆" => "+",
        "…" => "~",
        "·" | "•" => ".",
        "▸" | "›" | "→" | "⟶" => ">",
        "▾" | "↓" => "v",
        "‹" | "←" => "<",
        "↑" => "^",
        "⇄" => "=",
        "⎈" => "@",
        "⚠" => "!",
        "⟳" => "@",
        "×" => "x",
        _ => return None,
    })
}

/// The rounded border set (ratatui's), for code that draws borders by hand.
pub const ROUNDED: border::Set = border::ROUNDED;

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
    }

    #[test]
    fn color_depth_from_the_environment() {
        assert_eq!(Depth::detect(env(&[("COLORTERM", "truecolor"), ("TERM", "xterm-256color")])), Depth::True);
        assert_eq!(Depth::detect(env(&[("TERM", "xterm-256color")])), Depth::X256);
        assert_eq!(Depth::detect(env(&[("TERM", "xterm")])), Depth::X16);
        assert_eq!(Depth::detect(env(&[("TERM", "dumb")])), Depth::None);
        assert_eq!(Depth::detect(env(&[("NO_COLOR", "1"), ("COLORTERM", "truecolor")])), Depth::None);
        // An empty NO_COLOR does not count (no-color.org).
        assert_eq!(Depth::detect(env(&[("NO_COLOR", ""), ("COLORTERM", "24bit")])), Depth::True);
    }

    #[test]
    fn colorfgbg_says_light_or_dark() {
        assert_eq!(colorfgbg(Some("15;0")), Some(Bg::Dark));
        assert_eq!(colorfgbg(Some("0;15")), Some(Bg::Light));
        assert_eq!(colorfgbg(Some("12;default;7")), Some(Bg::Light));
        assert_eq!(colorfgbg(None), None);
        assert_eq!(colorfgbg(Some("garbage")), None);
    }

    #[test]
    fn nearest_256_colors() {
        assert_eq!(nearest_256(0, 0, 0), 16);
        assert_eq!(nearest_256(255, 255, 255), 231);
        assert_eq!(nearest_256(0x80, 0x80, 0x80), 244);
        // The accent lands on a violet.
        assert_eq!(nearest_256(0xa7, 0x8b, 0xfa), 141);
    }

    #[test]
    fn meter_in_eighths() {
        let t = Theme::build(Preset::Mono, Term { depth: Depth::None, ascii: false, bg: None }, Background::Auto);
        let text = |sp: Vec<Span>| sp.iter().map(|s| s.content.to_string()).collect::<String>();
        assert_eq!(text(t.meter(0.0, 4)), "░░░░");
        assert_eq!(text(t.meter(0.5, 4)), "██░░");
        assert_eq!(text(t.meter(0.53, 4)), "██▏░");
        assert_eq!(text(t.meter(1.0, 4)), "████");
        assert_eq!(text(t.meter(2.0, 4)), "████");
        assert_eq!(text(t.meter(f64::NAN, 3)), "░░░");
        // Stacked: parts by share in eighths, the rest track; a tiny total
        // is a sliver, not a cell per part.
        let parts = [(0.25, Style::new(), "█"), (0.25, Style::new().bold(), "█"), (0.2, Style::new(), "▒")];
        assert_eq!(text(t.stacked(&parts, 8)), "████▒▒░░");
        assert_eq!(t.stacked(&parts, 8)[1].content, "██");
        assert_eq!(text(t.stacked(&[(0.3, Style::new(), "█")], 4)), "█▎░░");
        assert_eq!(text(t.stacked(&[(0.001, Style::new(), "█"), (0.001, Style::new(), "▒")], 4)), "▏░░░");
        // In use beats cache in a sliver: three tiny solid parts, more cache.
        let tiny = [(0.0005, Style::new(), "█"), (0.0003, Style::new(), "█"), (0.0017, Style::new(), "█"), (0.0016, Style::new(), "▒")];
        assert_eq!(text(t.stacked(&tiny, 29)), format!("▍{}", "░".repeat(28)));
        assert_eq!(text(t.stacked(&[(2.0, Style::new(), "█")], 4)), "████");
        // A tiny share still shows.
        assert_eq!(t.bar(0.0001, 5, Style::new()).content, "▏    ");
        assert_eq!(t.bar(0.0, 3, Style::new()).content, "   ");
        assert_eq!(text(t.sparkline(&[0.0, 0.5, 1.0], 1.0, 5)), "  ▁▅█");
        assert_eq!(text(t.sparkline(&[1.0; 9], 1.0, 4)), "████");
        let g: Vec<String> = t.graph(&[0.0, 0.25, 0.5, 1.0], 1.0, 5, 2).iter().map(|l| l.to_string()).collect();
        assert_eq!(g, vec!["    █", " ▁▄██"]);
    }

    #[test]
    fn gradient_goes_green_to_red() {
        let t = Theme::build(Preset::Default, Term { depth: Depth::True, ascii: false, bg: Some(Bg::Dark) }, Background::Auto);
        assert_eq!(t.level(0.0).fg, Some(Color::Rgb(0x4a, 0xde, 0x80)));
        assert_eq!(t.level(1.0).fg, Some(Color::Rgb(0xf8, 0x71, 0x71)));
        let t16 = Theme::build(Preset::Default, Term { depth: Depth::X16, ascii: false, bg: None }, Background::Auto);
        assert_eq!(t16.level(0.1).fg, Some(Color::Green));
        assert_eq!(t16.level(0.7).fg, Some(Color::Yellow));
        assert_eq!(t16.level(0.95).fg, Some(Color::Red));
    }

    #[test]
    fn backgrounds_pick_palettes_and_zebra() {
        let dark = Theme::build(Preset::Default, Term { depth: Depth::True, ascii: false, bg: Some(Bg::Rgb(0x1e, 0x1e, 0x2e)) }, Background::Auto);
        assert_eq!(dark.accent.fg, Some(Color::Rgb(0xa7, 0x8b, 0xfa)));
        assert!(dark.zebra.is_some());
        let light = Theme::build(Preset::Default, Term { depth: Depth::True, ascii: false, bg: Some(Bg::Rgb(0xff, 0xff, 0xff)) }, Background::Auto);
        assert_eq!(light.accent.fg, Some(Color::Rgb(0x6d, 0x28, 0xd9)));
        assert_eq!(light.zebra, Some(Style::new().bg(Color::Rgb(0xf2, 0xf2, 0xf2))));
        // Unknown: colors for both, a selection with its own text color.
        let unknown = Theme::build(Preset::Default, Term { depth: Depth::True, ascii: false, bg: None }, Background::Auto);
        assert!(unknown.zebra.is_none());
        assert_eq!(unknown.selected.fg, Some(Color::Rgb(0xff, 0xff, 0xff)));
        // ui.yaml's `background: light` wins over a guess.
        let forced = Theme::build(Preset::Default, Term { depth: Depth::True, ascii: false, bg: Some(Bg::Dark) }, Background::Light);
        assert_eq!(forced.accent.fg, light.accent.fg);
    }

    #[test]
    fn mono_and_no_color_have_no_colors() {
        for t in [
            Theme::build(Preset::Mono, Term { depth: Depth::True, ascii: false, bg: None }, Background::Auto),
            Theme::build(Preset::Default, Term { depth: Depth::None, ascii: false, bg: None }, Background::Auto),
        ] {
            for s in [t.accent, t.pill, t.ok, t.warn, t.bad, t.done, t.dim, t.border, t.selected, t.key, t.chip, t.track, t.level(0.9)] {
                assert_eq!((s.fg, s.bg), (None, None), "{s:?}");
            }
            assert!(t.selected.add_modifier.contains(Modifier::REVERSED));
        }
    }

    #[test]
    fn statuses() {
        assert_eq!(Health::of_pod("Running", "1/1"), Health::Ok);
        assert_eq!(Health::of_pod("Running", "0/1"), Health::Warn);
        assert_eq!(Health::of_pod("CrashLoopBackOff", "0/1"), Health::Bad);
        assert_eq!(Health::of_pod("Completed", "0/1"), Health::Done);
        assert_eq!(Health::of_pod("Init:0/1", "0/1"), Health::Warn);
        assert_eq!(Health::of_container("exited", "Exited (0) 1 hour ago"), Health::Done);
        assert_eq!(Health::of_container("exited", "Exited (137) 1 hour ago"), Health::Bad);
        assert_eq!(Health::of_container("running", "Up 2 minutes (unhealthy)"), Health::Bad);
    }

    #[test]
    fn ascii_pass_maps_symbols() {
        use ratatui::layout::Rect;
        let mut b = Buffer::empty(Rect::new(0, 0, 8, 1));
        b.set_string(0, 0, "╭─●█▌░…é", Style::new());
        asciify(&mut b);
        let s: String = b.content.iter().map(|c| c.symbol()).collect();
        assert_eq!(s, "+-*##.~é");
    }
}
