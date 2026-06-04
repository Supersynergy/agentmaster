//! Rendering. Pure function of `App`: no side effects, no mutation. TUI best
//! practices baked in — consistent color semantics, one focus highlight, clear
//! empty states, contextual footer, truncation that never overflows, a help
//! overlay, and a responsive layout that survives resize.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

use crate::app::{
    App, ButtonId, CARD_H, CHAT_PANE_H, FOOTER_H, HEADER_H, InputKind, Mode, TOOLBAR, View,
};
use crate::fleet::{Agent, Lane, Source, Status};

// ---- color semantics (one meaning per color, used everywhere) -------------
const C_ACCENT: Color = Color::Cyan;
const C_WORKING: Color = Color::Green;
const C_IDLE: Color = Color::Indexed(244);
const C_BLOCKED: Color = Color::Yellow;
const C_REVIEW: Color = Color::Magenta;
const C_DONE: Color = Color::Blue;
const C_DEAD: Color = Color::Indexed(240);
const C_DIM: Color = Color::Indexed(243);
const C_FAINT: Color = Color::Indexed(238);

fn status_color(s: Status) -> Color {
    match s {
        Status::Queued => C_IDLE,
        Status::Working => C_WORKING,
        Status::Idle => C_IDLE,
        Status::Blocked => C_BLOCKED,
        Status::Review => C_REVIEW,
        Status::Done => C_DONE,
        Status::Dead => C_DEAD,
    }
}

/// A glyph per status — shape conveys state without relying on color (a11y).
/// Working spins so motion signals "actively running".
fn status_glyph(s: Status, spin: char) -> char {
    match s {
        Status::Queued => '◔',
        Status::Working => spin,
        Status::Idle => '◌',
        Status::Blocked => '▲',
        Status::Review => '◍',
        Status::Done => '✓',
        Status::Dead => '✗',
    }
}

/// Card status row: **how long in the current state** — the honest observability
/// metric (works for imported agents whose true start we never owned). A
/// long-blocked agent gets a loud "stuck" marker so it surfaces at a glance.
fn status_line(a: &Agent, col: Color) -> Line<'static> {
    let held = a.in_status_secs();
    let mut spans = vec![
        Span::styled(
            format!("{} ", a.status.label()),
            Style::new().fg(col).bold(),
        ),
        Span::styled(format!("for {}", fmt_dur(held)), Style::new().fg(C_DIM)),
    ];
    // Blocked-too-long is the one thing that needs the operator now.
    if matches!(a.status, Status::Blocked) && held > 300 {
        spans.push(Span::styled("  ⏰", Style::new().fg(C_BLOCKED).bold()));
    } else if matches!(a.status, Status::Idle) && held > 600 {
        spans.push(Span::styled("  💤", Style::new().fg(C_IDLE)));
    }
    // Claude/Codex 1h prompt-cache countdown, ticking down from the last
    // generation. Hot (full) while working; turns red as it nears expiry, then
    // "cold" once the next turn would pay full uncached input cost.
    let kind = agent_kind(a);
    if kind == "claude" || kind == "codex" {
        let rem = a.cache_remaining_secs();
        let (txt, col) = if rem == 0 {
            ("🧊cold".to_string(), C_DEAD)
        } else {
            (format!("🧊{}", fmt_countdown(rem)), cache_color(rem))
        };
        spans.push(Span::styled(format!("  {txt}"), Style::new().fg(col)));
    }
    Line::from(spans)
}

/// Wall-clock HH:MM for an event that happened `secs_ago` seconds in the past.
fn clock_at(secs_ago: i64) -> String {
    (chrono::Local::now() - chrono::Duration::seconds(secs_ago))
        .format("%H:%M")
        .to_string()
}

/// Cache countdown MM:SS (or H:MM:SS for the full hour-plus window).
fn fmt_countdown(s: i64) -> String {
    let s = s.max(0);
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// Cache freshness color: green with plenty left, amber under 15m, red under 5m.
fn cache_color(rem: i64) -> Color {
    if rem <= 300 {
        Color::Red
    } else if rem <= 900 {
        C_BLOCKED
    } else {
        C_WORKING
    }
}

/// Strip cmux's own title decoration (`🟣 Claude ✓ · <task>`) down to the task,
/// and drop a trailing ` [target]` (the tmux pane/cmux ref we already show
/// elsewhere). Our glyph + color convey agent + state, so the rest is noise.
fn clean_title(s: &str) -> &str {
    let s = match s.split_once(" · ") {
        Some((_, task)) if !task.trim().is_empty() => task.trim(),
        _ => s.trim(),
    };
    // Drop cmux's leading ⏱ shell-tab marker so non-agent tabs read cleanly.
    let s = s.trim_start_matches('⏱').trim_start();
    // strip a trailing " [..]" address suffix
    if let Some(idx) = s.rfind(" [")
        && s.ends_with(']')
    {
        return s[..idx].trim_end();
    }
    s
}

/// The title a human should read: the cleaned multiplexer title, unless that's a
/// system fragment (`</task-notification>`, `## ▸ …`) — then the real task pulled
/// from the transcript (`task_label`). This is what makes the list legible instead
/// of a column of `</task-notification>`.
fn display_title(a: &Agent) -> String {
    if crate::peek::is_fragment_title(&a.name)
        && let Some(t) = a.task_label.as_deref()
        && !t.is_empty()
    {
        return t.to_string();
    }
    clean_title(&a.name).to_string()
}

/// Agent kind (claude / codex / …) parsed from a cmux-style title prefix, for a
/// compact badge that replaces the redundant `cmux` runtime label.
fn agent_kind(a: &Agent) -> &str {
    let head = a.name.split(" · ").next().unwrap_or("").to_lowercase();
    if head.contains("codex") {
        "codex"
    } else if head.contains("claude") {
        "claude"
    } else {
        a.runtime.as_str()
    }
}

/// Where the agent lives — repo dir for native, the stable backend ref for
/// imported agents (so line 2 isn't blank for cmux/tmux agents).
fn source_label(a: &Agent) -> String {
    match &a.source {
        Source::Native => {
            if a.project.is_empty() {
                a.cwd.clone()
            } else {
                a.project.clone()
            }
        }
        Source::Cmux(r) => r.clone(),
        Source::Tmux(t) => t.clone(),
    }
}

/// A 10-cell unicode progress bar. Shape carries the value without color (a11y).
fn progress_bar(p: u8) -> String {
    let filled = (p as usize * 10 / 100).min(10);
    let mut s = String::with_capacity(10);
    for i in 0..10 {
        s.push(if i < filled { '▰' } else { '▱' });
    }
    s
}

/// Progress color ramps blocked→working→done so a glance reads "how far along".
fn progress_color(p: u8) -> Color {
    if p >= 100 {
        C_DONE
    } else if p >= 60 {
        C_WORKING
    } else if p >= 25 {
        C_BLOCKED
    } else {
        C_IDLE
    }
}

fn kind_color(kind: &str) -> Color {
    match kind {
        "spawn" => C_WORKING,
        "kill" | "error" => Color::Red,
        "exit" | "stop" => C_DEAD,
        "send" => C_ACCENT,
        "state" => C_REVIEW,
        _ => C_DIM,
    }
}

pub fn render(f: &mut Frame, app: &App) {
    let area = f.area();
    // header · board · permanent orchestrator chat pane · footer
    let rows = Layout::vertical([
        Constraint::Length(HEADER_H),
        Constraint::Min(3),
        Constraint::Length(CHAT_PANE_H),
        Constraint::Length(FOOTER_H),
    ])
    .split(area);

    header(f, rows[0], app);

    match app.mode {
        Mode::Help => help(f, rows[1]),
        Mode::Inspect => inspect(f, rows[1], app),
        _ => match app.view {
            View::List => list(f, rows[1], app),
            View::Kanban => kanban(f, rows[1], app),
            View::Tree => tree(f, rows[1], app),
            View::Logs => logs(f, rows[1], app),
        },
    }

    chat_pane(f, rows[2], app);
    footer(f, rows[3], app);

    // Modal input box only for the non-chat prompts (new/send/filter/goal);
    // the orchestrator types straight into its pinned bottom pane.
    if app.mode == Mode::Input && app.input_kind != InputKind::Orchestrate {
        input_overlay(f, area, app);
    }
}

/// The always-on orchestrator chat, pinned to the bottom. Shows the routed-message
/// transcript + a live input line. Type into it with `o` (or click the pane);
/// `#N <msg>` steers agent N, `#* <msg>` broadcasts, `v` dictates by voice.
fn chat_pane(f: &mut Frame, area: Rect, app: &App) {
    let focused = app.mode == Mode::Input && app.input_kind == InputKind::Orchestrate;
    let border = if focused { C_ACCENT } else { C_FAINT };
    let title = if focused {
        " 💬 orchestrator — typing (Enter route · Esc back) ".to_string()
    } else {
        " 💬 orchestrator — o to talk · #N msg · #* all · v voice ".to_string()
    };
    let block = Block::bordered()
        .title(title)
        .border_style(Style::new().fg(border));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 {
        return;
    }
    let parts = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);

    // transcript (last N routed messages)
    let h = parts[0].height as usize;
    let start = app.chat_log.len().saturating_sub(h);
    let lines: Vec<Line> = if app.chat_log.is_empty() {
        vec![Line::from(Span::styled(
            "no messages yet — press o, then  #3 run the tests",
            Style::new().fg(C_FAINT),
        ))]
    } else {
        app.chat_log
            .iter()
            .skip(start)
            .map(|m| Line::from(trunc(m, parts[0].width as usize)))
            .collect()
    };
    f.render_widget(Paragraph::new(lines), parts[0]);

    // input line
    let input = if focused {
        Line::from(vec![
            Span::styled("› ", Style::new().fg(C_ACCENT).bold()),
            Span::raw(app.input.clone()),
            Span::styled("▏", Style::new().fg(C_ACCENT)),
        ])
    } else {
        Line::from(Span::styled(
            "› press o to message your agents",
            Style::new().fg(C_DIM),
        ))
    };
    f.render_widget(
        Paragraph::new(input).style(Style::new().bg(Color::Indexed(235))),
        parts[1],
    );
}

