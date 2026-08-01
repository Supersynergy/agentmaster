//! Local IPC bridge for native PTYs owned by an AgentMaster TUI.
//!
//! External agents already have their own control surfaces (tmux, cmux, and
//! runtime CLIs). Native PTYs only exist inside the TUI process, so this
//! line-delimited JSON protocol lets the headless CLI observe and steer them
//! without exposing a network listener or a general shell endpoint.

use std::fs;
use std::io::{BufReader, Read, Write};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const SOCKET_NAME: &str = "native-control.sock";
const MAX_REQUEST_BYTES: u64 = 8 * 1024;
const MAX_RESPONSE_BYTES: u64 = 256 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(2);

/// One native-agent record that is safe to show in headless status output.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NativeAgent {
    #[serde(rename = "ref")]
    pub reference: String,
    pub id: u64,
    pub name: String,
    pub runtime: String,
    pub pid: Option<u32>,
    pub status: String,
    pub cwd: String,
    pub goal: Option<String>,
    pub progress: u8,
    pub last_line: String,
    pub lines_total: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output: Vec<String>,
}

/// Wire commands accepted by the local TUI bridge.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum NativeCommand {
    Ping,
    List,
    Peek { target: String, tail: usize },
    Send { target: String, message: String },
    Interrupt { target: String },
    Focus { target: String },
}

/// Response returned both on the Unix socket and through the TUI event loop.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NativeResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<NativeAgent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<NativeAgent>,
}

impl NativeResponse {
    pub fn ok(message: impl Into<String>) -> Self {
        Self {
            ok: true,
            message: Some(message.into()),
            agents: Vec::new(),
            agent: None,
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            message: Some(message.into()),
            agents: Vec::new(),
            agent: None,
        }
    }
}

/// A command waiting for the TUI event loop. The socket thread never touches a
/// PTY directly; that keeps ownership and state transitions in `app.rs`.
pub struct ControlRequest {
    pub command: NativeCommand,
    pub reply: Sender<NativeResponse>,
}

