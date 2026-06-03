//! Application state + the single-threaded event loop. Render is a pure function
//! of `App` (see `ui`); all mutation happens here. PTY reader threads feed events
//! in over a channel; crossterm key events drive the state machine.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use anyhow::Result;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::Rect;

use crate::fleet::{Agent, Fleet, Lane, Source, Status};
use crate::obs::Metrics;
use crate::store::Store;
use crate::{backend, peek, pty, runtime, state};

/// Shared layout geometry — single source of truth for both render (`ui.rs`) and
/// mouse hit-testing here, so clicks always land where the cards are drawn.
pub const HEADER_H: u16 = 4;
pub const FOOTER_H: u16 = 1;
pub const CARD_H: u16 = 6;

/// Clickable toolbar buttons (the footer in Normal mode). One source of truth so
/// the rendered labels and the click hit-test never drift. Labels are ASCII so
/// display width == char count == hit-test width.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ButtonId {
    Kanban,
    Tree,
    Logs,
    Orchestrate,
    New,
    Discover,
    Mouse,
    Help,
    Quit,
}

pub const TOOLBAR: [(ButtonId, &str); 9] = [
    (ButtonId::Kanban, "[1 kanban]"),
    (ButtonId::Tree, "[2 tree]"),
    (ButtonId::Logs, "[3 logs]"),
    (ButtonId::Orchestrate, "[o talk]"),
    (ButtonId::New, "[+ new]"),
    (ButtonId::Discover, "[* find]"),
    (ButtonId::Mouse, "[m mouse]"),
    (ButtonId::Help, "[? help]"),
    (ButtonId::Quit, "[q quit]"),
];

/// Which toolbar button is at column `col` (labels joined by one space).
pub fn toolbar_hit(col: u16) -> Option<ButtonId> {
    let mut x = 0u16;
    for (id, label) in TOOLBAR {
        let w = label.chars().count() as u16;
        if col >= x && col < x + w {
            return Some(id);
        }
        x += w + 1; // single-space separator between buttons
    }
    None
}

/// Events produced by PTY reader threads.
pub enum AppEvent {
    Output { id: u64, line: String },
    Exited { id: u64 },
}

#[derive(Clone, Copy, PartialEq)]
pub enum View {
    Kanban,
    Tree,
    Logs,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    Normal,
    Inspect,
    Input,
    Help,
}

#[derive(Clone, Copy, PartialEq)]
pub enum InputKind {
    None,
    NewAgent,
    Send,
    Filter,
    Orchestrate,
    Goal,
}

pub struct App {
    pub fleet: Fleet,
    pub store: Store,
    pub metrics: Metrics,
    pub tx: Sender<AppEvent>,
    rx: Receiver<AppEvent>,
    pub view: View,
    pub mode: Mode,
    pub lane_idx: usize,
    pub card_idx: usize,
    pub input: String,
    pub input_kind: InputKind,
    pub filter: String,
    pub status_msg: String,
    /// Orchestrator transcript (top strip) — what you told which agent.
    pub chat_log: Vec<String>,
    pub should_quit: bool,
    pub mouse_on: bool,
    /// Last known terminal rect, refreshed each frame for mouse hit-testing.
    area: Rect,
    tick: u64,
}

/// Entry point for the `tui` subcommand.
pub fn run(dir: PathBuf) -> Result<()> {
    let store = Store::open(&dir.join("agentmaster.db"))?;
    let (tx, rx) = channel();
    let mut app = App::new(store, tx, rx);
    app.store
        .log(None, "system", "start", "agentmaster tui started");
    tracing::info!("agentmaster tui started");

    let mut terminal = ratatui::init();
    let _ = execute!(std::io::stdout(), EnableMouseCapture);
    let res = app.main_loop(&mut terminal);
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    app.store
        .log(None, "system", "stop", "agentmaster tui stopped");
    res
}

