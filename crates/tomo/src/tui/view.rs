//! The pure render layer (UX-V2 §3a/§3b): `Model` → `ratatui` widgets. No state
//! is mutated and no I/O happens here; a [`Theme`] (built once by the shell from
//! `crate::style`) supplies the color/glyph decisions so `NO_COLOR`,
//! `TOMO_COLOR`, and `TOMO_ASCII` are honored exactly as the rest of the CLI.
//!
//! Rendering is deliberately dumb: given the same `Model` it draws the same
//! frame, which is what the `TestBackend` smoke tests rely on.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style as RStyle};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use super::state::{
    cli_echo, compare_header, format_ago, last_sync_text, restore_echo, Modal, Model, PathRow,
    Screen, Side, VersionRow, VisRow,
};

/// Color + glyph capability for the TUI, mirrored from `crate::style`'s startup
/// detection so the two surfaces agree (invariant: same env → same fallbacks).
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    /// Whether ANSI colors are emitted.
    pub color: bool,
    /// Whether Unicode glyphs are used (else ASCII fallbacks).
    pub unicode: bool,
}

impl Theme {
    /// Build a theme from the process-wide [`crate::style`] decision. We reuse
    /// its glyph choice (not a private field) so color/ASCII policy stays in one
    /// place: `●` renders only when Unicode is enabled.
    #[must_use]
    pub fn from_style() -> Self {
        let s = crate::style::current();
        Theme {
            color: s.enabled(),
            unicode: s.g_dot_on() == "●",
        }
    }

    fn fg(self, color: Color) -> RStyle {
        if self.color {
            RStyle::default().fg(color)
        } else {
            RStyle::default()
        }
    }

    fn dim(self) -> RStyle {
        if self.color {
            RStyle::default().add_modifier(Modifier::DIM)
        } else {
            RStyle::default()
        }
    }

    fn accent(self) -> RStyle {
        self.fg(Color::Rgb(0xff, 0x8a, 0x5c))
    }

    fn side_style(self, side: Side) -> RStyle {
        match side {
            Side::You => self.fg(Color::Cyan),
            Side::Peer => self.fg(Color::Magenta),
        }
    }

    fn border_set(self) -> border::Set<'static> {
        if self.unicode {
            border::PLAIN
        } else {
            border::Set {
                top_left: "+",
                top_right: "+",
                bottom_left: "+",
                bottom_right: "+",
                vertical_left: "|",
                vertical_right: "|",
                horizontal_top: "-",
                horizontal_bottom: "-",
            }
        }
    }

    fn g(self, uni: &'static str, ascii: &'static str) -> &'static str {
        if self.unicode {
            uni
        } else {
            ascii
        }
    }
}

/// Draw the whole UI for the current model.
pub fn render(f: &mut Frame, model: &Model, theme: Theme) {
    let area = f.area();
    if Model::too_small(area.width, area.height) {
        render_too_small(f, area, theme);
        return;
    }
    match model.screen {
        Screen::Main => render_main(f, area, model, theme),
        Screen::Conflicts => render_conflicts(f, area, model, theme),
        Screen::HistoryPicker => render_picker(f, area, model, theme),
        Screen::HistoryTimeline => render_timeline(f, area, model, theme),
    }
    if model.help {
        render_help(f, area, model, theme);
    }
    if let Some(Modal::AckAll { count }) = &model.modal {
        render_ack_modal(f, area, *count, theme);
    }
    if model.modal == Some(Modal::StopConfirm) {
        render_stop_modal(f, area, theme);
    }
    if let Some(Modal::RestoreConfirm {
        path,
        version,
        size,
        wall_ms,
        deleted,
    }) = &model.modal
    {
        render_restore_modal(
            f, area, path, *version, *size, *wall_ms, *deleted, model, theme,
        );
    }
}

fn render_too_small(f: &mut Frame, area: Rect, _theme: Theme) {
    let p = Paragraph::new("terminal too small (need 40x8)").wrap(Wrap { trim: true });
    f.render_widget(p, area);
}

// ---- main screen (§3a) ----------------------------------------------------

fn render_main(f: &mut Frame, area: Rect, model: &Model, theme: Theme) {
    let xfer_h = u16::try_from(model.transfers.len()).unwrap_or(0);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(xfer_h),
            Constraint::Length(1),
        ])
        .split(area);
    render_stream(f, chunks[0], model, theme);
    render_transfers(f, chunks[1], model, theme);
    render_status(f, chunks[2], model, theme);
}