// ---- header ---------------------------------------------------------------

fn header(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered().border_style(Style::new().fg(C_ACCENT));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let rows = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(inner);

    let c = app.fleet.counts();
    let total: usize = c.iter().sum();
    let mut spans = vec![
        Span::styled(
            " agentmaster ",
            Style::new().fg(Color::Black).bg(C_ACCENT).bold(),
        ),
        Span::raw("  "),
        Span::styled(format!("⛁ {total} agents"), Style::new().bold()),
        Span::raw("   "),
        Span::styled(format!("◔ {} queued", c[0]), Style::new().fg(C_IDLE)),
        Span::raw("  "),
        Span::styled(format!("● {} working", c[1]), Style::new().fg(C_WORKING)),
        Span::raw("  "),
        Span::styled(format!("▲ {} blocked", c[2]), Style::new().fg(C_BLOCKED)),
        Span::raw("  "),
        Span::styled(format!("◍ {} review", c[3]), Style::new().fg(C_REVIEW)),
        Span::raw("  "),
        Span::styled(format!("✓ {} done", c[4]), Style::new().fg(C_DONE)),
    ];
    // Needs-you alert: blocked agents are the one thing that wants the operator
    // right now. Loud but STABLE (a solid badge, no strobe — flicker reads as
    // noise, not urgency).
    if c[2] > 0 {
        spans.push(Span::raw("   "));
        spans.push(Span::styled(
            format!(" ⏸ {} NEED YOU ", c[2]),
            Style::new().fg(Color::Black).bg(C_BLOCKED).bold(),
        ));
        // Of those, how many have been blocked long enough to be truly stuck —
        // the subset worth interrupting first.
        let stuck = app
            .fleet
            .agents
            .iter()
            .filter(|a| matches!(a.status, Status::Blocked) && a.in_status_secs() > 300)
            .count();
        if stuck > 0 {
            spans.push(Span::styled(
                format!(" ⏰ {stuck} stuck>5m"),
                Style::new().fg(C_BLOCKED).bold(),
            ));
        }
    }
    f.render_widget(Paragraph::new(Line::from(spans)), rows[0]);

    let view_name = match app.view {
        View::List => "list",
        View::Kanban => "board",
        View::Tree => "tree",
        View::Logs => "logs",
    };
    let cpu = app.metrics.cpu();
    let cpu_col = if cpu > 80.0 {
        Color::Red
    } else if cpu > 50.0 {
        C_BLOCKED
    } else {
        C_WORKING
    };
    // Time-coverage: how many agents show a ground-truth `↩` last-response time
    // (resolved transcript) vs fall back to time-in-state. The operator's at-a-
    // glance trust signal for the clock — high % means the times are real.
    let timed = app
        .fleet
        .agents
        .iter()
        .filter(|a| a.last_response_secs().is_some())
        .count();
    let cov = (timed * 100).checked_div(total).unwrap_or(0);
    let cov_col = if cov >= 90 {
        C_WORKING
    } else if cov >= 60 {
        C_BLOCKED
    } else {
        C_DIM
    };
    let l1 = Line::from(vec![
        Span::styled(format!(" cpu {cpu:>4.0}% "), Style::new().fg(cpu_col)),
        Span::styled(
            format!(
                "mem {:.1}/{:.1}G  ",
                app.metrics.mem_used_gb(),
                app.metrics.mem_total_gb()
            ),
            Style::new().fg(C_DIM),
        ),
        Span::styled(
            format!("↩ {timed}/{total} timed {cov}%  "),
            Style::new().fg(cov_col),
        ),
        Span::styled(
            if app.notify_on { "🔔 " } else { "🔕 " },
            Style::new().fg(if app.notify_on { C_WORKING } else { C_FAINT }),
        ),
        Span::styled(format!("view:{view_name}"), Style::new().fg(C_ACCENT)),
        Span::raw("  │  "),
        Span::styled(
            trunc(&app.status_msg, area.width as usize / 2),
            Style::new().fg(C_DIM),
        ),
    ]);
    f.render_widget(Paragraph::new(l1), rows[1]);
}