impl App {
    fn new(store: Store, tx: Sender<AppEvent>, rx: Receiver<AppEvent>) -> Self {
        App {
            fleet: Fleet::new(),
            store,
            metrics: Metrics::new(),
            tx,
            rx,
            view: View::Kanban,
            mode: Mode::Normal,
            lane_idx: 1, // focus WORKING by default
            card_idx: 0,
            input: String::new(),
            input_kind: InputKind::None,
            filter: String::new(),
            chat_log: vec![
                "orchestrator ready — press [* find] to import tmux+cmux agents,".into(),
                "then [o talk] and type  #N <message>  to steer agent #N.".into(),
            ],
            status_msg: "ready — o talk · * find agents · n new · ? help".into(),
            should_quit: false,
            mouse_on: true,
            area: Rect::new(0, 0, 0, 0),
            tick: 0,
        }
    }

    /// Current housekeeping tick — used by the renderer for blink/animation.
    pub fn tick(&self) -> u64 {
        self.tick
    }

    /// Braille spinner frame — motion that means "this agent is actively working".
    pub fn spin(&self) -> char {
        const F: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
        F[(self.tick / 2) as usize % F.len()]
    }

    fn main_loop(&mut self, terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
        loop {
            while let Ok(ev) = self.rx.try_recv() {
                self.handle_pty(ev);
            }
            self.tick = self.tick.wrapping_add(1);
            if self.tick.is_multiple_of(10) {
                self.housekeep();
            }
            let sz = terminal.size()?;
            self.area = Rect::new(0, 0, sz.width, sz.height);
            terminal.draw(|f| crate::ui::render(f, self))?;
            if event::poll(Duration::from_millis(50))? {
                match event::read()? {
                    Event::Key(k) => self.handle_key(k),
                    Event::Mouse(m) => self.handle_mouse(m),
                    _ => {}
                }
            }
            if self.should_quit {
                break;
            }
        }
        Ok(())
    }

    // ---- periodic upkeep --------------------------------------------------

    fn housekeep(&mut self) {
        self.metrics.refresh();
        // Targeted per-process resource refresh, snapshot-then-apply to keep the
        // metrics borrow disjoint from the mutable fleet iteration.
        let pids: Vec<u32> = self.fleet.agents.iter().filter_map(|a| a.pid).collect();
        self.metrics.refresh_procs(&pids);
        let stats: Vec<(u32, f32, u64)> = pids
            .iter()
            .filter_map(|&p| self.metrics.proc_stats(p).map(|(c, m)| (p, c, m)))
            .collect();
        // One cmux query per tick if we track any cmux agents; map by ws_ref below.
        let has_cmux = self
            .fleet
            .agents
            .iter()
            .any(|a| matches!(a.source, Source::Cmux(_)));
        let cmux_snapshot = if has_cmux {
            backend::list_cmux()
        } else {
            Vec::new()
        };
        for a in self.fleet.agents.iter_mut() {
            if let Some(pid) = a.pid
                && let Some(&(_, cpu, mem)) = stats.iter().find(|(p, _, _)| *p == pid)
            {
                a.cpu = cpu;
                a.mem_bytes = mem;
            }
            // tmux-backed agents: snapshot their pane and derive state from it.
            if let Source::Tmux(target) = a.source.clone() {
                match backend::capture(&target) {
                    Some(screen) => {
                        if let Some(last) = screen.lines().rev().find(|l| !l.trim().is_empty()) {
                            let line = last.trim().to_string();
                            if line != a.last_line {
                                if let Some(ns) = state::detect(a, &line) {
                                    a.status = ns;
                                }
                                a.push_line(line);
                            }
                        }
                    }
                    None => a.status = Status::Dead, // pane closed
                }
            }
            // cmux-backed agents: refresh status + title from `cmux top --all`.
            if let Source::Cmux(ws) = a.source.clone()
                && let Some(w) = cmux_snapshot.iter().find(|w| w.ws_ref == ws)
            {
                a.status = cmux_status(&w.status);
                if !w.title.is_empty() && w.title != a.last_line {
                    a.push_line(w.title.clone());
                }
                if a.pid.is_none() {
                    a.pid = w.pid;
                }
            }
            if matches!(a.source, Source::Native) {
                state::idle_sweep(a, 20);
            }
            if let Some(h) = a.pty.as_mut() {
                if a.pid.is_none() {
                    a.pid = h.child.process_id();
                }
                if let Ok(Some(_)) = h.child.try_wait()
                    && !matches!(a.status, Status::Done)
                {
                    a.status = Status::Dead;
                }
            }
        }
    }