/// Running Unix-socket endpoint. Dropping it stops the listener and removes the
/// socket, so stale sockets do not masquerade as a live TUI.
pub struct NativeControlServer {
    path: PathBuf,
    running: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl NativeControlServer {
    pub fn start(dir: &Path, requests: Sender<ControlRequest>) -> Result<Self> {
        fs::create_dir_all(dir)?;
        let path = socket_path(dir);
        prepare_socket(&path)?;
        let listener = UnixListener::bind(&path)
            .with_context(|| format!("bind native control socket {}", path.display()))?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;

        let running = Arc::new(AtomicBool::new(true));
        let thread_running = running.clone();
        let thread = thread::spawn(move || {
            while thread_running.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => handle_connection(stream, &requests),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            path,
            running,
            thread: Some(thread),
        })
    }

    fn shutdown(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        if fs::symlink_metadata(&self.path)
            .map(|meta| meta.file_type().is_socket())
            .unwrap_or(false)
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

impl Drop for NativeControlServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Location of the per-user, per-AgentMaster native control socket.
pub fn socket_path(dir: &Path) -> PathBuf {
    let candidate = dir.join(SOCKET_NAME);
    // macOS limits `sockaddr_un.sun_path` far below PATH_MAX. Keep ordinary
    // ~/.agentmaster installs self-contained, but use a deterministic short
    // per-user fallback when callers choose a long custom data directory.
    if candidate.to_string_lossy().len() <= 88 {
        return candidate;
    }
    let digest = Sha256::digest(dir.to_string_lossy().as_bytes());
    let tag = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    PathBuf::from(format!("/tmp/agentmaster-native-{tag}.sock"))
}

/// Ask the currently running TUI to perform a native-PTY operation.
pub fn request(dir: &Path, command: NativeCommand) -> Result<NativeResponse> {
    let path = socket_path(dir);
    let mut stream = UnixStream::connect(&path).with_context(|| {
        format!(
            "native AgentMaster TUI is not running or its control socket is unavailable ({})",
            path.display()
        )
    })?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let payload = serde_json::to_vec(&command)?;
    if payload.len() as u64 > MAX_REQUEST_BYTES {
        bail!("native control request exceeds {MAX_REQUEST_BYTES} bytes");
    }
    stream.write_all(&payload)?;
    stream.write_all(b"\n")?;
    stream.shutdown(std::net::Shutdown::Write)?;

    let mut response = String::new();
    let mut reader = BufReader::new(stream);
    reader
        .by_ref()
        .take(MAX_RESPONSE_BYTES)
        .read_to_string(&mut response)?;
    if response.trim().is_empty() {
        bail!("native AgentMaster TUI returned no control response");
    }
    serde_json::from_str(&response).context("native AgentMaster TUI returned invalid control JSON")
}

/// True when the error means no TUI owns native PTYs right now. Callers should
/// treat that as an empty native fleet rather than an external-backend failure.
pub fn is_inactive_error(error: &anyhow::Error) -> bool {
    let text = error.to_string().to_lowercase();
    text.contains("native agentmaster tui is not running")
        || text.contains("connection refused")
        || text.contains("no such file")
}

fn prepare_socket(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_socket() {
        bail!(
            "refusing to replace non-socket native control path {}",
            path.display()
        );
    }
    if UnixStream::connect(path).is_ok() {
        bail!(
            "another AgentMaster TUI already owns native control at {}",
            path.display()
        );
    }
    fs::remove_file(path)
        .with_context(|| format!("remove stale native control socket {}", path.display()))?;
    Ok(())
}

fn handle_connection(mut stream: UnixStream, requests: &Sender<ControlRequest>) {
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let response = (|| -> Result<NativeResponse> {
        let mut raw = String::new();
        let mut reader = BufReader::new(stream.try_clone()?);
        reader
            .by_ref()
            .take(MAX_REQUEST_BYTES)
            .read_to_string(&mut raw)?;
        if raw.trim().is_empty() {
            bail!("empty native control request");
        }
        let command = serde_json::from_str::<NativeCommand>(&raw)
            .context("invalid native control request JSON")?;
        let (reply_tx, reply_rx): (Sender<NativeResponse>, Receiver<NativeResponse>) =
            mpsc::channel();
        requests
            .send(ControlRequest {
                command,
                reply: reply_tx,
            })
            .map_err(|_| anyhow::anyhow!("AgentMaster TUI control loop stopped"))?;
        reply_rx
            .recv_timeout(IO_TIMEOUT)
            .map_err(|_| anyhow::anyhow!("AgentMaster TUI control loop timed out"))
    })()
    .unwrap_or_else(|error| NativeResponse::error(error.to_string()));
    let encoded = serde_json::to_vec(&response).unwrap_or_else(|_| {
        serde_json::to_vec(&NativeResponse::error(
            "failed to encode native control response",
        ))
        .unwrap_or_else(|_| b"{\"ok\":false}".to_vec())
    });
    let encoded = if encoded.len() as u64 > MAX_RESPONSE_BYTES {
        serde_json::to_vec(&NativeResponse::error(
            "native control response exceeds 256 KiB",
        ))
        .unwrap_or_else(|_| b"{\"ok\":false}".to_vec())
    } else {
        encoded
    };
    let _ = stream.write_all(&encoded);
    let _ = stream.write_all(b"\n");
    let _ = stream.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("agentmaster-{label}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn native_socket_round_trip_routes_a_structured_request() {
        let dir = temp_dir("native-control-test");
        let (tx, rx) = mpsc::channel();
        let server = NativeControlServer::start(&dir, tx).unwrap();
        let worker = thread::spawn(move || {
            let request = rx.recv_timeout(Duration::from_secs(1)).unwrap();
            assert_eq!(request.command, NativeCommand::List);
            request
                .reply
                .send(NativeResponse {
                    ok: true,
                    message: Some("one native agent".into()),
                    agents: vec![NativeAgent {
                        reference: "native:7".into(),
                        id: 7,
                        name: "codex-7".into(),
                        runtime: "codex".into(),
                        pid: Some(42),
                        status: "working".into(),
                        cwd: "/tmp".into(),
                        goal: None,
                        progress: 0,
                        last_line: "running".into(),
                        lines_total: 1,
                        output: Vec::new(),
                    }],
                    agent: None,
                })
                .unwrap();
        });
        let response = request(&dir, NativeCommand::List).unwrap();
        assert!(response.ok, "{:?}", response.message);
        assert_eq!(response.agents[0].reference, "native:7");
        worker.join().unwrap();
        drop(server);
        assert!(!socket_path(&dir).exists());
        let _ = fs::remove_dir(&dir);
    }

    #[test]
    fn refuses_to_replace_a_regular_file_at_the_socket_path() {
        let dir = temp_dir("native-control-file");
        fs::write(socket_path(&dir), "not a socket").unwrap();
        let (tx, _rx) = mpsc::channel();
        let error = NativeControlServer::start(&dir, tx)
            .err()
            .expect("regular file must be rejected");
        assert!(error.to_string().contains("refusing to replace non-socket"));
        let _ = fs::remove_file(socket_path(&dir));
        let _ = fs::remove_dir(&dir);
    }

    #[test]
    fn inactive_errors_are_not_reported_as_a_live_backend_failure() {
        let error = anyhow::anyhow!("native AgentMaster TUI is not running");
        assert!(is_inactive_error(&error));
        assert!(!is_inactive_error(&anyhow::anyhow!("permission denied")));
    }

    #[test]
    fn long_data_paths_use_a_short_deterministic_socket_path() {
        let long = format!("/tmp/{}", "agentmaster-data-".repeat(12));
        let dir = Path::new(&long);
        let socket = socket_path(dir);
        assert!(
            socket
                .to_string_lossy()
                .starts_with("/tmp/agentmaster-native-")
        );
        assert!(socket.to_string_lossy().len() <= 88);
        assert_eq!(socket, socket_path(dir));
    }
}