// ---- list (master-detail, the primary overview) ---------------------------

/// Dense, full-width, scrollable list of EVERY agent (left ~62%) beside a detail
/// panel for the selected one (right ~38%) — the golden-ratio split a human reads
/// top-down: "who needs me", scan, then read/act. Scales to the whole fleet where
/// the board's cards cannot.
fn list(f: &mut Frame, area: Rect, app: &App) {
    let agents = app.sorted_agents();
    // Responsive: a narrow terminal gets the full width as one compact, symbol-led
    // column (the form Maxim asked for); a wide one keeps the detail pane on the
    // right. Keeping the wide branch is also what keeps `detail_panel` live.
    let wide = area.width >= 100;
    let (list_area, detail_area) = if wide {
        let cols = Layout::horizontal([Constraint::Percentage(62), Constraint::Percentage(38)])
            .split(area);
        (cols[0], Some(cols[1]))
    } else {
        (area, None)
    };

    let noise = if app.hide_noise { "noise" } else { "all" };
    let title = format!(
        " agents {}  ·  sort:{} (S)  ·  H hide:{}  ·  / filter ",
        agents.len(),
        app.sort.label(),
        noise
    );
    // Scroll geometry up front so the position can live in the block's bottom
    // border instead of overlapping a data row. Borders take 2 rows, header 1.
    let vis = (list_area.height as usize).saturating_sub(3).max(1);
    let scroll = if app.sel >= vis { app.sel + 1 - vis } else { 0 };
    let mut lblock = Block::bordered()
        .title(title)
        .border_style(Style::new().fg(C_ACCENT));
    if agents.len() > vis {
        let shown_end = (scroll + vis).min(agents.len());
        lblock = lblock.title_bottom(
            Line::from(format!(
                " {}–{} of {}  j/k scroll ",
                scroll + 1,
                shown_end,
                agents.len()
            ))
            .right_aligned(),
        );
    }
    let linner = lblock.inner(list_area);
    f.render_widget(lblock, list_area);
    if agents.is_empty() {
        f.render_widget(
            Paragraph::new("no agents — press d to discover, n to spawn")
                .alignment(Alignment::Center)
                .style(Style::new().fg(C_FAINT)),
            linner,
        );
    } else {
        let parts = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(linner);
        // Legend, not a rigid column header — the row is symbol-led, not tabular.
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  state · 📁proj · agent · task — what it's doing · signals · ↩last · 🧊ttl",
                Style::new().fg(C_FAINT),
            )))
            .style(Style::new().bg(Color::Indexed(235))),
            parts[0],
        );
        let w = parts[1].width as usize;
        // Use the same `scroll`/`vis` computed for the bottom-border position, so
        // the rows and the "N–M of K" indicator never disagree.
        let spin = app.spin();
        let end = (scroll + vis).min(agents.len());
        let rows: Vec<Line> = agents[scroll..end]
            .iter()
            .enumerate()
            .map(|(i, a)| agent_row(a, scroll + i == app.sel, spin, w))
            .collect();
        f.render_widget(Paragraph::new(rows), parts[1]);
    }

    // ---- right: detail of the selected agent (+ its auto-peek digest) ----
    if let Some(da) = detail_area {
        let sel = agents.get(app.sel).copied();
        let digest = sel.and_then(|a| app.peek_for(a.id));
        detail_panel(f, da, sel, digest);
    }
}

/// One dense row in the list. Fixed-width columns then the title fills the rest.
/// Display width in terminal cells (emoji count as 2), so fixed columns stay
/// aligned even when content mixes emoji and ASCII.
fn cells(s: &str) -> usize {
    use unicode_width::UnicodeWidthStr;
    s.width()
}

/// Right-pad `s` with spaces to exactly `n` cells (truncating to fit if longer).
fn pad_cells(s: &str, n: usize) -> String {
    let w = cells(s);
    if w == n {
        return s.to_string();
    }
    if w < n {
        return format!("{s}{}", " ".repeat(n - w));
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let cw = cells(&ch.to_string());
        if used + cw > n {
            break;
        }
        out.push(ch);
        used += cw;
    }
    while cells(&out) < n {
        out.push(' ');
    }
    out
}

/// A stable emoji per project, so the eye can group the fleet by repo at a glance.
/// Substring match keeps it robust to path/casing variants; all icons are 2-cell.
/// (Future: load an override map from `~/.config/agentmaster/icons.toml`.)
fn project_icon(project: &str) -> &'static str {
    let p = project.to_ascii_lowercase();
    if p.is_empty() {
        "  " // native shell, no repo
    } else if p.contains("agentmaster") {
        "🤖"
    } else if p.contains("synapse") {
        "🔮"
    } else if p.contains("event") {
        "🎫"
    } else if p.contains("supermax") {
        "⚡"
    } else if p.contains("supersyn") {
        "🌀"
    } else if p.contains("cmux") {
        "🧩"
    } else if p.contains("lead") {
        "🧲"
    } else if p.contains("crm") {
        "📇"
    } else if p.contains("zeroclaw") {
        "🦀"
    } else if p.contains("achiev") {
        "🏆"
    } else {
        "📁"
    }
}

/// Claude vs Codex (vs any other runtime) as a short colored word — replaces the
/// old 🟣/🧠 emoji. Color carries the identity; the word stays readable in a
/// screenshot or logfile. Claude = warm amber, Codex = cool blue.
fn agent_tag(a: &Agent) -> (String, Color) {
    match agent_kind(a) {
        "claude" => ("Claude".to_string(), Color::Indexed(215)),
        "codex" => ("Codex".to_string(), Color::Indexed(75)),
        other => (other.to_string(), C_DIM),
    }
}

/// Per-agent signal glyphs — only the ones that currently apply, most-urgent
/// first. Each carries info (waiting on you, uncommitted work, goal progress,
/// workflow phase, long idle) so a glance says what each agent *needs*.
fn agent_flags(a: &Agent) -> Vec<Span<'static>> {
    let mut v: Vec<Span<'static>> = Vec::new();
    // 1) waiting on you — the one thing that needs the operator now.
    if matches!(a.status, Status::Blocked) {
        let held = a.in_status_secs();
        if held > 300 {
            v.push(Span::styled(
                format!("⏰{} ", fmt_dur(held)),
                Style::new().fg(C_BLOCKED).bold(),
            ));
        } else {
            v.push(Span::styled(
                "🔔 ".to_string(),
                Style::new().fg(C_BLOCKED).bold(),
            ));
        }
    }
    // 2) goal progress
    if a.has_goal() {
        v.push(Span::styled(
            format!("🎯{}% ", a.progress),
            Style::new().fg(progress_color(a.progress)),
        ));
    }
    // 3) repo state — uncommitted / unpushed work waiting
    if let Some(g) = a.git.as_deref() {
        let (sym, col) = match g {
            "dirty" => ("⎇dirty", C_BLOCKED),
            "ahead" => ("⇡ahead", C_REVIEW),
            "clean" => ("⎇clean", C_WORKING),
            _ => ("⎇·", C_DIM),
        };
        v.push(Span::styled(format!("{sym} "), Style::new().fg(col)));
    }
    // 4) workflow phase
    if let Some(p) = a.phase.as_deref() {
        v.push(Span::styled(format!("◇{p} "), Style::new().fg(C_ACCENT)));
    }
    // 5) gone quiet for a long time
    if matches!(a.status, Status::Idle) && a.in_status_secs() > 600 {
        v.push(Span::styled(
            format!("💤{} ", fmt_dur(a.in_status_secs())),
            Style::new().fg(C_IDLE),
        ));
    }
    v
}

