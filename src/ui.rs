//! Rendering. Pure function of `App`: no side effects, no mutation. TUI best
//! practices baked in — consistent color semantics, one focus highlight, clear
//! empty states, contextual footer, truncation that never overflows, a help
//! overlay, and a responsive layout that survives resize.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

use crate::app::{App, InputKind, Mode, View};
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
    let rows = Layout::vertical([
        Constraint::Length(4),
        Constraint::Min(3),
        Constraint::Length(1),
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

    footer(f, rows[2], app);

    if app.mode == Mode::Input {
        input_overlay(f, area, app);
    }
}

// ---- header ---------------------------------------------------------------

fn header(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered().border_style(Style::new().fg(C_ACCENT));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let rows = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(inner);

    let c = app.fleet.counts();
    let total: usize = c.iter().sum();
    let l0 = Line::from(vec![
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
    ]);
    f.render_widget(Paragraph::new(l0), rows[0]);

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
    let card_h: u16 = 6;
    let max = (inner.height / card_h).max(1) as usize;
    let constraints: Vec<Constraint> = (0..max).map(|_| Constraint::Length(card_h)).collect();
    let slots = Layout::vertical(constraints).split(inner);
    for (idx, agent) in agents.iter().take(max).enumerate() {
        let selected = focused && idx == app.card_idx;
        card(f, slots[idx], agent, selected, &app.filter);
    }
    if agents.len() > max {
        // Honesty over silent truncation: say how many cards are hidden.
        let last = slots[max.saturating_sub(1)];
        let note = Rect {
            x: last.x,
            y: last.y.saturating_add(card_h.saturating_sub(1)),
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

fn card(f: &mut Frame, area: Rect, a: &Agent, selected: bool, filter: &str) {
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
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(bstyle);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let w = inner.width as usize;
    let name_style = if dim {
        Style::new().fg(C_IDLE)
    } else {
        Style::new().bold()
    };
    let lines = vec![
        Line::from(vec![
            Span::styled("● ", Style::new().fg(col)),
            Span::styled(trunc(&a.name, w.saturating_sub(10)), name_style),
            Span::raw(" "),
            Span::styled(a.runtime.clone(), Style::new().fg(C_ACCENT)),
        ]),
        Line::from(vec![
            Span::styled("⎇ ", Style::new().fg(C_DIM)),
            Span::styled(
                trunc(&a.project, w.saturating_sub(14)),
                Style::new().fg(C_DIM),
            ),
            Span::styled(
                format!(
                    "  pid {}",
                    a.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into())
                ),
                Style::new().fg(C_FAINT),
            ),
        ]),
        Line::from(vec![
            Span::styled(
                format!("⏱ {} ", fmt_dur(a.age_secs())),
                Style::new().fg(C_DIM),
            ),
            Span::styled(
                format!("idle {} ", fmt_dur(a.idle_secs())),
                Style::new().fg(if a.idle_secs() > 20 { C_IDLE } else { C_DIM }),
            ),
            Span::styled(a.status.label(), Style::new().fg(col)),
        ]),
        Line::from(vec![
            Span::styled("▸ ", Style::new().fg(col)),
            Span::styled(
                trunc(a.last_line.trim(), w.saturating_sub(2)),
                Style::new().fg(Color::Indexed(250)),
            ),
        ]),
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
        " inspect: {} [{}] pid {} — Esc back · s send line ",
        a.name,
        a.status.label(),
        a.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into())
    );
    let block = Block::bordered()
        .title(title)
        .border_style(Style::new().fg(status_color(a.status)));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let parts = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(inner);

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

    let h = parts[1].height as usize;
    let start = a.output.len().saturating_sub(h);
    let lines: Vec<Line> = a
        .output
        .iter()
        .skip(start)
        .map(|l| Line::from(trunc(l, parts[1].width as usize)))
        .collect();
    let body = if lines.is_empty() {
        vec![Line::from(Span::styled(
            "(no output yet)",
            Style::new().fg(C_FAINT),
        ))]
    } else {
        lines
    };
    f.render_widget(Paragraph::new(body), parts[1]);
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
        .title(" event log — every state change & action (SQLite + JSONL) ")
        .border_style(Style::new().fg(C_ACCENT));
    let inner = block.inner(area);
    f.render_widget(block, area);

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
    let hint = match app.mode {
        Mode::Input => "Enter submit · Esc cancel",
        Mode::Help => "any key to close",
        Mode::Inspect => "Esc/q back · s send line",
        Mode::Normal => {
            "[1]kanban [2]tree [3]logs   lane:h/l/Tab  card:j/k  ↵inspect   n)ew s)end K)ill /filter ?help q)uit"
        }
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(hint, Style::new().fg(C_DIM))))
            .style(Style::new().bg(Color::Indexed(235))),
        area,
    );
}

fn input_overlay(f: &mut Frame, area: Rect, app: &App) {
    let prompt = match app.input_kind {
        InputKind::NewAgent => "new agent",
        InputKind::Send => "send line",
        InputKind::Filter => "filter",
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
        Line::from(""),
        head("Act"),
        Line::from("  n   new agent   <runtime> [task]   e.g.  'shell'   or  'claude fix the bug'"),
        Line::from("  s   send a line to selected        K   kill selected         /   filter"),
        Line::from(""),
        head("Lanes are agent state"),
        Line::from(
            "  queued · working · blocked(needs you) · review · done — the board IS the truth",
        ),
        Line::from(""),
        head("Observability"),
        Line::from(
            "  every state change + action -> SQLite audit log + JSONL trace in ~/.agentmaster/",
        ),
        Line::from("  headless:  agentmaster events -n 100   ·   agentmaster doctor"),
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

fn short_ts(ts: &str) -> String {
    ts.split('T')
        .nth(1)
        .map(|t| t.split(['.', '+', 'Z']).next().unwrap_or(t).to_string())
        .unwrap_or_else(|| ts.to_string())
}