fn render_stream(f: &mut Frame, area: Rect, model: &Model, theme: Theme) {
    let filtered = model.filtered_events();
    let height = area.height as usize;
    // A nudge line at the bottom eats one row when browsing scrollback.
    let nudge = model.new_activity && !model.follow;
    let body_rows = if nudge {
        height.saturating_sub(1)
    } else {
        height
    };

    let bottom = filtered.len().saturating_sub(model.scroll);
    let start = bottom.saturating_sub(body_rows);
    let mut lines: Vec<Line> = filtered[start..bottom]
        .iter()
        .map(|l| stream_line(&l.event, model.peer_name.as_deref(), theme))
        .collect();
    if nudge {
        lines.push(Line::from(Span::styled(
            format!("{} new activity", theme.g("▾", "v")),
            theme.accent(),
        )));
    }
    f.render_widget(Paragraph::new(lines), area);
}

/// Render one stream event, mirroring the plain CLI wording/glyphs from
/// `crate::events_cmd` (synced `↓`, sent `↑`, removed `✗`, conflict `⚠`).
fn stream_line<'a>(
    event: &'a crate::ctl::proto::Event,
    peer: Option<&str>,
    theme: Theme,
) -> Line<'a> {
    use crate::ctl::proto::{ConflictSide, Event};
    let sz = |b: u64| crate::history_cmd::human_size(b);
    match event {
        Event::Synced { path, size } => Line::from(vec![
            Span::styled(format!("  {} ", theme.g("↓", "<-")), theme.accent()),
            Span::raw(path.clone()),
            Span::styled(format!("  {}", sz(*size)), theme.dim()),
        ]),
        Event::Sent { path, size } => Line::from(vec![
            Span::styled(format!("  {} ", theme.g("↑", "->")), theme.accent()),
            Span::raw(path.clone()),
            Span::styled(format!("  {}", sz(*size)), theme.dim()),
        ]),
        Event::Removed { path } => Line::from(vec![
            Span::styled(format!("  {} ", theme.g("✗", "X")), theme.fg(Color::Red)),
            Span::raw(format!("{path} removed")),
        ]),
        Event::Conflict { winner, path, .. } => {
            let who = match winner {
                ConflictSide::Local => "your".to_owned(),
                ConflictSide::Peer => format!("{}'s", peer.unwrap_or("peer")),
            };
            Line::from(vec![
                Span::styled(format!("  {} ", theme.g("⚠", "!")), theme.fg(Color::Yellow)),
                Span::raw(format!("conflict {path} — kept {who} copy")),
                Span::styled("  · c to review", theme.dim()),
            ])
        }
        Event::Connected {
            peer_name,
            peer_addr,
        } => {
            let who = match (peer_name.as_deref(), peer_addr.as_deref()) {
                (Some(n), Some(a)) => format!(" {n} ({a})"),
                (Some(n), None) => format!(" {n}"),
                (None, Some(a)) => format!(" {a}"),
                (None, None) => String::new(),
            };
            Line::from(vec![
                Span::styled(format!("  {} ", theme.g("●", "*")), theme.fg(Color::Green)),
                Span::raw(format!("connected{who}")),
            ])
        }
        Event::Disconnected => Line::from(vec![
            Span::styled(format!("  {} ", theme.g("○", "o")), theme.dim()),
            Span::raw("disconnected"),
        ]),
        Event::Note { message } => Line::from(Span::styled(format!("  {message}"), theme.dim())),
        Event::Error { message } => Line::from(vec![
            Span::styled(format!("  {} ", theme.g("✗", "X")), theme.fg(Color::Red)),
            Span::styled(format!("error: {message}"), theme.fg(Color::Red)),
        ]),
        Event::Lagged => Line::from(Span::styled(
            "  event stream lagged — some events were dropped",
            theme.fg(Color::Yellow),
        )),
        // Not log lines; never reach the stream, but keep the match exhaustive.
        Event::Transfer { .. } | Event::Heartbeat { .. } => Line::default(),
    }
}