/// One compact, symbol-led row: `status · 📁proj · agent · task · signals · ↩last · 🧊ttl`.
/// Leading status glyph (1 cell, colored) says *what is happening*; the project
/// icon and agent tag say *where* and *who*; the flags say *what it needs*; the
/// right edge holds the real last-response age and the cache TTL.
fn agent_row(a: &Agent, selected: bool, spin: char, width: usize) -> Line<'static> {
    let col = status_color(a.status);
    let glyph = status_glyph(a.status, spin);
    let sel_bg = if selected {
        Style::new().bg(Color::Indexed(237))
    } else {
        Style::new()
    };
    let mark = if selected { "▸" } else { " " };

    let (tag, tag_col) = agent_tag(a);
    let proj = project_icon(&a.project);

    // Right meta: real last-response age (transcript mtime) + cache TTL.
    let cache = a.cache_remaining_secs();
    let cache_txt = if matches!(a.status, Status::Working) {
        "hot".to_string()
    } else if cache == 0 {
        "cold".to_string()
    } else {
        fmt_countdown(cache)
    };
    let cache_col = if matches!(a.status, Status::Working) {
        C_WORKING
    } else {
        cache_color(cache)
    };
    let (age_txt, age_col) = match a.last_response_secs() {
        Some(s) => (
            format!("↩{}", fmt_dur(s)),
            if s > 1800 { C_BLOCKED } else { C_DIM },
        ),
        None => (fmt_dur(a.in_status_secs()), C_FAINT),
    };
    let time = format!("{age_txt}  🧊{cache_txt}");
    let time_cells = cells(&time);

    // Flags, capped to a fixed budget so the time block always aligns right.
    const FLAG_BUDGET: usize = 14;
    let mut acc = 0usize;
    let mut flags: Vec<Span<'static>> = Vec::new();
    for sp in agent_flags(a) {
        let c = cells(&sp.content);
        if acc + c > FLAG_BUDGET {
            break;
        }
        acc += c;
        flags.push(sp);
    }
    let flag_cells = acc;

    // Left identity block = mark(1)+glyph(1)+sp(1) + proj(2)+sp(1) + tag(6)+sp(1) = 13.
    let left_fixed = 13;
    let title_w = width
        .saturating_sub(left_fixed + FLAG_BUDGET + time_cells + 2)
        .max(10);
    let task = pad_cells(&display_title(a), title_w);

    // Push the time block to the right edge: slack the flags didn't use.
    let used = left_fixed + cells(&task) + flag_cells;
    let gap = width.saturating_sub(used + time_cells).max(1);

    let mut spans: Vec<Span<'static>> = vec![
        Span::styled(
            format!("{mark}{glyph} "),
            Style::new().fg(col).bold().patch(sel_bg),
        ),
        Span::styled(format!("{proj} "), sel_bg),
        Span::styled(pad_cells(&tag, 6), Style::new().fg(tag_col).patch(sel_bg)),
        Span::styled(" ", sel_bg),
        Span::styled(
            task,
            if selected {
                Style::new().fg(Color::White).bold().patch(sel_bg)
            } else {
                Style::new().fg(Color::Indexed(252)).patch(sel_bg)
            },
        ),
    ];
    for mut sp in flags {
        sp.style = sp.style.patch(sel_bg);
        spans.push(sp);
    }
    spans.push(Span::styled(" ".repeat(gap), sel_bg));
    spans.push(Span::styled(
        format!("{age_txt}  "),
        Style::new().fg(age_col).patch(sel_bg),
    ));
    spans.push(Span::styled(
        format!("🧊{cache_txt}"),
        Style::new().fg(cache_col).patch(sel_bg),
    ));
    Line::from(spans)
}