    /// Import every tmux pane not already tracked, as a `Source::Tmux` agent.
    fn discover_tmux(&mut self) {
        if !backend::tmux_available() {
            self.status_msg = "tmux not found".into();
            return;
        }
        let mut added = 0;
        for p in backend::list_panes() {
            // Don't import our own pane, and don't double-import.
            if p.command.contains("agentmaster") {
                continue;
            }
            if self
                .fleet
                .agents
                .iter()
                .any(|a| a.source == Source::Tmux(p.target.clone()))
            {
                continue;
            }
            let id = self.fleet.next_id;
            self.fleet.next_id += 1;
            let name = if p.title.is_empty() || p.title == p.command {
                p.target.clone()
            } else {
                format!("{} [{}]", p.title, p.target)
            };
            let mut a = Agent::new(
                id,
                name.clone(),
                p.command.clone(),
                p.command.clone(),
                vec![],
                p.path.clone(),
            );
            a.source = Source::Tmux(p.target.clone());
            a.pid = p.pid;
            a.status = Status::Idle;
            self.fleet.agents.push(a);
            self.rehydrate_goal(id, &name);
            self.store
                .log(Some(id), &name, "import", &format!("tmux {}", p.target));
            tracing::info!(agent = id, target = %p.target, "imported tmux pane");
            added += 1;
        }
        self.status_msg = format!("discovered {added} new tmux pane(s)");
    }

    /// Import every cmux workspace not already tracked, as a `Source::Cmux` agent.
    /// Status is mapped from the workspace's agent tag (`cmux top --all`).
    fn discover_cmux(&mut self) {
        if !backend::cmux_available() {
            return;
        }
        let mut added = 0;
        for w in backend::list_cmux() {
            if w.ws_ref.is_empty() {
                continue;
            }
            if self
                .fleet
                .agents
                .iter()
                .any(|a| a.source == Source::Cmux(w.ws_ref.clone()))
            {
                continue;
            }
            let id = self.fleet.next_id;
            self.fleet.next_id += 1;
            let name = if w.title.is_empty() {
                w.ws_ref.clone()
            } else {
                w.title.clone()
            };
            let mut a = Agent::new(
                id,
                name.clone(),
                "cmux".into(),
                "cmux".into(),
                vec![],
                String::new(),
            );
            a.source = Source::Cmux(w.ws_ref.clone());
            a.pid = w.pid;
            a.status = cmux_status(&w.status);
            a.push_line(w.title.clone());
            self.fleet.agents.push(a);
            self.rehydrate_goal(id, &name);
            self.store
                .log(Some(id), &name, "import", &format!("cmux {}", w.ws_ref));
            tracing::info!(agent = id, ws = %w.ws_ref, "imported cmux workspace");
            added += 1;
        }
        self.status_msg = format!("{} +{added} cmux workspace(s)", self.status_msg);
    }

    /// One button to pull in agents from every backend (tmux panes + cmux
    /// workspaces). The `[* find]` action and the `d` key both land here.
    fn discover_all(&mut self) {
        self.status_msg = "discovered".into();
        self.discover_tmux();
        self.discover_cmux();
    }

    /// Read the selected agent's transcript off disk and surface its last
    /// user/assistant/next-action into the orchestrator chat — the zero-tax peek.
    fn peek_selected(&mut self) {
        let Some(id) = self.current_agent_id() else {
            return;
        };
        let (name, transcript) = match self.fleet.get(id) {
            Some(a) => (a.name.clone(), a.transcript.clone()),
            None => return,
        };
        let Some(path) = transcript else {
            self.status_msg = format!("{name}: no transcript to peek (native/tmux agent)");
            return;
        };
        let d = peek::digest(std::path::Path::new(&path), 220);
        if !d.last_user.is_empty() {
            self.chat_log
                .push(format!("peek {name} 🧑 {}", d.last_user));
        }
        if !d.next_action.is_empty() {
            self.chat_log
                .push(format!("peek {name} 🎯 {}", d.next_action));
        }
        self.status_msg = format!("peeked {name}");
    }