fn render_transfers(f: &mut Frame, area: Rect, model: &Model, theme: Theme) {
    if area.height == 0 {
        return;
    }
    let width = area.width as usize;
    let lines: Vec<Line> = model
        .transfers
        .iter()
        .map(|t| {
            let pct = t
                .done
                .saturating_mul(100)
                .checked_div(t.total)
                .map_or(100, |p| p.min(100));
            let bar = progress_bar(pct, width.saturating_sub(30).clamp(8, 24), theme);
            Line::from(vec![
                Span::styled(format!("  {} ", theme.g("⇡", ">>")), theme.accent()),
                Span::raw(format!("{}  ", t.path)),
                Span::styled(bar, theme.accent()),
                Span::raw(format!(" {pct}%")),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines), area);
}

fn progress_bar(pct: u64, width: usize, theme: Theme) -> String {
    let pct = usize::try_from(pct).unwrap_or(0).min(100);
    let filled = (pct * width / 100).min(width);
    let (f_ch, e_ch) = if theme.unicode {
        ("█", "░")
    } else {
        ("#", "-")
    };
    let mut s = String::with_capacity(width * 3);
    for _ in 0..filled {
        s.push_str(f_ch);
    }
    for _ in 0..width.saturating_sub(filled) {
        s.push_str(e_ch);
    }
    s
}

fn render_status(f: &mut Frame, area: Rect, model: &Model, theme: Theme) {
    let mut spans: Vec<Span> = Vec::new();
    // Peer + connection state.
    let peer = model.peer_name.clone().unwrap_or_else(|| "peer".to_owned());
    let peer_label = match &model.peer_addr {
        Some(a) => format!("{peer} ({a})"),
        None => peer,
    };
    spans.push(Span::styled(peer_label, theme.accent()));
    spans.push(Span::raw(" "));
    if model.connected {
        spans.push(Span::styled(
            format!("{} connected", theme.g("✓", "OK")),
            theme.fg(Color::Green),
        ));
    } else {
        spans.push(Span::styled(
            format!("{} reconnecting…", theme.g("○", "o")),
            theme.fg(Color::Yellow),
        ));
    }
    if model.unresolved > 0 {
        spans.push(sep());
        spans.push(Span::styled(
            format!("{} {}", theme.g("⚠", "!"), model.unresolved),
            theme.fg(Color::Yellow),
        ));
    }
    if let Some(text) = last_sync_text(model) {
        spans.push(sep());
        spans.push(Span::styled(text, theme.dim()));
    }
    if model.filter_editing {
        spans.push(sep());
        spans.push(Span::styled(
            format!(
                "/{}{}",
                model.filter.clone().unwrap_or_default(),
                theme.g("▌", "_")
            ),
            theme.accent(),
        ));
    } else if let Some(fstr) = model.filter.as_deref().filter(|s| !s.is_empty()) {
        spans.push(sep());
        spans.push(Span::styled(format!("filter:/{fstr}"), theme.accent()));
    }
    spans.push(sep());
    if let Some(flash) = &model.flash {
        // A transient flash takes the hint slot so it is never missed.
        spans.push(Span::styled(flash.clone(), theme.accent()));
    } else {
        spans.push(Span::styled(
            "c conflicts · h history · d detach · ? help",
            theme.dim(),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn sep() -> Span<'static> {
    Span::raw(" · ")
}

// ---- conflict center (§3b) ------------------------------------------------

fn render_conflicts(f: &mut Frame, area: Rect, model: &Model, theme: Theme) {
    if model.celebrating() {
        let msg = format!("0 conflicts {}", theme.g("🎉", ""));
        let p = Paragraph::new(msg.trim().to_owned()).block(outer_block(model, theme));
        f.render_widget(p, area);
        return;
    }
    let block = outer_block(model, theme);
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Body (panes) above, two footer lines below.
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);
    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(rows[0]);

    render_conflict_list(f, panes[0], model, theme);
    render_diff_pane(f, panes[1], model, theme);
    render_footer(f, rows[1], rows[2], model, theme);
}

fn outer_block<'a>(model: &Model, theme: Theme) -> Block<'a> {
    let peer = model.peer_name.clone().unwrap_or_else(|| "peer".to_owned());
    let state = if model.connected {
        "connected"
    } else {
        "reconnecting"
    };
    let badge = if model.unresolved > 0 {
        format!(" ── {} {}", theme.g("⚠", "!"), model.unresolved)
    } else {
        String::new()
    };
    let title = format!(" tomo ── {peer} ── {state}{badge} ");
    Block::default()
        .borders(Borders::ALL)
        .border_set(theme.border_set())
        .title(title)
}

fn render_conflict_list(f: &mut Frame, area: Rect, model: &Model, theme: Theme) {
    let visible = model.visible_conflicts();
    let vis_rows = model.vis_rows();
    let mut lines: Vec<Line> = Vec::new();
    if vis_rows.is_empty() {
        lines.push(Line::from(Span::styled("  no conflicts", theme.dim())));
    }
    for (i, row) in vis_rows.iter().enumerate() {
        let selected = i == model.sel;
        let marker = if selected { theme.g("> ", "> ") } else { "  " };
        match row {
            VisRow::Conflict(idx) | VisRow::GroupChild(idx) => {
                let Some(c) = visible.get(*idx) else { continue };
                let indent = if matches!(row, VisRow::GroupChild(_)) {
                    "    "
                } else {
                    ""
                };
                let ago = format_ago(model.now_ms.saturating_sub(c.wall_ms));
                let mut line = vec![
                    Span::styled(marker, theme.accent()),
                    Span::raw(format!("{indent}{}", c.path)),
                    Span::styled(format!("  {ago}  "), theme.dim()),
                    Span::styled(theme.g("⚠", "!"), theme.fg(Color::Yellow)),
                ];
                if selected {
                    for s in &mut line {
                        s.style = s.style.add_modifier(Modifier::BOLD);
                    }
                }
                lines.push(Line::from(line));
                let kept = c.winner.label(model.peer_name.as_deref());
                lines.push(Line::from(Span::styled(
                    format!("      kept: {kept}'s copy"),
                    theme.dim(),
                )));
            }
            VisRow::GroupHeader => {
                let arrow = if model.group_collapsed {
                    theme.g("▸", ">")
                } else {
                    theme.g("▾", "v")
                };
                let peer = model.peer_name.clone().unwrap_or_else(|| "peer".to_owned());
                let n = model.group_size();
                let style = if selected {
                    theme.accent().add_modifier(Modifier::BOLD)
                } else {
                    theme.accent()
                };
                lines.push(Line::from(Span::styled(
                    format!("{marker}adoption from {peer} ({n} files) {arrow}"),
                    style,
                )));
            }
        }
    }
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_set(theme.border_set())
        .title("CONFLICTS");
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_diff_pane(f: &mut Frame, area: Rect, model: &Model, theme: Theme) {
    let mut lines: Vec<Line> = Vec::new();
    if let Some(c) = model.selected_conflict() {
        lines.push(Line::from(Span::styled(
            c.path.clone(),
            theme.accent().add_modifier(Modifier::BOLD),
        )));
        if let Some(detail) = model.diffs.get(&c.id) {
            let w_side = detail.winner.side.label(model.peer_name.as_deref());
            let l_side = detail.loser.side.label(model.peer_name.as_deref());
            let w_ago = format_ago(model.now_ms.saturating_sub(detail.winner.wall_ms));
            let l_ago = format_ago(model.now_ms.saturating_sub(detail.loser.wall_ms));
            lines.push(Line::from(vec![
                Span::raw("on disk now — "),
                Span::styled(w_side, theme.side_style(detail.winner.side)),
                Span::styled(format!(", {w_ago}"), theme.dim()),
            ]));
            lines.push(Line::from(vec![
                Span::raw("in history  — "),
                Span::styled(l_side, theme.side_style(detail.loser.side)),
                Span::styled(format!(", {l_ago}"), theme.dim()),
            ]));
            lines.push(Line::from(Span::styled(
                "─".repeat(area.width.saturating_sub(2) as usize),
                theme.dim(),
            )));
            if detail.diffable {
                for dl in &detail.diff {
                    lines.push(diff_line(dl, theme));
                }
            } else {
                lines.push(Line::from(Span::styled(
                    "binary or oversized; use `tomo restore --stdout` to inspect",
                    theme.dim(),
                )));
            }
        } else {
            lines.push(Line::from(Span::styled("loading diff…", theme.dim())));
        }
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn diff_line(line: &str, theme: Theme) -> Line<'_> {
    let style = if line.starts_with('+') {
        theme.fg(Color::Green)
    } else if line.starts_with('-') {
        theme.fg(Color::Red)
    } else if line.starts_with("@@") {
        theme.fg(Color::Cyan)
    } else {
        theme.dim()
    };
    Line::from(Span::styled(line, style))
}

fn render_footer(f: &mut Frame, hints: Rect, echo: Rect, model: &Model, theme: Theme) {
    let keys = "enter keep · t take yours · b keep both · space skip · a ack all · u undo · ? help";
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(keys, theme.dim()))),
        hints,
    );
    // A transient flash (e.g. an executed undo's CLI echo) takes the echo line;
    // otherwise the magit-style CLI equivalent of the highlighted action.
    if let Some(flash) = &model.flash {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(flash.clone(), theme.accent()))),
            echo,
        );
    } else if let Some(text) = cli_echo(model) {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(text, theme.accent()))),
            echo,
        );
    }
}