/// Right pane: everything about the selected agent + the actions you can take.
fn detail_panel(
    f: &mut Frame,
    area: Rect,
    a: Option<&Agent>,
    digest: Option<&crate::peek::Digest>,
) {
    let block = Block::bordered()
        .title(" detail — ↵ inspect · f tab · s send · g goal ")
        .border_style(Style::new().fg(C_FAINT));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let Some(a) = a else {
        f.render_widget(
            Paragraph::new("nothing selected").style(Style::new().fg(C_FAINT)),
            inner,
        );
        return;
    };
    let col = status_color(a.status);
    let w = inner.width as usize;
    let mut lines = vec![
        Line::from(Span::styled(
            trunc(&display_title(a), w),
            Style::new().fg(Color::White).bold(),
        )),
        Line::from(vec![
            Span::styled(format!("{} ", agent_kind(a)), Style::new().fg(C_ACCENT)),
            Span::styled(source_label(a), Style::new().fg(C_DIM)),
        ]),
        Line::from(vec![
            Span::styled(
                format!("{} ", a.status.label()),
                Style::new().fg(col).bold(),
            ),
            Span::styled(
                format!("for {}   ", fmt_dur(a.in_status_secs())),
                Style::new().fg(C_DIM),
            ),
            Span::styled(
                if matches!(a.status, Status::Working) {
                    "🧊 cache hot".to_string()
                } else if a.cache_remaining_secs() == 0 {
                    "🧊 cache cold".to_string()
                } else {
                    format!("🧊 {}", fmt_countdown(a.cache_remaining_secs()))
                },
                Style::new().fg(cache_color(a.cache_remaining_secs())),
            ),
        ]),
        // Real last-response time off the transcript mtime: when did this agent
        // last actually do something? (the precise clock the user asked for)
        Line::from(match a.last_response_secs() {
            Some(s) => Span::styled(
                format!("↩ last response {} ago  ({})", fmt_dur(s), clock_at(s)),
                Style::new().fg(if s > 1800 { C_BLOCKED } else { C_WORKING }),
            ),
            None => Span::styled(
                "↩ last response: (no transcript linked)",
                Style::new().fg(C_FAINT),
            ),
        }),
    ];
    // Rich cmux tags: repo state + workflow phase, when reported. "dirty/ahead"
    // means there's uncommitted/unpushed work waiting — flag it amber.
    if a.git.is_some() || a.phase.is_some() {
        let mut tag_spans = Vec::new();
        if let Some(g) = a.git.as_deref() {
            let col = match g {
                "dirty" | "ahead" => C_BLOCKED,
                "clean" => C_WORKING,
                _ => C_DIM,
            };
            tag_spans.push(Span::styled(format!("⎇ {g}"), Style::new().fg(col)));
        }
        if let Some(p) = a.phase.as_deref() {
            if !tag_spans.is_empty() {
                tag_spans.push(Span::styled("  ·  ", Style::new().fg(C_FAINT)));
            }
            tag_spans.push(Span::styled(format!("◇ {p}"), Style::new().fg(C_ACCENT)));
        }
        lines.push(Line::from(tag_spans));
    }
    if a.has_goal() {
        lines.push(Line::from(vec![
            Span::styled(
                format!("🎯 {} {:>3}% ", progress_bar(a.progress), a.progress),
                Style::new().fg(progress_color(a.progress)).bold(),
            ),
            Span::styled(
                trunc(a.goal.as_deref().unwrap_or(""), w.saturating_sub(20)),
                Style::new().fg(Color::Indexed(250)),
            ),
        ]));
    }
    // Auto-peek: what the agent last said + its inferred next move, read straight
    // off the transcript (zero token cost). Far more useful than the raw screen
    // tail, so it leads; the raw output fills whatever rows remain below.
    let has_peek =
        digest.is_some_and(|d| !d.last_assistant.is_empty() || !d.next_action.is_empty());
    if let Some(d) = digest.filter(|_| has_peek) {
        lines.push(Line::from(Span::styled(
            "─ last said ─",
            Style::new().fg(C_FAINT),
        )));
        if !d.last_user.is_empty() {
            lines.push(wrapped("🧑", &d.last_user, w, Color::Indexed(110)));
        }
        if !d.last_assistant.is_empty() {
            for (i, seg) in wrap_text(&d.last_assistant, w.saturating_sub(2), 3)
                .into_iter()
                .enumerate()
            {
                let pre = if i == 0 { "🤖 " } else { "   " };
                lines.push(Line::from(Span::styled(
                    format!("{pre}{seg}"),
                    Style::new().fg(Color::Indexed(252)),
                )));
            }
        }
        if !d.next_action.is_empty() {
            lines.push(wrapped("🎯 next:", &d.next_action, w, C_REVIEW));
        }
    }
    lines.push(Line::from(Span::styled(
        "─ recent ─",
        Style::new().fg(C_FAINT),
    )));
    let body_h = inner.height as usize;
    let used = lines.len();
    let tail = body_h.saturating_sub(used);
    let start = a.output.len().saturating_sub(tail);
    for l in a.output.iter().skip(start) {
        lines.push(Line::from(Span::styled(
            trunc(l.trim(), w),
            Style::new().fg(C_DIM),
        )));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

// ---- kanban ---------------------------------------------------------------

fn kanban(f: &mut Frame, area: Rect, app: &App) {
    let cols = Layout::horizontal([Constraint::Ratio(1, 5); 5]).split(area);
    for (i, lane) in Lane::ALL.iter().enumerate() {
        let mut agents = app.fleet.in_lane(*lane);
        // Surface urgency: in BLOCKED + REVIEW, the longest-waiting agent is the
        // one to act on first, so sort by time-in-state (descending) → top card.
        if matches!(lane, Lane::Blocked | Lane::Review) {
            agents.sort_by_key(|b| std::cmp::Reverse(b.in_status_secs()));
        }
        let focused = i == app.lane_idx;
        let title = format!(" {} {} ", lane.title(), agents.len());
        let bstyle = if focused {
            Style::new().fg(C_ACCENT).bold()
        } else {
            Style::new().fg(C_FAINT)
        };
        let btype = if focused {
            BorderType::Thick
        } else {
            BorderType::Rounded
        };
        let block = Block::bordered()
            .title(title)
            .border_style(bstyle)
            .border_type(btype);
        let inner = block.inner(cols[i]);
        f.render_widget(block, cols[i]);
        render_cards(f, inner, &agents, focused, app);
    }
}

fn render_cards(f: &mut Frame, inner: Rect, agents: &[&Agent], focused: bool, app: &App) {
    if agents.is_empty() {
        let p = Paragraph::new(Line::from(Span::styled(
            "— empty —",
            Style::new().fg(C_FAINT),
        )))
        .alignment(Alignment::Center);
        f.render_widget(p, inner);
        return;
    }
    // Reserve one row for the scroll indicator when the lane overflows, so the
    // hidden cards are reachable (j/k scrolls) instead of a dead "+N more".
    let overflow = agents.len() > (inner.height / CARD_H) as usize;
    let body_h = if overflow {
        inner.height.saturating_sub(1)
    } else {
        inner.height
    };
    let vis = (body_h / CARD_H).max(1) as usize;
    // Scroll the focused lane so the selected card is always on screen.
    let offset = if focused && app.card_idx >= vis {
        app.card_idx - vis + 1
    } else {
        0
    };
    let end = (offset + vis).min(agents.len());
    let constraints: Vec<Constraint> = (0..vis).map(|_| Constraint::Length(CARD_H)).collect();
    let slots = Layout::vertical(constraints).split(Rect {
        height: body_h,
        ..inner
    });
    let spin = app.spin();
    for (slot_i, agent) in agents[offset..end].iter().enumerate() {
        let selected = focused && (offset + slot_i) == app.card_idx;
        card(f, slots[slot_i], agent, selected, &app.filter, spin);
    }
    if overflow {
        let above = offset;
        let below = agents.len().saturating_sub(end);
        let note = Rect {
            x: inner.x,
            y: inner.y + body_h,
            width: inner.width,
            height: 1,
        };
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("  ↑{above} above · ↓{below} below — j/k scroll"),
                Style::new().fg(C_DIM),
            )))
            .alignment(Alignment::Center),
            note,
        );
    }
}

