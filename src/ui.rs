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
    // strip a trailing " [..]" address suffix
    if let Some(idx) = s.rfind(" [")
        && s.ends_with(']')
    {
        return s[..idx].trim_end();
    }
    s
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
    let cols =
        Layout::horizontal([Constraint::Percentage(62), Constraint::Percentage(38)]).split(area);

    // ---- left: the agent table ----
    let title = format!(
        " agents {}  ·  sort:{} (S)  ·  / filter ",
        agents.len(),
        app.sort.label()
    );
    let lblock = Block::bordered()
        .title(title)
        .border_style(Style::new().fg(C_ACCENT));
    let linner = lblock.inner(cols[0]);
    f.render_widget(lblock, cols[0]);
    if agents.is_empty() {
        f.render_widget(
            Paragraph::new("no agents — press d to discover, n to spawn")
                .alignment(Alignment::Center)
                .style(Style::new().fg(C_FAINT)),
            linner,
        );
    } else {
        let parts = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(linner);
        // column header — aligned to the same title width the rows use.
        let tw = (parts[1].width as usize).saturating_sub(40).max(10);
        let header = format!(
            "   {:<tw$} {:<6} {:<7} {:>5} {:>6} {:>4}",
            "agent / task",
            "kind",
            "state",
            "for",
            "🧊 ttl",
            "cpu",
            tw = tw
        );
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(header, Style::new().fg(C_FAINT))))
                .style(Style::new().bg(Color::Indexed(235))),
            parts[0],
        );
        let w = parts[1].width as usize;
        let vis = parts[1].height as usize;
        // keep the selection on screen
        let scroll = if app.sel >= vis { app.sel + 1 - vis } else { 0 };
        let spin = app.spin();
        let end = (scroll + vis).min(agents.len());
        let rows: Vec<Line> = agents[scroll..end]
            .iter()
            .enumerate()
            .map(|(i, a)| agent_row(a, scroll + i == app.sel, spin, w))
            .collect();
        f.render_widget(Paragraph::new(rows), parts[1]);
        // scroll bar hint along the right edge
        if agents.len() > vis {
            let above = scroll;
            let below = agents.len() - end;
            let bar = Rect {
                x: parts[1].x,
                y: parts[1].y,
                width: parts[1].width,
                height: 1,
            };
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!("↑{above}"),
                    Style::new().fg(C_DIM),
                )))
                .alignment(Alignment::Right),
                bar,
            );
            let _ = below; // shown in the footer hint of the detail panel
        }
    }

    // ---- right: detail of the selected agent ----
    detail_panel(f, cols[1], agents.get(app.sel).copied());
}

/// One dense row in the list. Fixed-width columns then the title fills the rest.
fn agent_row(a: &Agent, selected: bool, spin: char, width: usize) -> Line<'static> {
    let col = status_color(a.status);
    let glyph = status_glyph(a.status, spin);
    let kind = agent_kind(a);
    let cache = a.cache_remaining_secs();
    let cache_txt = if matches!(a.status, Status::Working) {
        "hot".to_string()
    } else if cache == 0 {
        "cold".to_string()
    } else {
        fmt_countdown(cache)
    };
    // Title fills everything the fixed columns (kind+state+dur+cache+cpu ≈ 38) leave.
    let title_w = width.saturating_sub(40).max(10);
    let sel_bg = if selected {
        Style::new().bg(Color::Indexed(237))
    } else {
        Style::new()
    };
    let mark = if selected { "▸" } else { " " };
    let cache_col = if matches!(a.status, Status::Working) {
        C_WORKING
    } else {
        cache_color(cache)
    };
    Line::from(vec![
        Span::styled(
            format!("{mark}{glyph} "),
            Style::new().fg(col).bold().patch(sel_bg),
        ),
        Span::styled(
            format!(
                "{:<width$} ",
                trunc(clean_title(&a.name), title_w),
                width = title_w
            ),
            if selected {
                Style::new().fg(Color::White).bold().patch(sel_bg)
            } else {
                Style::new().fg(Color::Indexed(252)).patch(sel_bg)
            },
        ),
        Span::styled(
            format!("{:<6} ", trunc(kind, 6)),
            Style::new().fg(C_ACCENT).patch(sel_bg),
        ),
        Span::styled(
            format!("{:<7} ", a.status.label()),
            Style::new().fg(col).patch(sel_bg),
        ),
        Span::styled(
            format!("{:>5} ", fmt_dur(a.in_status_secs())),
            Style::new().fg(C_DIM).patch(sel_bg),
        ),
        Span::styled(
            format!("🧊{cache_txt:>5} "),
            Style::new().fg(cache_col).patch(sel_bg),
        ),
        Span::styled(
            format!("{:>3.0}%", a.cpu),
            Style::new().fg(C_FAINT).patch(sel_bg),
        ),
    ])
}

/// Right pane: everything about the selected agent + the actions you can take.
fn detail_panel(f: &mut Frame, area: Rect, a: Option<&Agent>) {
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
            trunc(clean_title(&a.name), w),
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
    ];
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
                trunc(clean_title(&a.name), w.saturating_sub(kind.len() + 5)),
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
    let start = a.output.len().saturating_sub(h);
    let lines: Vec<Line> = a
        .output
        .iter()
        .skip(start)
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
        Line::from("  s   send a line to selected        K   kill/untrack          /   filter"),
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
}