// ---- history browser (§3, TUI v2) -----------------------------------------

fn render_picker(f: &mut Frame, area: Rect, model: &Model, theme: Theme) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(theme.border_set())
        .title(" tomo ── history ── pick a path ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(inner);

    // Filter line (less-style, always editing on this screen).
    let filter = Line::from(vec![
        Span::styled("filter ", theme.dim()),
        Span::styled(
            format!("/{}{}", model.picker_filter, theme.g("▌", "_")),
            theme.accent(),
        ),
    ]);
    f.render_widget(Paragraph::new(filter), rows[0]);

    // Path list.
    let paths = model.filtered_paths();
    let mut lines: Vec<Line> = Vec::new();
    if paths.is_empty() {
        lines.push(Line::from(Span::styled("  no matching paths", theme.dim())));
    }
    // Window the list around the selection so it never renders off-pane.
    let height = rows[1].height as usize;
    let start = model.picker_sel.saturating_sub(height.saturating_sub(1));
    for (i, p) in paths.iter().enumerate().skip(start).take(height) {
        lines.push(picker_row(p, i == model.picker_sel, model.now_ms, theme));
    }
    f.render_widget(Paragraph::new(lines), rows[1]);

    let hint = if let Some(flash) = &model.flash {
        Span::styled(flash.clone(), theme.accent())
    } else {
        Span::styled(
            "type to filter · ↑/↓ move · enter open · esc back · ? help",
            theme.dim(),
        )
    };
    f.render_widget(Paragraph::new(Line::from(hint)), rows[2]);
}