    // ---- pty events -------------------------------------------------------

    fn handle_pty(&mut self, ev: AppEvent) {
        match ev {
            AppEvent::Output { id, line } => {
                let mut transition = None;
                let mut progressed = None;
                if let Some(a) = self.fleet.get_mut(id) {
                    if let Some(ns) = state::detect(a, &line)
                        && ns != a.status
                    {
                        transition = Some((a.name.clone(), ns));
                        a.status = ns;
                    }
                    // Goal-aware: ratchet progress from milestones in the output.
                    if a.has_goal() {
                        let np = state::infer_progress(a.progress, &line);
                        if np != a.progress {
                            a.progress = np;
                            progressed = Some((a.name.clone(), np));
                        }
                    }
                    a.push_line(line);
                }
                if let Some((name, ns)) = transition {
                    self.store.log(Some(id), &name, "state", ns.label());
                    tracing::info!(agent = id, status = ns.label(), "state change");
                }
                if let Some((name, np)) = progressed {
                    self.store.save_progress(&name, np);
                }
            }
            AppEvent::Exited { id } => {
                if let Some(a) = self.fleet.get_mut(id) {
                    if !matches!(a.status, Status::Done) {
                        a.status = Status::Dead;
                    }
                    let name = a.name.clone();
                    self.store.log(Some(id), &name, "exit", "process exited");
                    tracing::info!(agent = id, "process exited");
                }
            }
        }
    }

    // ---- actions ----------------------------------------------------------

