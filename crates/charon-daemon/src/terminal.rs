//! Terminal manager (M2.4).
//!
//! Each terminal is a PTY (via `portable-pty`) running a shell or user-supplied
//! command inside a workspace's worktree. One blocking thread per PTY drains
//! output into a `tokio::sync::broadcast` channel that every WS session
//! subscribes to. Writes / resize / kill go through `tokio::task::spawn_blocking`.
//!
//! Output is wired as base64-encoded chunks inside JSON envelopes — simpler
//! than the 36-byte UUID prefix binary attachment scheme docs/03 sketches, and
//! enough for typical shell output. If we ever want to stream big binary blobs
//! we can layer the binary attachment scheme on top without breaking these
//! frames.
//!
//! Scrollback is an in-memory ring buffer per terminal (1 MiB cap by default).
//! Terminals do NOT persist across daemon restarts (the child process dies
//! with the daemon anyway).

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use charon_core::{
    CreateTerminalPayload, Terminal, TerminalListResponse, TerminalScrollbackResponse,
    TerminalStatus, Workspace,
};
use chrono::Utc;
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::{RwLock, broadcast};
use tracing::{debug, info, warn};
use uuid::Uuid;

const DEFAULT_SCROLLBACK_BYTES: usize = 1024 * 1024; // 1 MiB
const PTY_READ_CHUNK: usize = 4096;
const BROADCAST_CAPACITY: usize = 1024;
const FIXED_TERM: &str = "xterm-256color";

#[derive(Debug, Clone)]
pub enum TerminalEvent {
    Output {
        terminal_id: String,
        data: Bytes,
        seq: u64,
    },
    Exited {
        terminal_id: String,
        exit_code: Option<i32>,
    },
}

pub struct TerminalManager {
    inner: RwLock<HashMap<String, Arc<TerminalHandle>>>,
    events_tx: broadcast::Sender<TerminalEvent>,
}

struct TerminalHandle {
    info: std::sync::Mutex<Terminal>,
    master: std::sync::Mutex<Box<dyn MasterPty + Send>>,
    writer: std::sync::Mutex<Box<dyn Write + Send>>,
    child: std::sync::Mutex<Box<dyn portable_pty::Child + Send + Sync>>,
    scrollback: std::sync::Mutex<VecDeque<u8>>,
    next_seq: AtomicU64,
}

impl Default for TerminalManager {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalManager {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        Self {
            inner: RwLock::new(HashMap::new()),
            events_tx: tx,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<TerminalEvent> {
        self.events_tx.subscribe()
    }

    pub async fn create(
        &self,
        workspace: &Workspace,
        payload: CreateTerminalPayload,
    ) -> Result<Terminal> {
        let id = Uuid::new_v4().to_string();
        let cmd_str = payload
            .command
            .unwrap_or_else(|| std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string()));
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: payload.rows,
                cols: payload.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("openpty")?;

        let mut cmd = CommandBuilder::new(&cmd_str);
        cmd.cwd(&workspace.worktree_path);
        populate_terminal_env(&mut cmd, &payload.env);

        let child = pair
            .slave
            .spawn_command(cmd)
            .context("spawn command in PTY slave")?;
        // Drop our slave handle so master EOFs cleanly when the child exits.
        drop(pair.slave);

        let writer = pair.master.take_writer().context("PTY take_writer")?;
        let reader = pair.master.try_clone_reader().context("PTY clone_reader")?;

        let info = Terminal {
            id: id.clone(),
            workspace_id: workspace.id.clone(),
            command: cmd_str.clone(),
            cols: payload.cols,
            rows: payload.rows,
            status: TerminalStatus::Running,
            created_at: Utc::now(),
            exited_at: None,
            exit_code: None,
        };

        let handle = Arc::new(TerminalHandle {
            info: std::sync::Mutex::new(info.clone()),
            master: std::sync::Mutex::new(pair.master),
            writer: std::sync::Mutex::new(writer),
            child: std::sync::Mutex::new(child),
            scrollback: std::sync::Mutex::new(VecDeque::new()),
            next_seq: AtomicU64::new(0),
        });

        self.inner.write().await.insert(id.clone(), handle.clone());

        // Reader: blocking thread, drains PTY → scrollback + broadcast.
        let events_tx = self.events_tx.clone();
        let handle_for_reader = handle.clone();
        let id_for_reader = id.clone();
        tokio::task::spawn_blocking(move || {
            reader_loop(reader, handle_for_reader, events_tx, id_for_reader);
        });