fn picker_row(p: &PathRow, selected: bool, now_ms: u64, theme: Theme) -> Line<'static> {
    let marker = if selected { "> " } else { "  " };
    let ago = format_ago(now_ms.saturating_sub(p.last_wall_ms));
    let plural = if p.versions == 1 { "" } else { "s" };
    let mut line = vec![
        Span::styled(marker, theme.accent()),
        Span::raw(p.path.clone()),
        Span::styled(
            format!("  {} version{plural} · {ago}", p.versions),
            theme.dim(),
        ),
    ];
    if selected {
        for s in &mut line {
            s.style = s.style.add_modifier(Modifier::BOLD);
        }
    }
    Line::from(line)
}

fn render_timeline(f: &mut Frame, area: Rect, model: &Model, theme: Theme) {
    let path = model.history_path.as_deref().unwrap_or("");
    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(theme.border_set())
        .title(format!(" tomo ── history ── {path} "));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);
    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(rows[0]);

    render_timeline_list(f, panes[0], model, theme);
    render_timeline_diff(f, panes[1], model, theme);
    render_timeline_footer(f, rows[1], rows[2], model, theme);
}

fn render_timeline_list(f: &mut Frame, area: Rect, model: &Model, theme: Theme) {
    let mut lines: Vec<Line> = Vec::new();
    if model.timeline.is_empty() {
        lines.push(Line::from(Span::styled("  loading…", theme.dim())));
    }
    let height = area.height as usize;
    let start = model.timeline_sel.saturating_sub(height.saturating_sub(1));
    for (i, v) in model.timeline.iter().enumerate().skip(start).take(height) {
        lines.push(timeline_row(v, i == model.timeline_sel, model, theme));
    }
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_set(theme.border_set())
        .title("VERSIONS");
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn timeline_row<'a>(v: &'a VersionRow, selected: bool, model: &Model, theme: Theme) -> Line<'a> {
    let marker = if selected { "> " } else { "  " };
    let is_mark = model.mark == Some(v.id);
    let ago = format_ago(model.now_ms.saturating_sub(v.wall_ms));
    let size = match (v.present, v.size) {
        (true, Some(b)) => crate::history_cmd::human_size(b),
        _ => "deleted".to_owned(),
    };
    let author = v.side.label(model.peer_name.as_deref());
    let mut line = vec![
        Span::styled(marker, theme.accent()),
        Span::styled(if is_mark { "◆ " } else { "" }, theme.fg(Color::Yellow)),
        Span::styled(format!("#{}", v.id), theme.accent()),
        Span::styled(format!("  {ago}  "), theme.dim()),
        Span::raw(format!("{size}  ")),
        Span::styled(author, theme.side_style(v.side)),
    ];
    if v.exec {
        line.push(Span::styled("  exec", theme.dim()));
    }
    if selected {
        for s in &mut line {
            s.style = s.style.add_modifier(Modifier::BOLD);
        }
    }
    Line::from(line)
}

fn render_timeline_diff(f: &mut Frame, area: Rect, model: &Model, theme: Theme) {
    let mut lines: Vec<Line> = Vec::new();
    if let Some(header) = compare_header(model) {
        lines.push(Line::from(Span::styled(
            header,
            theme.accent().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(Span::styled(
            "─".repeat(area.width.saturating_sub(2) as usize),
            theme.dim(),
        )));
        match model
            .diff_pair()
            .and_then(|pair| model.version_diffs.get(&pair))
        {
            Some(detail) if detail.identical => {
                lines.push(Line::from(Span::styled("(no changes)", theme.dim())));
            }
            Some(detail) if detail.diffable => {
                for dl in &detail.diff {
                    lines.push(diff_line(dl, theme));
                }
            }
            Some(_) => {
                lines.push(Line::from(Span::styled(
                    "binary or oversized; use `tomo restore --stdout` to inspect",
                    theme.dim(),
                )));
            }
            None => lines.push(Line::from(Span::styled("loading diff…", theme.dim()))),
        }
    } else if model.selected_version().is_some() {
        lines.push(Line::from(Span::styled(
            "first recorded version — nothing earlier to compare",
            theme.dim(),
        )));
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn render_timeline_footer(f: &mut Frame, hints: Rect, echo: Rect, model: &Model, theme: Theme) {
    let keys = "j/k move · m mark · r restore · esc back · ? help";
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(keys, theme.dim()))),
        hints,
    );
    if let Some(flash) = &model.flash {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(flash.clone(), theme.accent()))),
            echo,
        );
    } else if let Some(text) = restore_echo(model) {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(text, theme.accent()))),
            echo,
        );
    }
}

// ---- overlays -------------------------------------------------------------

