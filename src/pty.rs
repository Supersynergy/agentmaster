//! Native PTY backend. agentmaster owns the pseudo-terminal itself, so it works
//! on *any* terminal or bare Linux — no tmux/zellij/kitty cooperation required.
//! (Adapters that attach to agents already living in tmux/cmux/rmux are a later
//! stage; this universal native path is the floor that always works.)

use std::io::{Read, Write};
use std::sync::mpsc::Sender;
use std::thread;

use anyhow::Result;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::app::AppEvent;

/// Handles we keep alive for a running agent. Dropping `master` closes the PTY,
/// so the Agent owns it for the process lifetime.
pub struct PtyHandle {
    /// Held purely for RAII: dropping the master closes the PTY and signals the
    /// child. Never read directly — the reader thread owns a cloned reader.
    #[allow(dead_code)]
    pub master: Box<dyn MasterPty + Send>,
    pub writer: Box<dyn Write + Send>,
    pub child: Box<dyn Child + Send + Sync>,
}

/// Spawn `program args` under a fresh PTY in `cwd`. A reader thread streams clean
/// (ANSI-stripped) lines back over `tx` as `AppEvent::Output`, and signals
/// `AppEvent::Exited` on EOF.
pub fn spawn(
    id: u64,
    program: &str,
    args: &[String],
    cwd: &str,
    env: &[(String, String)],
    tx: Sender<AppEvent>,
) -> Result<PtyHandle> {
    let sys = native_pty_system();
    let pair = sys.openpty(PtySize {
        rows: 40,
        cols: 120,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let mut cmd = CommandBuilder::new(program);
    for a in args {
        cmd.arg(a);
    }
    cmd.cwd(cwd);
    for (k, v) in env {
        cmd.env(k, v);
    }
    // Make agent CLIs behave: a real TERM and disabled pagers help line parsing.
    cmd.env("TERM", "xterm-256color");

    let child = pair.slave.spawn_command(cmd)?;
    // Move the master out, then drop the slave so the master sees EOF on exit.
    let master = pair.master;
    drop(pair.slave);

    let mut reader = master.try_clone_reader()?;
    let writer = master.take_writer()?;

    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let mut acc = String::new();
        loop {
            match reader.read(&mut buf) {
                Ok(0) => {
                    let _ = tx.send(AppEvent::Exited { id });
                    break;
                }
                Ok(n) => {
                    acc.push_str(&String::from_utf8_lossy(&buf[..n]));
                    while let Some(pos) = acc.find('\n') {
                        let raw: String = acc.drain(..=pos).collect();
                        let line = clean_line(&raw);
                        if !line.trim().is_empty() {
                            let _ = tx.send(AppEvent::Output { id, line });
                        }
                    }
                    // Flush very long unterminated output (e.g. a live progress bar)
                    // so the UI still updates instead of waiting for a newline.
                    if acc.len() > 8192 {
                        let line = clean_line(&acc);
                        acc.clear();
                        if !line.trim().is_empty() {
                            let _ = tx.send(AppEvent::Output { id, line });
                        }
                    }
                }
                Err(_) => {
                    let _ = tx.send(AppEvent::Exited { id });
                    break;
                }
            }
        }
    });

    Ok(PtyHandle {
        master,
        writer,
        child,
    })
}

fn clean_line(s: &str) -> String {
    strip_ansi(s).trim_end_matches(['\r', '\n']).to_string()
}

/// Strip ANSI CSI/OSC escape sequences and carriage returns. Char-based so it
/// never splits a UTF-8 codepoint.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    while let Some(&nc) = chars.peek() {
                        chars.next();
                        if ('\u{40}'..='\u{7e}').contains(&nc) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(&nc) = chars.peek() {
                        chars.next();
                        if nc == '\u{07}' {
                            break;
                        }
                        if nc == '\u{1b}' {
                            if chars.peek().copied() == Some('\\') {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                _ => { /* lone ESC: drop */ }
            }
        } else if c != '\r' {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_csi_color_codes() {
        let s = "\u{1b}[31mhello\u{1b}[0m world\r";
        assert_eq!(clean_line(s), "hello world");
    }

    #[test]
    fn strips_osc_title() {
        let s = "\u{1b}]0;my title\u{07}done";
        assert_eq!(clean_line(s), "done");
    }

    #[test]
    fn keeps_utf8() {
        assert_eq!(clean_line("⎇ feat/möp ✓"), "⎇ feat/möp ✓");
    }
}