    fn spawn_agent(&mut self, name: String, runtime_name: String, task: Option<String>) {
        let spec = runtime::resolve(&runtime_name, task.as_deref());
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "/".into());
        let id = self.fleet.next_id;
        self.fleet.next_id += 1;
        let mut agent = Agent::new(
            id,
            name.clone(),
            runtime_name.clone(),
            spec.program.clone(),
            spec.args.clone(),
            cwd.clone(),
        );
        match pty::spawn(id, &spec.program, &spec.args, &cwd, &[], self.tx.clone()) {
            Ok(h) => {
                agent.pid = h.child.process_id();
                agent.status = Status::Working;
                agent.pty = Some(h);
                self.store.log(
                    Some(id),
                    &name,
                    "spawn",
                    &format!("{} {}", spec.program, spec.args.join(" ")),
                );
                tracing::info!(agent = id, runtime = %runtime_name, "spawned");
                self.status_msg = format!("spawned {name} ({runtime_name})");
            }
            Err(e) => {
                agent.status = Status::Dead;
                self.store.log(Some(id), &name, "error", &e.to_string());
                tracing::error!(agent = id, error = %e, "spawn failed");
                self.status_msg = format!("spawn failed: {e}");
            }
        }
        self.fleet.agents.push(agent);
        self.rehydrate_goal(id, &name);
    }

    fn send_line(&mut self, id: u64, text: &str) {
        use std::io::Write;
        let mut ok = false;
        let mut name = String::new();
        if let Some(a) = self.fleet.get_mut(id) {
            name = a.name.clone();
            match &a.source {
                Source::Native => {
                    if let Some(h) = a.pty.as_mut() {
                        let _ = h.writer.write_all(text.as_bytes());
                        let _ = h.writer.write_all(b"\n");
                        let _ = h.writer.flush();
                        ok = true;
                    }
                }
                Source::Tmux(target) => {
                    backend::send_keys(target, text);
                    ok = true;
                }
                Source::Cmux(ws_ref) => {
                    backend::cmux_send(ws_ref, text);
                    ok = true;
                }
            }
            if ok && matches!(a.status, Status::Blocked | Status::Idle) {
                a.status = Status::Working;
            }
        }
        if ok {
            self.store.log(Some(id), &name, "send", text);
            tracing::info!(agent = id, "sent line");
            self.status_msg = format!("sent to {name}");
        }
    }

    fn kill(&mut self, id: u64) {
        let mut name = String::new();
        if let Some(a) = self.fleet.get_mut(id) {
            name = a.name.clone();
            match &a.source {
                Source::Native => {
                    if let Some(h) = a.pty.as_mut() {
                        let _ = h.child.kill();
                    }
                }
                Source::Tmux(target) => backend::kill_pane(target),
                // Never destroy an external cmux workspace from here — just stop
                // tracking it. Killing someone else's session is not our call.
                Source::Cmux(_) => {}
            }
            a.status = Status::Dead;
        }
        self.store
            .log(Some(id), &name, "kill", "untracked / killed by user");
        tracing::warn!(agent = id, "killed by user");
        self.status_msg = format!("dropped {name}");
    }

    // ---- selection helpers ------------------------------------------------

    pub fn current_lane(&self) -> Lane {
        Lane::ALL[self.lane_idx]
    }

    pub fn current_agent_id(&self) -> Option<u64> {
        self.fleet
            .in_lane(self.current_lane())
            .get(self.card_idx)
            .map(|a| a.id)
    }

    // ---- input ------------------------------------------------------------

    fn start_input(&mut self, kind: InputKind) {
        self.input.clear();
        self.input_kind = kind;
        self.mode = Mode::Input;
        self.status_msg = match kind {
            InputKind::NewAgent => {
                "new agent: <runtime> [task]  — e.g. 'shell'  or  'claude fix the bug'".into()
            }
            InputKind::Send => "send a line to the selected agent".into(),
            InputKind::Filter => "filter (substring of name / last line)".into(),
            InputKind::Orchestrate => {
                "orchestrate: #N <msg> → steer agent N · #* <msg> → broadcast to all live".into()
            }
            InputKind::Goal => {
                "goal for selected agent:  <goal text>  ::  <definition of done>".into()
            }
            InputKind::None => String::new(),
        };
    }

    fn submit_input(&mut self, buf: String) {
        match self.input_kind {
            InputKind::NewAgent => {
                let buf = buf.trim().to_string();
                if buf.is_empty() {
                    return;
                }
                let mut parts = buf.splitn(2, ' ');
                let runtime = parts.next().unwrap_or("shell").to_string();
                let task = parts.next().map(|s| s.to_string());
                let name = format!("{}-{}", runtime, self.fleet.next_id);
                self.spawn_agent(name, runtime, task);
            }
            InputKind::Send => {
                if let Some(id) = self.current_agent_id() {
                    self.send_line(id, &buf);
                }
            }
            InputKind::Filter => self.filter = buf.trim().to_string(),
            InputKind::Orchestrate => self.orchestrate(&buf),
            InputKind::Goal => self.set_goal_on_selected(&buf),
            InputKind::None => {}
        }
    }

    /// Orchestrator send from the one master session: `#N <msg>` steers agent N,
    /// `#* <msg>` (or `#all <msg>`) broadcasts to every live (non-terminal) agent.
    /// This is the in-TUI port of `cmux-meta-orchestrator send` — zero token tax,
    /// the routing is just a line write to each agent's transport.
    fn orchestrate(&mut self, buf: &str) {
        let buf = buf.trim();
        let Some(rest) = buf.strip_prefix('#') else {
            self.status_msg = "orchestrate: start with #N or #*  (e.g. #3 run the tests)".into();
            return;
        };
        let mut parts = rest.splitn(2, char::is_whitespace);
        let sel = parts.next().unwrap_or("").trim();
        let msg = parts.next().unwrap_or("").trim();
        if msg.is_empty() {
            self.status_msg = "orchestrate: empty message".into();
            return;
        }
        if sel == "*" || sel.eq_ignore_ascii_case("all") {
            let ids: Vec<u64> = self
                .fleet
                .agents
                .iter()
                .filter(|a| !a.is_terminal())
                .map(|a| a.id)
                .collect();
            let n = ids.len();
            for id in ids {
                self.send_line(id, msg);
            }
            self.chat_log.push(format!("#* ({n} agents) ◂ {msg}"));
            self.store.log(
                None,
                "orchestrator",
                "broadcast",
                &format!("{n} agents: {msg}"),
            );
            self.status_msg = format!("broadcast to {n} live agent(s)");
        } else if let Ok(id) = sel.parse::<u64>() {
            if let Some(name) = self.fleet.get(id).map(|a| a.name.clone()) {
                self.send_line(id, msg);
                self.chat_log.push(format!("#{id} {name} ◂ {msg}"));
            } else {
                self.status_msg = format!("no agent #{id}");
            }
        } else {
            self.status_msg = format!("orchestrate: '{sel}' is not an agent id or *");
        }
        while self.chat_log.len() > 200 {
            self.chat_log.remove(0);
        }
    }

    /// Pin a goal (+ optional definition-of-done after `::`) on the selected agent
    /// and persist it. Progress then ratchets up from the agent's own output and a
    /// DoD match flips it to DONE — all observed, never asked for.
    fn set_goal_on_selected(&mut self, buf: &str) {
        let Some(id) = self.current_agent_id() else {
            self.status_msg = "no agent selected for goal".into();
            return;
        };
        let (goal, dod) = match buf.split_once("::") {
            Some((g, d)) => (g.trim().to_string(), {
                let d = d.trim();
                if d.is_empty() {
                    None
                } else {
                    Some(d.to_string())
                }
            }),
            None => (buf.trim().to_string(), None),
        };
        if goal.is_empty() {
            self.status_msg = "goal cleared (empty)".into();
            return;
        }
        let name = self
            .fleet
            .get(id)
            .map(|a| a.name.clone())
            .unwrap_or_default();
        self.store.set_goal(&name, &goal, dod.as_deref());
        if let Some(a) = self.fleet.get_mut(id) {
            a.goal = Some(goal.clone());
            a.done_def = dod.clone();
            a.progress = 0;
        }
        self.store.log(Some(id), &name, "goal", &goal);
        tracing::info!(agent = id, goal = %goal, "goal set");
        self.status_msg = format!("🎯 goal set on {name}");
    }

    /// Apply any stored goal for `name` to a freshly spawned/imported agent, so a
    /// goal set in a previous run is still tracked today.
    fn rehydrate_goal(&mut self, id: u64, name: &str) {
        let found = self
            .store
            .load_goals()
            .into_iter()
            .find(|(n, ..)| n == name);
        if let Some((_, goal, dod, progress)) = found
            && let Some(a) = self.fleet.get_mut(id)
        {
            a.goal = Some(goal);
            a.done_def = dod;
            a.progress = progress;
        }
    }

    // ---- key handling -----------------------------------------------------

    fn handle_key(&mut self, k: KeyEvent) {
        if k.kind != KeyEventKind::Press {
            return;
        }
        match self.mode {
            Mode::Input => match k.code {
                KeyCode::Enter => {
                    let buf = std::mem::take(&mut self.input);
                    self.submit_input(buf);
                    self.mode = Mode::Normal;
                    self.input_kind = InputKind::None;
                }
                KeyCode::Esc => {
                    self.input.clear();
                    self.mode = Mode::Normal;
                    self.input_kind = InputKind::None;
                }
                KeyCode::Backspace => {
                    self.input.pop();
                }
                KeyCode::Char(c) => self.input.push(c),
                _ => {}
            },
            Mode::Help => self.mode = Mode::Normal,
            Mode::Inspect => match k.code {
                KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Normal,
                KeyCode::Char('i') | KeyCode::Char('s') => self.start_input(InputKind::Send),
                KeyCode::Char('g') => self.start_input(InputKind::Goal),
                KeyCode::Char('p') => self.peek_selected(),
                _ => {}
            },
            Mode::Normal => match k.code {
                KeyCode::Char('q') => self.should_quit = true,
                KeyCode::Char('1') => self.view = View::Kanban,
                KeyCode::Char('2') => self.view = View::Tree,
                KeyCode::Char('3') => self.view = View::Logs,
                KeyCode::Char('?') => self.mode = Mode::Help,
                KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
                    self.lane_idx = (self.lane_idx + 1) % 5;
                    self.card_idx = 0;
                }
                KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => {
                    self.lane_idx = (self.lane_idx + 4) % 5;
                    self.card_idx = 0;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    let n = self.fleet.in_lane(self.current_lane()).len();
                    if n > 0 {
                        self.card_idx = (self.card_idx + 1) % n;
                    }
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    let n = self.fleet.in_lane(self.current_lane()).len();
                    if n > 0 {
                        self.card_idx = (self.card_idx + n - 1) % n;
                    }
                }
                KeyCode::Enter if self.current_agent_id().is_some() => {
                    self.mode = Mode::Inspect;
                }
                KeyCode::Char('n') => self.start_input(InputKind::NewAgent),
                KeyCode::Char('o') => self.start_input(InputKind::Orchestrate),
                KeyCode::Char('g') if self.current_agent_id().is_some() => {
                    self.start_input(InputKind::Goal);
                }
                KeyCode::Char('p') if self.current_agent_id().is_some() => self.peek_selected(),
                KeyCode::Char('s') if self.current_agent_id().is_some() => {
                    self.start_input(InputKind::Send);
                }
                KeyCode::Char('/') => self.start_input(InputKind::Filter),
                KeyCode::Char('K') => {
                    if let Some(id) = self.current_agent_id() {
                        self.kill(id);
                    }
                }
                KeyCode::Char('m') => self.toggle_mouse(),
                KeyCode::Char('d') => self.discover_all(),
                _ => {}
            },
        }
        self.clamp_card();
    }

    fn dispatch_button(&mut self, b: ButtonId) {
        match b {
            ButtonId::Kanban => self.view = View::Kanban,
            ButtonId::Tree => self.view = View::Tree,
            ButtonId::Logs => self.view = View::Logs,
            ButtonId::Orchestrate => self.start_input(InputKind::Orchestrate),
            ButtonId::New => self.start_input(InputKind::NewAgent),
            ButtonId::Discover => self.discover_all(),
            ButtonId::Mouse => self.toggle_mouse(),
            ButtonId::Help => self.mode = Mode::Help,
            ButtonId::Quit => self.should_quit = true,
        }
    }

    fn clamp_card(&mut self) {
        let n = self.fleet.in_lane(self.current_lane()).len();
        if n == 0 {
            self.card_idx = 0;
        } else if self.card_idx >= n {
            self.card_idx = n - 1;
        }
    }

    /// Toggle mouse capture. Off hands the mouse back to the terminal for native
    /// text selection / copy (respecting the "selection wins by default" pref).
    fn toggle_mouse(&mut self) {
        self.mouse_on = !self.mouse_on;
        let _ = if self.mouse_on {
            execute!(std::io::stdout(), EnableMouseCapture)
        } else {
            execute!(std::io::stdout(), DisableMouseCapture)
        };
        self.status_msg = if self.mouse_on {
            "mouse ON — click lanes/cards, scroll to navigate, click again to inspect".into()
        } else {
            "mouse OFF — native text selection enabled (press m to re-enable)".into()
        };
    }

    // ---- mouse handling ---------------------------------------------------

    fn handle_mouse(&mut self, m: MouseEvent) {
        if self.mode != Mode::Normal {
            return;
        }
        // Footer toolbar is clickable from any view (e.g. click [1 kanban] to
        // return). Handle it before the board, which is Kanban-only.
        if let MouseEventKind::Down(MouseButton::Left) = m.kind {
            let footer_row = self.area.height.saturating_sub(FOOTER_H);
            if m.row >= footer_row
                && let Some(b) = toolbar_hit(m.column)
            {
                self.dispatch_button(b);
                return;
            }
        }
        if self.view != View::Kanban {
            return;
        }
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some((lane, card)) = self.hit_test(m.column, m.row) {
                    let same = self.lane_idx == lane && card == Some(self.card_idx);
                    self.lane_idx = lane;
                    match card {
                        Some(ci) => {
                            let n = self.fleet.in_lane(self.current_lane()).len();
                            self.card_idx = if n > 0 { ci.min(n - 1) } else { 0 };
                        }
                        None => self.card_idx = 0,
                    }
                    // Click an already-selected card again → open it.
                    if same && self.current_agent_id().is_some() {
                        self.mode = Mode::Inspect;
                    }
                }
            }
            MouseEventKind::ScrollDown => {
                let n = self.fleet.in_lane(self.current_lane()).len();
                if n > 0 {
                    self.card_idx = (self.card_idx + 1) % n;
                }
            }
            MouseEventKind::ScrollUp => {
                let n = self.fleet.in_lane(self.current_lane()).len();
                if n > 0 {
                    self.card_idx = (self.card_idx + n - 1) % n;
                }
            }
            _ => {}
        }
        self.clamp_card();
    }

    /// Map a click (col,row) to (lane, optional card index) against the current
    /// terminal rect. Returns None for clicks in the header/footer chrome.
    fn hit_test(&self, col: u16, row: u16) -> Option<(usize, Option<usize>)> {
        hit_test_geom(self.area, col, row)
    }
}