fn card(f: &mut Frame, area: Rect, a: &Agent, selected: bool, filter: &str, spin: char) {
    let col = status_color(a.status);
    let dim = matches!(a.status, Status::Idle | Status::Dead);
    let matched = filter.is_empty()
        || a.name.to_lowercase().contains(&filter.to_lowercase())
        || a.last_line.to_lowercase().contains(&filter.to_lowercase());

    // Selected card: a STABLE bright highlight (no blink) + a border that spells
    // out the two actions, so clicking feels intentional without strobing.
    let bstyle = if selected {
        Style::new().fg(Color::White).bold()
    } else if !matched {
        Style::new().fg(C_FAINT)
    } else {
        Style::new().fg(Color::Indexed(239))
    };
    let mut block = Block::bordered()
        .border_type(if selected {
            BorderType::Thick
        } else {
            BorderType::Rounded
        })
        .border_style(bstyle);
    if selected {
        // Subtle bg + an action hint: Enter opens it inside, f/click jumps to the
        // real session tab. Native agents have no tab, so only show the inspect cue.
        block = block.style(Style::new().bg(Color::Indexed(236)));
        let hint = if matches!(a.source, Source::Native) {
            " ↵ inspect ".to_string()
        } else {
            " ↵ inspect · f→tab ".to_string()
        };
        block = block.title(hint);
    }
    let inner = block.inner(area);
    f.render_widget(block, area);

    let w = inner.width as usize;
    let name_style = if dim {
        Style::new().fg(C_IDLE)
    } else {
        Style::new().bold()
    };
    // Glyph carries meaning beyond color: a spinner = actively working, distinct
    // shapes for the rest. Working agents visibly move so "alive" is obvious.
    let glyph = status_glyph(a.status, spin);
    // Kind badge (claude/codex/runtime) replaces the noisy duplicate `cmux cmux`;
    // the title is stripped of cmux's own glyph decoration.
    let kind = agent_kind(a);
    let lines = vec![
        Line::from(vec![
            Span::styled(format!("{glyph} "), Style::new().fg(col).bold()),
            Span::styled(format!("{} ", trunc(kind, 7)), Style::new().fg(C_ACCENT)),
            Span::styled(
                trunc(&display_title(a), w.saturating_sub(kind.len() + 5)),
                name_style,
            ),
        ]),
        Line::from(vec![
            Span::styled("⌂ ", Style::new().fg(C_DIM)),
            Span::styled(
                trunc(&source_label(a), w.saturating_sub(14)),
                Style::new().fg(C_DIM),
            ),
            Span::styled(
                format!(
                    "  pid {}{}",
                    a.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into()),
                    if a.mem_bytes > 0 {
                        format!(" · {}", fmt_mem(a.mem_bytes))
                    } else {
                        String::new()
                    }
                ),
                Style::new().fg(C_FAINT),
            ),
        ]),
        status_line(a, col),
        if a.has_goal() {
            // Goal line wins the 4th row: progress bar + goal text, so the board
            // shows movement toward the objective, not just the latest log line.
            Line::from(vec![
                Span::styled("🎯 ", Style::new().fg(C_REVIEW)),
                Span::styled(
                    format!("{} ", progress_bar(a.progress)),
                    Style::new().fg(progress_color(a.progress)),
                ),
                Span::styled(
                    format!("{:>3}% ", a.progress),
                    Style::new().fg(progress_color(a.progress)).bold(),
                ),
                Span::styled(
                    trunc(a.goal.as_deref().unwrap_or(""), w.saturating_sub(12)),
                    Style::new().fg(Color::Indexed(250)),
                ),
            ])
        } else {
            Line::from(vec![
                Span::styled("▸ ", Style::new().fg(col)),
                Span::styled(
                    trunc(a.last_line.trim(), w.saturating_sub(2)),
                    Style::new().fg(Color::Indexed(250)),
                ),
            ])
        },
    ];
    f.render_widget(Paragraph::new(lines), inner);
}

// ---- inspect (focus one agent) --------------------------------------------

fn inspect(f: &mut Frame, area: Rect, app: &App) {
    let Some(id) = app.current_agent_id() else {
        f.render_widget(
            Paragraph::new("no agent selected").alignment(Alignment::Center),
            area,
        );
        return;
    };
    let Some(a) = app.fleet.get(id) else { return };
    let title = format!(
        " inspect: {} [{}] pid {} — Esc back · s send · f→live tab · g goal · p peek ",
        a.name,
        a.status.label(),
        a.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into())
    );
    let block = Block::bordered()
        .title(title)
        .border_style(Style::new().fg(status_color(a.status)));
    let inner = block.inner(area);
    f.render_widget(block, area);
    // Reserve a goal row only when a goal is pinned, so non-goal agents are unchanged.
    let parts = if a.has_goal() {
        Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(inner)
    } else {
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(inner)
    };

    // Detail line: the exact command, cwd, and branch (if known).
    let branch = a
        .branch
        .as_deref()
        .map(|b| format!(" ⎇ {b}"))
        .unwrap_or_default();
    let detail = Line::from(vec![
        Span::styled(
            format!("$ {} {}", a.program, a.args.join(" ")),
            Style::new().fg(C_ACCENT),
        ),
        Span::styled(format!("   {}{}", a.cwd, branch), Style::new().fg(C_DIM)),
    ]);
    f.render_widget(
        Paragraph::new(detail).style(Style::new().bg(Color::Indexed(235))),
        parts[0],
    );

    // Goal row (only present when pinned): progress bar + % + goal · DoD.
    let out_slot = if a.has_goal() {
        let dod = a
            .done_def
            .as_deref()
            .map(|d| format!("   ✓dod: {d}"))
            .unwrap_or_default();
        let goal = Line::from(vec![
            Span::styled(
                format!("🎯 {} {:>3}%  ", progress_bar(a.progress), a.progress),
                Style::new().fg(progress_color(a.progress)).bold(),
            ),
            Span::styled(
                a.goal.as_deref().unwrap_or("").to_string(),
                Style::new().fg(Color::Indexed(252)),
            ),
            Span::styled(dod, Style::new().fg(C_DIM)),
        ]);
        f.render_widget(
            Paragraph::new(goal).style(Style::new().bg(Color::Indexed(234))),
            parts[1],
        );
        parts[2]
    } else {
        parts[1]
    };

    let h = out_slot.height as usize;
    // Scroll back through the buffer with the wheel / j-k (0 = live tail).
    let max_scroll = a.output.len().saturating_sub(h);
    let scroll = app.inspect_scroll.min(max_scroll);
    let start = max_scroll - scroll;
    let lines: Vec<Line> = a
        .output
        .iter()
        .skip(start)
        .take(h)
        .map(|l| Line::from(trunc(l, out_slot.width as usize)))
        .collect();
    let body = if lines.is_empty() {
        vec![Line::from(Span::styled(
            "(no output yet)",
            Style::new().fg(C_FAINT),
        ))]
    } else {
        lines
    };
    f.render_widget(Paragraph::new(body), out_slot);
}

// ---- tree (process / project hierarchy) -----------------------------------

fn tree(f: &mut Frame, area: Rect, app: &App) {
    use std::collections::BTreeMap;
    let block = Block::bordered()
        .title(" process tree — project ▸ agents ")
        .border_style(Style::new().fg(C_ACCENT));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut groups: BTreeMap<&str, Vec<&Agent>> = BTreeMap::new();
    for a in &app.fleet.agents {
        groups.entry(a.project.as_str()).or_default().push(a);
    }

    let mut lines: Vec<Line> = Vec::new();
    for (proj, ags) in &groups {
        lines.push(Line::from(vec![
            Span::styled(format!("▾ {proj}"), Style::new().fg(C_ACCENT).bold()),
            Span::styled(format!("  ({})", ags.len()), Style::new().fg(C_DIM)),
        ]));
        for a in ags {
            lines.push(Line::from(vec![
                Span::raw("   ├─ "),
                Span::styled("● ", Style::new().fg(status_color(a.status))),
                Span::raw(format!("{:<18}", trunc(&a.name, 18))),
                Span::styled(format!("{:<8}", a.runtime), Style::new().fg(C_ACCENT)),
                Span::styled(
                    format!(
                        "pid {:<8}",
                        a.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into())
                    ),
                    Style::new().fg(C_DIM),
                ),
                Span::styled(
                    format!("{:<8}", a.status.label()),
                    Style::new().fg(status_color(a.status)),
                ),
                Span::styled(
                    format!("cpu {:>4.0}%  mem {:<6}  ", a.cpu, fmt_mem(a.mem_bytes)),
                    Style::new().fg(C_DIM),
                ),
                Span::styled(
                    format!("⏱ {}", fmt_dur(a.age_secs())),
                    Style::new().fg(C_DIM),
                ),
            ]));
        }
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "no agents — press n to spawn one",
            Style::new().fg(C_FAINT),
        )));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