fn render_help(f: &mut Frame, area: Rect, _model: &Model, theme: Theme) {
    let rect = centered(area, 62, 18);
    f.render_widget(Clear, rect);
    let lines = vec![
        Line::from(Span::styled(
            "tomo — help",
            theme.accent().add_modifier(Modifier::BOLD),
        )),
        Line::default(),
        Line::from("  main screen"),
        Line::from("    c        conflict center"),
        Line::from("    h        history browser"),
        Line::from("    /        filter the stream by path"),
        Line::from("    PgUp/PgDn scroll back / forward"),
        Line::from("    End / G   jump to latest (re-follow)"),
        Line::from("    ? help · q quit"),
        Line::default(),
        Line::from("  conflict center"),
        Line::from("    j/k move · enter keep · t take · b both"),
        Line::from("    space skip · a ack all · u undo · esc back"),
        Line::default(),
        Line::from("  history browser"),
        Line::from("    picker: type to filter · ↑/↓ · enter open · esc back"),
        Line::from("    timeline: j/k move · m mark/compare · r restore · esc back"),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(theme.border_set())
        .title(" help ");
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

fn render_ack_modal(f: &mut Frame, area: Rect, count: usize, theme: Theme) {
    let rect = centered(area, 44, 6);
    f.render_widget(Clear, rect);
    let lines = vec![
        Line::from(format!("acknowledge all {count} conflicts?")),
        Line::default(),
        Line::from(Span::styled(
            "  enter/y confirm · n/esc cancel",
            theme.dim(),
        )),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(theme.border_set())
        .title(" ack all ");
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

/// The `q`-on-foreground confirmation (UX-V2 §1: quitting a foreground-started
/// session stops the sync; `d` detaches instead).
fn render_stop_modal(f: &mut Frame, area: Rect, theme: Theme) {
    let rect = centered(area, 46, 6);
    f.render_widget(Clear, rect);
    let lines = vec![
        Line::from("stop syncing? (d detaches, leaving it running)"),
        Line::default(),
        Line::from(Span::styled(
            "  enter/y stop session · n/esc keep running",
            theme.dim(),
        )),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(theme.border_set())
        .title(" stop ");
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

/// The `r`-on-timeline restore confirmation (UX-V2 §3): restore a version into
/// the tree; the live session ships it to the peer as an ordinary edit.
#[allow(clippy::too_many_arguments)] // one modal's worth of display fields
fn render_restore_modal(
    f: &mut Frame,
    area: Rect,
    path: &str,
    version: i64,
    size: u64,
    wall_ms: u64,
    deleted: bool,
    model: &Model,
    theme: Theme,
) {
    let rect = centered(area, 60, 6);
    f.render_widget(Clear, rect);
    let ago = format_ago(model.now_ms.saturating_sub(wall_ms));
    let what = if deleted {
        format!("deletion, {ago}")
    } else {
        format!("{}, {ago}", crate::history_cmd::human_size(size))
    };
    let lines = vec![
        Line::from(format!("restore {path} to #{version} ({what})?")),
        Line::default(),
        Line::from(Span::styled(
            "  enter/y confirm · n/esc cancel",
            theme.dim(),
        )),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(theme.border_set())
        .title(" restore ");
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    Rect {
        x,
        y,
        width: w,
        height: h,
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::field_reassign_with_default
)]
mod tests {
    use super::*;
    use crate::ctl::proto::{ConflictSide, Event};
    use crate::tui::state::{
        parse_conflicts, parse_detail, parse_history_log, parse_history_paths, parse_version_diff,
        CmdOutcome, CmdReply, Key, Msg,
    };
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use serde_json::json;

    fn press(model: Model, k: Key) -> Model {
        super::super::state::update(model, Msg::Key(k))
    }
    fn cmd(model: Model, reply: CmdReply) -> Model {
        super::super::state::update(
            model,
            Msg::Cmd(CmdOutcome {
                seq: 0,
                result: Ok(reply),
            }),
        )
    }

    fn theme() -> Theme {
        Theme {
            color: false,
            unicode: true,
        }
    }

    fn draw(model: &Model) -> String {
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let t = theme();
        term.draw(|f| render(f, model, t)).unwrap();
        buffer_text(term.backend())
    }

    fn buffer_text(backend: &TestBackend) -> String {
        let buf = backend.buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn main_screen_shows_stream_transfer_and_status() {
        let mut m = Model::default();
        m.now_ms = 1_000_000;
        m = super::super::state::update(
            m,
            Msg::Event(Event::Connected {
                peer_name: Some("vm8".to_owned()),
                peer_addr: Some("192.168.1.40".to_owned()),
            }),
        );
        m = super::super::state::update(
            m,
            Msg::Event(Event::Synced {
                path: "src/train.py".to_owned(),
                size: 12,
            }),
        );
        m = super::super::state::update(
            m,
            Msg::Event(Event::Transfer {
                path: "model.ckpt".to_owned(),
                done: 58,
                total: 100,
            }),
        );
        m = super::super::state::update(
            m,
            Msg::Event(Event::Heartbeat {
                last_sync_ms_ago: Some(2_000),
                unresolved_conflicts: 1,
            }),
        );
        let out = draw(&m);
        assert!(out.contains("src/train.py"), "stream line: {out}");
        assert!(out.contains("model.ckpt"), "transfer zone: {out}");
        assert!(out.contains("58%"), "progress pct: {out}");
        assert!(out.contains("vm8"), "peer in status: {out}");
        assert!(out.contains("connected"), "connection state: {out}");
        assert!(out.contains("last sync 2s ago"), "recency: {out}");
        assert!(out.contains("c conflicts"), "hints: {out}");
    }

    #[test]
    fn filter_indicator_shows_in_status() {
        let mut m = Model::default();
        m = super::super::state::update(m, Msg::Key(super::super::state::Key::Char('/')));
        for c in "src".chars() {
            m = super::super::state::update(m, Msg::Key(super::super::state::Key::Char(c)));
        }
        let out = draw(&m);
        assert!(out.contains("/src"), "editing filter shown: {out}");
    }

    #[test]
    fn conflict_center_has_mockup_shape() {
        let mut m = Model::default();
        m.now_ms = 1_000_000;
        m.screen = Screen::Conflicts;
        m.peer_name = Some("vm8".to_owned());
        m.connected = true;
        m.unresolved = 2;
        let rows = parse_conflicts(&json!([
            {"id": 7, "path": "src/train.py", "wall_unix_ms": 999_000, "resolved": false,
             "winner": {"origin": "remote"}, "loser": {"origin": "local"}},
            {"id": 8, "path": "src/config.yaml", "wall_unix_ms": 998_000, "resolved": false,
             "winner": {"origin": "remote"}, "loser": {"origin": "local"}}
        ]));
        m = super::super::state::update(
            m,
            Msg::Cmd(CmdOutcome {
                seq: 0,
                result: Ok(CmdReply::Conflicts(rows)),
            }),
        );
        let detail = parse_detail(&json!({
            "path": "src/train.py",
            "diffable": true,
            "diff": ["@@ -18,7 +18,9 @@", "-    lr = 3e-4", "+    lr = 1e-4"],
            "winner": {"origin": "remote", "wall_unix_ms": 999_000},
            "loser": {"origin": "local", "wall_unix_ms": 998_000},
        }))
        .unwrap();
        m = super::super::state::update(
            m,
            Msg::Cmd(CmdOutcome {
                seq: 1,
                result: Ok(CmdReply::Show { id: 7, detail }),
            }),
        );
        let out = draw(&m);
        assert!(out.contains("CONFLICTS"), "list header: {out}");
        assert!(out.contains("src/train.py"), "list row: {out}");
        assert!(out.contains("on disk now"), "framing: {out}");
        assert!(out.contains("in history"), "framing: {out}");
        assert!(out.contains("lr = 1e-4"), "diff line: {out}");
        assert!(
            out.contains("tomo conflicts resolve 7 --keep-current"),
            "cli echo: {out}"
        );
        assert!(out.contains("vm8"), "peer in title: {out}");
    }

    #[test]
    fn adoption_group_header_renders() {
        let mut m = Model::default();
        m.screen = Screen::Conflicts;
        m.peer_name = Some("vm8".to_owned());
        for id in [10, 11, 12] {
            m = super::super::state::update(
                m,
                Msg::Event(Event::Conflict {
                    id: Some(id),
                    path: format!("g{id}.txt"),
                    winner: ConflictSide::Peer,
                    adopted: true,
                }),
            );
        }
        let rows = parse_conflicts(&json!([
            {"id": 10, "path": "g10.txt", "wall_unix_ms": 3, "resolved": false, "winner": {"origin":"remote"}, "loser": {"origin":"local"}},
            {"id": 11, "path": "g11.txt", "wall_unix_ms": 2, "resolved": false, "winner": {"origin":"remote"}, "loser": {"origin":"local"}},
            {"id": 12, "path": "g12.txt", "wall_unix_ms": 1, "resolved": false, "winner": {"origin":"remote"}, "loser": {"origin":"local"}}
        ]));
        m = super::super::state::update(
            m,
            Msg::Cmd(CmdOutcome {
                seq: 0,
                result: Ok(CmdReply::Conflicts(rows)),
            }),
        );
        let out = draw(&m);
        assert!(
            out.contains("adoption from vm8 (3 files)"),
            "group header: {out}"
        );
    }

    #[test]
    fn help_overlay_renders() {
        let mut m = Model::default();
        m.help = true;
        let out = draw(&m);
        assert!(out.contains("help"), "help title: {out}");
        assert!(out.contains("conflict center"), "help body: {out}");
    }

    #[test]
    fn ack_modal_shows_count() {
        let mut m = Model::default();
        m.screen = Screen::Conflicts;
        m.modal = Some(Modal::AckAll { count: 3 });
        let out = draw(&m);
        assert!(out.contains("acknowledge all 3 conflicts?"), "modal: {out}");
    }

    #[test]
    fn celebration_renders() {
        let mut m = Model::default();
        m.screen = Screen::Conflicts;
        // Force the celebration state via the public path: resolve to zero.
        let rows = parse_conflicts(&json!([
            {"id": 1, "path": "a", "wall_unix_ms": 1, "resolved": false, "winner": {"origin":"local"}, "loser": {"origin":"remote"}}
        ]));
        m = super::super::state::update(
            m,
            Msg::Cmd(CmdOutcome {
                seq: 0,
                result: Ok(CmdReply::Conflicts(rows)),
            }),
        );
        m = super::super::state::update(m, Msg::Key(super::super::state::Key::Char('a')));
        m = super::super::state::update(m, Msg::Key(super::super::state::Key::Enter));
        let out = draw(&m);
        assert!(out.contains("0 conflicts"), "celebration: {out}");
    }

    /// A model in the history timeline of `a.txt` (three versions), diff for the
    /// predecessor pair (#20 → #30) delivered.
    fn timeline_view_model() -> Model {
        let mut m = Model::default();
        m.now_ms = 1_000_000;
        m.peer_name = Some("vm8".to_owned());
        m = press(m, Key::Char('h'));
        let paths = parse_history_paths(&json!([
            {"path":"a.txt","versions":3,"last_version":30,"last_wall_unix_ms":999_000}
        ]));
        m = cmd(m, CmdReply::HistoryPaths(paths));
        m = press(m, Key::Enter);
        let versions = parse_history_log(&json!([
            {"id":30,"wall_unix_ms":999_000,"size":3100,"origin":"local","exec":false,"present":true},
            {"id":20,"wall_unix_ms":998_000,"size":3000,"origin":"remote","exec":false,"present":true},
            {"id":10,"wall_unix_ms":997_000,"size":2900,"origin":"local","exec":false,"present":true}
        ]));
        m = cmd(
            m,
            CmdReply::HistoryLog {
                path: "a.txt".to_owned(),
                versions,
            },
        );
        cmd(
            m,
            CmdReply::VersionDiff {
                from: 20,
                to: 30,
                detail: parse_version_diff(&json!({
                    "identical": false, "diffable": true,
                    "diff": ["@@ -1 +1 @@", "-    lr = 3e-4", "+    lr = 1e-4"]
                })),
            },
        )
    }

    #[test]
    fn history_picker_renders_paths_and_filter() {
        let mut m = Model::default();
        m.now_ms = 1_000_000;
        m = press(m, Key::Char('h'));
        let paths = parse_history_paths(&json!([
            {"path":"src/train.py","versions":3,"last_version":30,"last_wall_unix_ms":900_000},
            {"path":"assets/logo.png","versions":1,"last_version":10,"last_wall_unix_ms":800_000}
        ]));
        m = cmd(m, CmdReply::HistoryPaths(paths));
        let out = draw(&m);
        assert!(out.contains("history"), "screen title: {out}");
        assert!(out.contains("src/train.py"), "path row: {out}");
        assert!(out.contains("3 versions"), "version count: {out}");
        assert!(out.contains("filter"), "filter line: {out}");
    }

    #[test]
    fn history_timeline_renders_versions_and_diff() {
        let m = timeline_view_model();
        let out = draw(&m);
        assert!(out.contains("VERSIONS"), "list header: {out}");
        assert!(out.contains("#30"), "version id: {out}");
        assert!(out.contains("vm8"), "peer side label: {out}");
        assert!(out.contains("lr = 1e-4"), "diff line: {out}");
        assert!(
            out.contains("tomo restore a.txt --version 30"),
            "restore echo: {out}"
        );
    }

    #[test]
    fn history_timeline_compare_header_shows_marked_pair() {
        let mut m = timeline_view_model();
        m = press(m, Key::Char('j')); // select #20
        m = press(m, Key::Char('m')); // mark #20
        m = press(m, Key::Char('k')); // back to #30
        let out = draw(&m);
        assert!(out.contains("comparing #20"), "compare header: {out}");
    }

    #[test]
    fn history_restore_modal_renders() {
        let mut m = timeline_view_model();
        m = press(m, Key::Char('r'));
        let out = draw(&m);
        assert!(out.contains("restore a.txt to #30"), "modal prompt: {out}");
        assert!(out.contains("enter/y"), "modal keys: {out}");
    }

    #[test]
    fn too_small_fallback() {
        let mut term = Terminal::new(TestBackend::new(30, 5)).unwrap();
        let t = theme();
        let m = Model::default();
        term.draw(|f| render(f, &m, t)).unwrap();
        let out = buffer_text(term.backend());
        assert!(out.contains("terminal too small"), "fallback: {out}");
    }
}
