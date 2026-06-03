//! Application state + the single-threaded event loop. Render is a pure function
//! of `App` (see `ui`); all mutation happens here. PTY reader threads feed events
//! in over a channel; crossterm key events drive the state machine.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};

use crate::fleet::{Agent, Fleet, Lane, Status};
use crate::obs::Metrics;
use crate::store::Store;
use crate::{pty, runtime, state};

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
    pub should_quit: bool,
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
    let res = app.main_loop(&mut terminal);
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
            status_msg: "ready — press n to spawn an agent, ? for help".into(),
            should_quit: false,
            tick: 0,
        }
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
            terminal.draw(|f| crate::ui::render(f, self))?;
            if event::poll(Duration::from_millis(50))?
                && let Event::Key(k) = event::read()?
            {
                self.handle_key(k);
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
        for a in self.fleet.agents.iter_mut() {
            state::idle_sweep(a, 20);
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

    // ---- pty events -------------------------------------------------------

    fn handle_pty(&mut self, ev: AppEvent) {
        match ev {
            AppEvent::Output { id, line } => {
                let mut transition = None;
                if let Some(a) = self.fleet.get_mut(id) {
                    if let Some(ns) = state::detect(a, &line)
                        && ns != a.status
                    {
                        transition = Some((a.name.clone(), ns));
                        a.status = ns;
                    }
                    a.push_line(line);
                }
                if let Some((name, ns)) = transition {
                    self.store.log(Some(id), &name, "state", ns.label());
                    tracing::info!(agent = id, status = ns.label(), "state change");
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
    }

    fn send_line(&mut self, id: u64, text: &str) {
        use std::io::Write;
        let mut ok = false;
        let mut name = String::new();
        if let Some(a) = self.fleet.get_mut(id) {
            name = a.name.clone();
            if let Some(h) = a.pty.as_mut() {
                let _ = h.writer.write_all(text.as_bytes());
                let _ = h.writer.write_all(b"\n");
                let _ = h.writer.flush();
                ok = true;
                if matches!(a.status, Status::Blocked | Status::Idle) {
                    a.status = Status::Working;
                }
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
            if let Some(h) = a.pty.as_mut() {
                let _ = h.child.kill();
            }
            a.status = Status::Dead;
        }
        self.store.log(Some(id), &name, "kill", "killed by user");
        tracing::warn!(agent = id, "killed by user");
        self.status_msg = format!("killed {name}");
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
            InputKind::None => {}
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
                KeyCode::Char('s') if self.current_agent_id().is_some() => {
                    self.start_input(InputKind::Send);
                }
                KeyCode::Char('/') => self.start_input(InputKind::Filter),
                KeyCode::Char('K') => {
                    if let Some(id) = self.current_agent_id() {
                        self.kill(id);
                    }
                }
                _ => {}
            },
        }
        // Clamp the card cursor to the (possibly changed) lane size.
        let n = self.fleet.in_lane(self.current_lane()).len();
        if n == 0 {
            self.card_idx = 0;
        } else if self.card_idx >= n {
            self.card_idx = n - 1;
        }
    }
}