// ---- logs (audit / observability stream) ----------------------------------

fn logs(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered()
        .title(" orchestrator chat ▸ event log (SQLite + JSONL) ")
        .border_style(Style::new().fg(C_ACCENT));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Top: the orchestrator transcript — what you told which agent (newest last).
    let chat_h = (inner.height / 3).clamp(2, 6) as usize;
    let rows = Layout::vertical([
        Constraint::Length(chat_h as u16),
        Constraint::Length(1),
        Constraint::Min(1),
    ])
    .split(inner);
    let chat_start = app.chat_log.len().saturating_sub(chat_h);
    let chat_lines: Vec<Line> = app
        .chat_log
        .iter()
        .skip(chat_start)
        .map(|l| {
            Line::from(Span::styled(
                trunc(l, rows[0].width as usize),
                Style::new().fg(C_ACCENT),
            ))
        })
        .collect();
    f.render_widget(Paragraph::new(chat_lines), rows[0]);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "─ events ─",
            Style::new().fg(C_FAINT),
        ))),
        rows[1],
    );
    let inner = rows[2];

    let evs = app.store.recent(inner.height as i64 * 2);
    let flt = app.filter.to_lowercase();
    let lines: Vec<Line> = evs
        .iter()
        .filter(|(_, name, kind, msg)| {
            flt.is_empty()
                || name.to_lowercase().contains(&flt)
                || kind.to_lowercase().contains(&flt)
                || msg.to_lowercase().contains(&flt)
        })
        .take(inner.height as usize)
        .map(|(ts, name, kind, msg)| {
            Line::from(vec![
                Span::styled(format!("{} ", short_ts(ts)), Style::new().fg(C_DIM)),
                Span::styled(format!("{kind:<7}"), Style::new().fg(kind_color(kind))),
                Span::styled(
                    format!("{:<16}", trunc(name, 16)),
                    Style::new().fg(C_ACCENT),
                ),
                Span::raw(trunc(msg, inner.width as usize)),
            ])
        })
        .collect();
    let body = if lines.is_empty() {
        vec![Line::from(Span::styled(
            "no events yet",
            Style::new().fg(C_FAINT),
        ))]
    } else {
        lines
    };
    f.render_widget(Paragraph::new(body), inner);
}

// ---- footer + overlays ----------------------------------------------------

fn footer(f: &mut Frame, area: Rect, app: &App) {
    let bg = Style::new().bg(Color::Indexed(235));
    if app.mode == Mode::Normal {
        // Clickable toolbar. Active view button is highlighted; every button is a
        // real click target (see app::toolbar_hit).
        let active = match app.view {
            View::List => ButtonId::List,
            View::Kanban => ButtonId::Kanban,
            View::Tree => ButtonId::Tree,
            View::Logs => ButtonId::Logs,
        };
        let mut spans = Vec::new();
        for (id, label) in TOOLBAR {
            let style = if id == active {
                Style::new().fg(Color::Black).bg(C_ACCENT).bold()
            } else {
                Style::new().fg(Color::Indexed(252)).bg(Color::Indexed(238))
            };
            spans.push(Span::styled(label, style));
            spans.push(Span::styled(" ", bg));
        }
        f.render_widget(Paragraph::new(Line::from(spans)).style(bg), area);
        return;
    }
    let hint = match app.mode {
        Mode::Input => "Enter submit · Esc cancel",
        Mode::Help => "any key to close",
        Mode::Inspect => "Esc/q back · s send line",
        Mode::Normal => "",
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(hint, Style::new().fg(C_DIM)))).style(bg),
        area,
    );
}

fn input_overlay(f: &mut Frame, area: Rect, app: &App) {
    let prompt = match app.input_kind {
        InputKind::NewAgent => "new agent",
        InputKind::Send => "send line",
        InputKind::Filter => "filter",
        InputKind::Orchestrate => "orchestrate  ·  #N msg / #* msg",
        InputKind::Goal => "goal  ·  text :: definition-of-done",
        InputKind::None => "",
    };
    let box_area = Rect {
        x: area.x.saturating_add(2),
        y: area.height.saturating_sub(5),
        width: area.width.saturating_sub(4),
        height: 3,
    };
    f.render_widget(Clear, box_area);
    let block = Block::bordered()
        .title(format!(" {prompt} "))
        .border_style(Style::new().fg(C_ACCENT));
    let inner = block.inner(box_area);
    f.render_widget(block, box_area);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw(app.input.clone()),
            Span::styled("▏", Style::new().fg(C_ACCENT)),
        ])),
        inner,
    );
}

fn help(f: &mut Frame, area: Rect) {
    let block = Block::bordered()
        .title(" agentmaster — help ")
        .border_style(Style::new().fg(C_ACCENT));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let head = |s: &'static str| Line::from(Span::styled(s, Style::new().fg(C_ACCENT).bold()));
    let lines = vec![
        head("Views"),
        Line::from("  1 list (all agents, scroll)   2 board   3 tree   4 logs    S cycle sort"),
        Line::from(""),
        head("Navigate — two views of an agent"),
        Line::from(
            "  j / k   move (scrolls)    h / l   page    ↵   inspect INSIDE (output, send a line)",
        ),
        Line::from("  f   JUMP to its real cmux/tmux tab (the live session) — the second view"),
        Line::from(
            "  🖱  click a row/card to select · click again → jump to its tab · wheel scrolls · m mouse",
        ),
        Line::from(""),
        head("Act"),
        Line::from("  n   new agent   <runtime> [task]   e.g.  'shell'   or  'claude fix the bug'"),
        Line::from(
            "  s   send a line to selected        K   kill/untrack          /   filter    H hide noise",
        ),
        Line::from("  d   discover + import tmux panes AND cmux workspaces (steer them all)"),
        Line::from("  🖱  footer is a clickable toolbar — every [button] is a click target"),
        Line::from(""),
        head("Orchestrate (one session steers all — zero token tax)"),
        Line::from("  o   talk:  #N <msg> → steer agent N   ·   #* <msg> → broadcast to all live"),
        Line::from("  g   set a goal on selected:  <goal>  ::  <definition of done>"),
        Line::from("  p   peek: read a session's last user/assistant/next off its transcript"),
        Line::from(""),
        head("Goals & indicators"),
        Line::from(
            "  🎯 progress ratchets from output milestones · DoD match → done · ⏸ NEED YOU alert",
        ),
        Line::from(""),
        head("Lanes are agent state"),
        Line::from(
            "  queued · working · blocked(needs you) · review · done — the board IS the truth",
        ),
        Line::from(""),
        head("Observability + headless orchestrator parity"),
        Line::from(
            "  every state change + action -> SQLite audit log + JSONL trace in ~/.agentmaster/",
        ),
        Line::from(
            "  read:  events · doctor · ls [--json] · goals · peek <id> · dash [--all] · find <q>",
        ),
        Line::from(
            "  act:   send <ref> <msg> · broadcast <msg> [--tmux] · goal <name> .. :: dod · batch <f>",
        ),
        Line::from(""),
        Line::from(Span::styled(
            "press any key to close",
            Style::new().fg(C_DIM),
        )),
    ];
    f.render_widget(Paragraph::new(lines), inner);
}