        // Waiter: blocking thread, marks status + emits Exited when child dies.
        let events_tx = self.events_tx.clone();
        let handle_for_waiter = handle.clone();
        let id_for_waiter = id.clone();
        tokio::task::spawn_blocking(move || {
            waiter_loop(handle_for_waiter, events_tx, id_for_waiter);
        });

        info!(
            id = %id,
            workspace = %workspace.id,
            command = %cmd_str,
            cols = payload.cols,
            rows = payload.rows,
            "terminal spawned"
        );
        Ok(info)
    }

    pub async fn list(&self, workspace_id: Option<&str>) -> Vec<Terminal> {
        let map = self.inner.read().await;
        map.values()
            .filter_map(|h| {
                let info = h.info.lock().unwrap().clone();
                match workspace_id {
                    Some(wid) if info.workspace_id != wid => None,
                    _ => Some(info),
                }
            })
            .collect()
    }

    pub async fn list_response(&self, workspace_id: Option<&str>) -> TerminalListResponse {
        TerminalListResponse {
            terminals: self.list(workspace_id).await,
        }
    }

    pub async fn send_keys(&self, terminal_id: &str, data: Vec<u8>) -> Result<usize> {
        let handle = self.get_handle(terminal_id).await?;
        let n = data.len();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut writer = handle.writer.lock().unwrap();
            writer.write_all(&data).context("PTY write")?;
            writer.flush().ok();
            Ok(())
        })
        .await
        .context("spawn_blocking write")??;
        Ok(n)
    }

    pub async fn resize(&self, terminal_id: &str, cols: u16, rows: u16) -> Result<()> {
        let handle = self.get_handle(terminal_id).await?;
        tokio::task::spawn_blocking(move || -> Result<()> {
            let master = handle.master.lock().unwrap();
            master
                .resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .context("PTY resize")?;
            let mut info = handle.info.lock().unwrap();
            info.cols = cols;
            info.rows = rows;
            Ok(())
        })
        .await
        .context("spawn_blocking resize")??;
        Ok(())
    }

    pub async fn kill(&self, terminal_id: &str) -> Result<()> {
        let handle = self.get_handle(terminal_id).await?;
        tokio::task::spawn_blocking(move || {
            let mut child = handle.child.lock().unwrap();
            let _ = child.kill();
        })
        .await
        .context("spawn_blocking kill")?;
        Ok(())
    }

    pub async fn scrollback(&self, terminal_id: &str) -> Result<TerminalScrollbackResponse> {
        let handle = self.get_handle(terminal_id).await?;
        let bytes: Vec<u8> = {
            let sb = handle.scrollback.lock().unwrap();
            sb.iter().copied().collect()
        };
        use base64::{Engine, engine::general_purpose::STANDARD};
        Ok(TerminalScrollbackResponse {
            terminal_id: terminal_id.to_string(),
            data_b64: STANDARD.encode(&bytes),
        })
    }

    async fn get_handle(&self, id: &str) -> Result<Arc<TerminalHandle>> {
        self.inner
            .read()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("terminal {id} not found"))
    }
}

/// PTYs should not inherit daemon credentials. Start from an empty environment,
/// copy only PATH, HOME, USER, SHELL, LANG, and LC_* from the daemon, then add
/// request-scoped opt-ins.
fn populate_terminal_env(cmd: &mut CommandBuilder, explicit_env: &HashMap<String, String>) {
    cmd.env_clear();

    for (key, value) in std::env::vars().filter(|(key, _)| is_allowed_daemon_env(key)) {
        cmd.env(key, value);
    }

    for (key, value) in explicit_env {
        cmd.env(key, value);
    }

    cmd.env("TERM", FIXED_TERM);
}

fn is_allowed_daemon_env(key: &str) -> bool {
    matches!(key, "PATH" | "HOME" | "USER" | "SHELL" | "LANG") || key.starts_with("LC_")
}

fn reader_loop(
    mut reader: Box<dyn Read + Send>,
    handle: Arc<TerminalHandle>,
    events_tx: broadcast::Sender<TerminalEvent>,
    terminal_id: String,
) {
    let mut buf = vec![0u8; PTY_READ_CHUNK];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => {
                debug!(terminal_id, "PTY EOF; reader exiting");
                break;
            }
            Ok(n) => {
                let chunk = Bytes::copy_from_slice(&buf[..n]);
                {
                    let mut sb = handle.scrollback.lock().unwrap();
                    sb.extend(chunk.iter());
                    while sb.len() > DEFAULT_SCROLLBACK_BYTES {
                        sb.pop_front();
                    }
                }
                let seq = handle.next_seq.fetch_add(1, Ordering::Relaxed);
                let _ = events_tx.send(TerminalEvent::Output {
                    terminal_id: terminal_id.clone(),
                    data: chunk,
                    seq,
                });
            }
            Err(e) => {
                debug!(?e, terminal_id, "PTY read error; reader exiting");
                break;
            }
        }
    }
}