/// Map a cmux agent-tag status string to our `Status`. The tags come from
/// `cmux top --all` (e.g. "Running", "Needs input", "done") — the same words a
/// human reads off the cmux tree, so this is observation, not a token report.
fn cmux_status(raw: &str) -> Status {
    let s = raw.to_lowercase();
    if s.is_empty() {
        return Status::Idle;
    }
    if ["need", "input", "wait", "block", "ask", "confirm"]
        .iter()
        .any(|k| s.contains(k))
    {
        Status::Blocked
    } else if ["review", "diff", "ready"].iter().any(|k| s.contains(k)) {
        Status::Review
    } else if ["done", "complet", "finish"].iter().any(|k| s.contains(k)) {
        Status::Done
    } else if ["work", "run", "busy", "think", "exec", "stream"]
        .iter()
        .any(|k| s.contains(k))
    {
        Status::Working
    } else {
        Status::Idle
    }
}

/// Pure geometry for hit-testing — shares `HEADER_H`/`FOOTER_H`/`CARD_H` with the
/// renderer so clicks land where cards are drawn. Free fn so it is unit-testable.
fn hit_test_geom(area: Rect, col: u16, row: u16) -> Option<(usize, Option<usize>)> {
    if area.width == 0 || row < HEADER_H || row >= area.height.saturating_sub(FOOTER_H) {
        return None;
    }
    let lane = ((col as u32 * 5) / (area.width.max(1) as u32)).min(4) as usize;
    let inner_top = HEADER_H + 1; // lane block border + content start
    if row < inner_top {
        return Some((lane, None));
    }
    let card = ((row - inner_top) / CARD_H) as usize;
    Some((lane, Some(card)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rect {
        Rect::new(0, 0, 100, 44) // 5 lanes of 20 cols
    }

    #[test]
    fn header_and_footer_are_not_hits() {
        assert_eq!(hit_test_geom(area(), 10, 0), None); // header
        assert_eq!(hit_test_geom(area(), 10, 43), None); // footer row
    }

    #[test]
    fn columns_map_to_lanes() {
        assert_eq!(hit_test_geom(area(), 0, 10).unwrap().0, 0);
        assert_eq!(hit_test_geom(area(), 25, 10).unwrap().0, 1);
        assert_eq!(hit_test_geom(area(), 99, 10).unwrap().0, 4);
    }

    #[test]
    fn toolbar_first_and_gap() {
        // "[1 kanban]" = 10 chars at cols 0..10, then a space at 10, next starts 11.
        assert_eq!(toolbar_hit(0), Some(ButtonId::Kanban));
        assert_eq!(toolbar_hit(9), Some(ButtonId::Kanban));
        assert_eq!(toolbar_hit(10), None); // separator space
        assert_eq!(toolbar_hit(11), Some(ButtonId::Tree));
    }

    #[test]
    fn rows_map_to_cards() {
        // inner_top = HEADER_H + 1 = 5; CARD_H = 6
        assert_eq!(hit_test_geom(area(), 25, 5).unwrap().1, Some(0));
        assert_eq!(hit_test_geom(area(), 25, 11).unwrap().1, Some(1));
        // a click on the lane title/border (above inner_top) selects no card
        assert_eq!(hit_test_geom(area(), 25, 4).unwrap().1, None);
    }
}
