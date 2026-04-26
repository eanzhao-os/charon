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
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::{RwLock, broadcast};
use tracing::{debug, info, warn};
use uuid::Uuid;

const DEFAULT_SCROLLBACK_BYTES: usize = 1024 * 1024; // 1 MiB
const PTY_READ_CHUNK: usize = 4096;
const BROADCAST_CAPACITY: usize = 1024;

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
    master: std::sync::Mutex<Option<Box<dyn MasterPty + Send>>>,
    writer: std::sync::Mutex<Option<Box<dyn Write + Send>>>,
    child: std::sync::Mutex<Option<Box<dyn portable_pty::Child + Send + Sync>>>,
    killer: std::sync::Mutex<Option<Box<dyn ChildKiller + Send + Sync>>>,
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
        for (k, v) in std::env::vars() {
            cmd.env(k, v);
        }
        cmd.env("TERM", "xterm-256color");

        let child = pair
            .slave
            .spawn_command(cmd)
            .context("spawn command in PTY slave")?;
        let killer = child.clone_killer();
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
            master: std::sync::Mutex::new(Some(pair.master)),
            writer: std::sync::Mutex::new(Some(writer)),
            child: std::sync::Mutex::new(Some(child)),
            killer: std::sync::Mutex::new(Some(killer)),
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
        let terminal_id = terminal_id.to_string();
        let n = data.len();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut writer = handle.writer.lock().unwrap();
            let writer = writer
                .as_mut()
                .ok_or_else(|| anyhow!("terminal {terminal_id} has exited"))?;
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
        let terminal_id = terminal_id.to_string();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let master = handle.master.lock().unwrap();
            let master = master
                .as_ref()
                .ok_or_else(|| anyhow!("terminal {terminal_id} has exited"))?;
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
        let terminal_id = terminal_id.to_string();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut killer = handle.killer.lock().unwrap();
            let killer = killer
                .as_mut()
                .ok_or_else(|| anyhow!("terminal {terminal_id} has exited"))?;
            killer.kill().context("PTY child kill")?;
            Ok(())
        })
        .await
        .context("spawn_blocking kill")??;
        Ok(())
    }

    pub async fn remove(&self, terminal_id: &str) -> Result<Terminal> {
        let mut map = self.inner.write().await;
        let handle = map
            .get(terminal_id)
            .cloned()
            .ok_or_else(|| anyhow!("terminal {terminal_id} not found"))?;
        let info = handle.info.lock().unwrap().clone();
        if info.status == TerminalStatus::Running {
            return Err(anyhow!("terminal {terminal_id} is still running"));
        }
        map.remove(terminal_id);
        Ok(info)
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
        let exit_code = match child.as_mut() {
            Some(child) => match child.wait() {
                Ok(status) => Some(status.exit_code() as i32),
                Err(e) => {
                    warn!(?e, terminal_id, "child wait failed");
                    None
                }
            },
            None => {
                warn!(terminal_id, "child handle already released before wait");
                None
            }
        };
        *child = None;
        exit_code
    };
    *handle.writer.lock().unwrap() = None;
    *handle.master.lock().unwrap() = None;
    *handle.killer.lock().unwrap() = None;
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

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Duration;

    use tempfile::TempDir;

    use super::*;

    fn test_workspace(tmp: &TempDir) -> Workspace {
        Workspace {
            id: "ws-test".to_string(),
            title: None,
            project_root: tmp.path().to_path_buf(),
            base_branch: "main".to_string(),
            branch: "test".to_string(),
            worktree_path: tmp.path().to_path_buf(),
            created_at: Utc::now(),
            archived_at: None,
        }
    }

    fn command(candidates: &[&str], fallback: &str) -> String {
        candidates
            .iter()
            .find(|candidate| Path::new(candidate).exists())
            .copied()
            .unwrap_or(fallback)
            .to_string()
    }

    async fn wait_for_exit(
        events: &mut broadcast::Receiver<TerminalEvent>,
        terminal_id: &str,
    ) -> Option<i32> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                panic!("timed out waiting for terminal {terminal_id} exit");
            }
            match tokio::time::timeout(remaining, events.recv()).await {
                Ok(Ok(TerminalEvent::Exited {
                    terminal_id: id,
                    exit_code,
                })) if id == terminal_id => return exit_code,
                Ok(Ok(_)) => {}
                Ok(Err(e)) => panic!("terminal event channel failed: {e}"),
                Err(_) => panic!("timed out waiting for terminal {terminal_id} exit"),
            }
        }
    }

    #[tokio::test]
    async fn exited_terminals_release_pty_resources_and_can_be_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = test_workspace(&tmp);
        let manager = TerminalManager::new();
        let mut events = manager.subscribe();
        let true_cmd = command(&["/usr/bin/true", "/bin/true"], "true");
        let mut terminal_ids = Vec::new();

        for _ in 0..10 {
            let terminal = manager
                .create(
                    &workspace,
                    CreateTerminalPayload {
                        workspace_id: workspace.id.clone(),
                        command: Some(true_cmd.clone()),
                        cols: 80,
                        rows: 24,
                    },
                )
                .await
                .unwrap();
            terminal_ids.push(terminal.id);
        }

        for terminal_id in &terminal_ids {
            wait_for_exit(&mut events, terminal_id).await;
        }

        let listed = manager.list(Some(&workspace.id)).await;
        assert_eq!(listed.len(), terminal_ids.len());
        assert!(
            listed
                .iter()
                .all(|terminal| terminal.status == TerminalStatus::Exited)
        );

        for terminal_id in &terminal_ids {
            let handle = manager.get_handle(terminal_id).await.unwrap();
            assert!(handle.master.lock().unwrap().is_none());
            assert!(handle.writer.lock().unwrap().is_none());
            assert!(handle.child.lock().unwrap().is_none());
            assert!(handle.killer.lock().unwrap().is_none());

            let removed = manager.remove(terminal_id).await.unwrap();
            assert_eq!(removed.status, TerminalStatus::Exited);
        }

        assert!(manager.list(Some(&workspace.id)).await.is_empty());
    }

    #[tokio::test]
    async fn remove_refuses_running_terminals() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = test_workspace(&tmp);
        let manager = TerminalManager::new();
        let mut events = manager.subscribe();
        let cat_cmd = command(&["/bin/cat", "/usr/bin/cat"], "cat");

        let terminal = manager
            .create(
                &workspace,
                CreateTerminalPayload {
                    workspace_id: workspace.id.clone(),
                    command: Some(cat_cmd),
                    cols: 80,
                    rows: 24,
                },
            )
            .await
            .unwrap();

        let err = manager.remove(&terminal.id).await.unwrap_err();
        assert!(err.to_string().contains("still running"));

        manager.kill(&terminal.id).await.unwrap();
        wait_for_exit(&mut events, &terminal.id).await;
        manager.remove(&terminal.id).await.unwrap();
        assert!(manager.list(Some(&workspace.id)).await.is_empty());
    }
}