fn waiter_loop(
    handle: Arc<TerminalHandle>,
    events_tx: broadcast::Sender<TerminalEvent>,
    terminal_id: String,
) {
    let exit_code: Option<i32> = {
        let mut child = handle.child.lock().unwrap();
        match child.wait() {
            Ok(status) => Some(status.exit_code() as i32),
            Err(e) => {
                warn!(?e, terminal_id, "child wait failed");
                None
            }
        }
    };
    {
        let mut info = handle.info.lock().unwrap();
        info.status = TerminalStatus::Exited;
        info.exited_at = Some(Utc::now());
        info.exit_code = exit_code;
    }
    let _ = events_tx.send(TerminalEvent::Exited {
        terminal_id: terminal_id.clone(),
        exit_code,
    });
    info!(terminal_id, ?exit_code, "terminal exited");
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
    use chrono::Utc;
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::sync::broadcast::error::RecvError;

    struct EnvVarGuard {
        key: String,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &str, value: &str) -> Self {
            let previous = std::env::var_os(key);
            unsafe {
                std::env::set_var(key, value);
            }
            Self {
                key: key.to_string(),
                previous,
            }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.previous {
                    Some(value) => std::env::set_var(&self.key, value),
                    None => std::env::remove_var(&self.key),
                }
            }
        }
    }

    fn fake_workspace(worktree: PathBuf) -> Workspace {
        Workspace {
            id: "ws-test".to_string(),
            title: None,
            project_root: worktree.clone(),
            base_branch: "main".to_string(),
            branch: "test".to_string(),
            worktree_path: worktree,
            created_at: Utc::now(),
            archived_at: None,
        }
    }

    async fn run_env_terminal(explicit_env: HashMap<String, String>) -> String {
        let tmp = TempDir::new().unwrap();
        let workspace = fake_workspace(tmp.path().to_path_buf());
        let manager = TerminalManager::new();
        let mut events = manager.subscribe();
        let terminal = manager
            .create(
                &workspace,
                CreateTerminalPayload {
                    workspace_id: workspace.id.clone(),
                    command: Some("/usr/bin/env".to_string()),
                    env: explicit_env,
                    cols: 80,
                    rows: 24,
                },
            )
            .await
            .unwrap();

        wait_for_terminal_exit(&mut events, &terminal.id).await;
        read_settled_scrollback(&manager, &terminal.id).await
    }

    async fn wait_for_terminal_exit(
        events: &mut broadcast::Receiver<TerminalEvent>,
        terminal_id: &str,
    ) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match events.recv().await {
                    Ok(TerminalEvent::Exited {
                        terminal_id: id, ..
                    }) if id == terminal_id => break,
                    Ok(_) | Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => panic!("terminal event channel closed"),
                }
            }
        })
        .await
        .expect("terminal did not exit");
    }

    async fn read_settled_scrollback(manager: &TerminalManager, terminal_id: &str) -> String {
        let mut previous = Vec::new();
        let mut stable_ticks = 0;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);

        loop {
            let response = manager.scrollback(terminal_id).await.unwrap();
            let bytes = BASE64.decode(response.data_b64).unwrap();
            if !bytes.is_empty() && bytes == previous {
                stable_ticks += 1;
                if stable_ticks >= 2 {
                    return String::from_utf8_lossy(&bytes).into_owned();
                }
            } else {
                previous = bytes;
                stable_ticks = 0;
            }

            assert!(
                tokio::time::Instant::now() < deadline,
                "terminal scrollback did not settle"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn create_filters_daemon_environment() {
        let _guard = EnvVarGuard::set("FAKE_API_KEY", "secret-from-daemon");

        let output = run_env_terminal(HashMap::new()).await;

        assert!(!output.contains("FAKE_API_KEY"));
        assert!(!output.contains("secret-from-daemon"));
        assert!(output.contains("TERM=xterm-256color"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn create_includes_explicit_environment() {
        let output = run_env_terminal(HashMap::from([(
            "CHARON_EXPLICIT_ENV".to_string(),
            "forwarded".to_string(),
        )]))
        .await;

        assert!(output.contains("CHARON_EXPLICIT_ENV=forwarded"));
    }
}