// ---- small helpers --------------------------------------------------------

/// One labelled, single-line detail row: `prefix text…`, clipped to the panel.
fn wrapped(prefix: &str, text: &str, w: usize, color: Color) -> Line<'static> {
    let one = text.split_whitespace().collect::<Vec<_>>().join(" ");
    Line::from(vec![
        Span::styled(format!("{prefix} "), Style::new().fg(C_FAINT)),
        Span::styled(
            trunc(&one, w.saturating_sub(prefix.chars().count() + 1)),
            Style::new().fg(color),
        ),
    ])
}

/// Word-wrap `s` into at most `max_lines` lines of `width` cols, collapsing
/// whitespace; the final line gets an ellipsis if text remains. Cheap, no deps.
fn wrap_text(s: &str, width: usize, max_lines: usize) -> Vec<String> {
    let width = width.max(8);
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut truncated = false;
    for word in s.split_whitespace() {
        let next = if cur.is_empty() {
            word.chars().count()
        } else {
            cur.chars().count() + 1 + word.chars().count()
        };
        if next > width && !cur.is_empty() {
            if lines.len() + 1 == max_lines {
                // Last allowed line is full and more text remains → truncate.
                truncated = true;
                break;
            }
            lines.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if truncated && let Some(last) = lines.last_mut() {
        *last = trunc(last, width);
        if !last.ends_with('…') {
            last.push('…');
        }
    }
    lines
}

fn trunc(s: &str, max: usize) -> String {
    let max = max.max(1);
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

fn fmt_dur(s: i64) -> String {
    if s < 0 {
        "0s".into()
    } else if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m{}s", s / 60, s % 60)
    } else {
        format!("{}h{}m", s / 3600, (s % 3600) / 60)
    }
}

fn fmt_mem(b: u64) -> String {
    if b == 0 {
        "-".into()
    } else if b < 1 << 20 {
        format!("{}K", b >> 10)
    } else if b < 1 << 30 {
        format!("{}M", b >> 20)
    } else {
        format!("{:.1}G", b as f64 / (1u64 << 30) as f64)
    }
}

fn short_ts(ts: &str) -> String {
    ts.split('T')
        .nth(1)
        .map(|t| t.split(['.', '+', 'Z']).next().unwrap_or(t).to_string())
        .unwrap_or_else(|| ts.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_title_strips_cmux_decoration() {
        assert_eq!(
            clean_title("🟣 Claude ✓ · kill dead code"),
            "kill dead code"
        );
        assert_eq!(clean_title("plain native name"), "plain native name");
    }

    #[test]
    fn wrap_text_respects_width_and_line_cap() {
        let w = wrap_text("the quick brown fox jumps over the lazy dog", 12, 2);
        assert!(w.len() <= 2);
        assert!(w.iter().all(|l| l.chars().count() <= 12));
        // text exceeds 2×12, so the final line is ellipsised
        assert!(w.last().unwrap().ends_with('…'));
        // short text fits on one line, untouched
        assert_eq!(wrap_text("all done", 40, 3), vec!["all done".to_string()]);
    }

    #[test]
    fn agent_kind_parses_from_title() {
        let mut a = Agent::new(
            1,
            "🟣 Claude ▶ · do x".into(),
            "cmux".into(),
            "cmux".into(),
            vec![],
            String::new(),
        );
        assert_eq!(agent_kind(&a), "claude");
        a.name = "🧠 Codex ✓ · y".into();
        assert_eq!(agent_kind(&a), "codex");
        a.name = "shell-3".into();
        a.runtime = "shell".into();
        assert_eq!(agent_kind(&a), "shell");
    }

    #[test]
    fn progress_bar_fills_proportionally() {
        assert_eq!(progress_bar(0), "▱▱▱▱▱▱▱▱▱▱");
        assert_eq!(progress_bar(100), "▰▰▰▰▰▰▰▰▰▰");
        assert_eq!(progress_bar(50).chars().filter(|c| *c == '▰').count(), 5);
    }

    #[test]
    fn countdown_formats_mmss_and_hms() {
        assert_eq!(fmt_countdown(0), "0:00");
        assert_eq!(fmt_countdown(65), "1:05");
        assert_eq!(fmt_countdown(3600), "1:00:00");
        assert_eq!(fmt_countdown(-5), "0:00");
    }

    #[test]
    fn cells_counts_emoji_as_two() {
        assert_eq!(cells("ab"), 2);
        assert_eq!(cells("🤖"), 2);
        assert_eq!(cells("🤖x"), 3);
    }

    #[test]
    fn pad_cells_pads_and_truncates_by_display_width() {
        assert_eq!(pad_cells("Codex", 6), "Codex ");
        assert_eq!(pad_cells("Claude", 6), "Claude");
        // emoji is 2 cells, so padding is width-aware, not char-aware
        assert_eq!(cells(&pad_cells("🤖", 4)), 4);
        // over-long is cut to fit the cell budget
        assert_eq!(cells(&pad_cells("toolongname", 4)), 4);
    }

    #[test]
    fn agent_tag_distinguishes_claude_and_codex() {
        let mut a = Agent::new(
            1,
            "🟣 Claude ▶ · x".into(),
            "cmux".into(),
            "cmux".into(),
            vec![],
            String::new(),
        );
        assert_eq!(agent_tag(&a).0, "Claude");
        a.name = "🧠 Codex ✓ · y".into();
        assert_eq!(agent_tag(&a).0, "Codex");
        // the two get different colors (that's the only identity signal now)
        let claude_col = {
            a.name = "🟣 Claude ▶ · x".into();
            agent_tag(&a).1
        };
        a.name = "🧠 Codex ✓ · y".into();
        assert_ne!(claude_col, agent_tag(&a).1);
    }

    #[test]
    fn project_icon_maps_known_and_falls_back() {
        assert_eq!(project_icon("agentmaster"), "🤖");
        assert_eq!(project_icon("~/BASE/projects/synapse"), "🔮");
        assert_eq!(project_icon("events-hub"), "🎫");
        assert_eq!(project_icon("some-random-repo"), "📁");
        assert_eq!(project_icon(""), "  ");
    }
}
