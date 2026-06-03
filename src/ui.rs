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
use crate::fleet::{Agent, Lane, Status};

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

/// Card status row: age · idle (highlighted once stale) · status label.
fn status_line(a: &Agent, col: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("⏱ {} ", fmt_dur(a.age_secs())),
            Style::new().fg(C_DIM),
        ),
        Span::styled(
            format!("idle {} ", fmt_dur(a.idle_secs())),
            Style::new().fg(if a.idle_secs() > 20 { C_IDLE } else { C_DIM }),
        ),
        Span::styled(a.status.label().to_string(), Style::new().fg(col)),
    ])
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
    // right now. Make it loud (and blink on odd ticks) so it can't be missed.
    if c[2] > 0 {
        let style = Style::new().fg(Color::Black).bg(C_BLOCKED).bold();
        spans.push(Span::raw("   "));
        spans.push(Span::styled(
            format!(" ⏸ {} NEED YOU ", c[2]),
            if app.tick().is_multiple_of(2) {
                style
            } else {
                Style::new().fg(C_BLOCKED).bold()
            },
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), rows[0]);

    let view_name = match app.view {
        View::Kanban => "kanban",
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

// ---- kanban ---------------------------------------------------------------

fn kanban(f: &mut Frame, area: Rect, app: &App) {
    let cols = Layout::horizontal([Constraint::Ratio(1, 5); 5]).split(area);
    for (i, lane) in Lane::ALL.iter().enumerate() {
        let agents = app.fleet.in_lane(*lane);
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
    let max = (inner.height / CARD_H).max(1) as usize;
    let constraints: Vec<Constraint> = (0..max).map(|_| Constraint::Length(CARD_H)).collect();
    let slots = Layout::vertical(constraints).split(inner);
    let spin = app.spin();
    for (idx, agent) in agents.iter().take(max).enumerate() {
        let selected = focused && idx == app.card_idx;
        card(f, slots[idx], agent, selected, &app.filter, spin);
    }
    if agents.len() > max {
        // Honesty over silent truncation: say how many cards are hidden.
        let last = slots[max.saturating_sub(1)];
        let note = Rect {
            x: last.x,
            y: last.y.saturating_add(CARD_H.saturating_sub(1)),
            width: last.width,
            height: 1,
        };
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("  +{} more…", agents.len() - max),
                Style::new().fg(C_DIM),
            ))),
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

    let bstyle = if selected {
        Style::new().fg(C_ACCENT).bold()
    } else if !matched {
        Style::new().fg(C_FAINT)
    } else {
        Style::new().fg(Color::Indexed(239))
    };
    let mut block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(bstyle);
    // Selected card gets a subtle background — focus feedback, not color-only.
    if selected {
        block = block.style(Style::new().bg(Color::Indexed(236)));
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
    let lines = vec![
        Line::from(vec![
            Span::styled(format!("{glyph} "), Style::new().fg(col).bold()),
            Span::styled(trunc(&a.name, w.saturating_sub(12)), name_style),
            Span::raw(" "),
            Span::styled(trunc(&a.runtime, 8), Style::new().fg(C_ACCENT)),
            Span::styled(
                if a.source.tag().is_empty() {
                    String::new()
                } else {
                    format!(" ·{}", a.source.tag())
                },
                Style::new().fg(C_REVIEW),
            ),
        ]),
        Line::from(vec![
            Span::styled("⎇ ", Style::new().fg(C_DIM)),
            Span::styled(
                trunc(&a.project, w.saturating_sub(14)),
                Style::new().fg(C_DIM),
            ),
            Span::styled(
                format!(
                    "  pid {} · {}",
                    a.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into()),
                    fmt_mem(a.mem_bytes)
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
        " inspect: {} [{}] pid {} — Esc back · s send · g goal · p peek ",
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
        Line::from("  1 kanban    2 tree    3 logs"),
        Line::from(""),
        head("Navigate (kanban)"),
        Line::from("  h / l / Tab   switch lane        j / k   select card        ↵   inspect"),
        Line::from(
            "  🖱  click a card to select · click again to open · wheel scrolls · m toggles mouse",
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
