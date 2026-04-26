use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use charon_core::{
    ClientFrame, CreateTerminalPayload, CreateWorkspaceRequest, DEFAULT_DAEMON_BIND,
    DEFAULT_NYXID_ISSUER, DEFAULT_USER_SERVICE_SLUG, HealthResponse, NyxIdentity, ServerFrame,
    TerminalIdPayload, TerminalSendKeysPayload, WhoAmIResponse, WorkspaceIdPayload,
    WorkspaceListPayload,
};
use charon_daemon::{DEFAULT_EXPECTED_AUD, DaemonConfig, default_home};
use clap::{Parser, Subcommand};
use futures::sink::SinkExt;
use futures::stream::StreamExt;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::Message as TungsteniteMessage;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;
use zeroize::Zeroizing;

#[derive(Parser, Debug)]
#[command(
    name = "charon",
    version,
    about = "Charon — remote AI workspace daemon"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Daemon lifecycle.
    Daemon {
        #[command(subcommand)]
        sub: DaemonCmd,
    },
    /// Run end-to-end self-checks: local daemon + nyxid node + proxy round-trip.
    Doctor {
        #[arg(
            long,
            env = "CHARON_ENDPOINT",
            default_value = "http://127.0.0.1:18789"
        )]
        endpoint: String,
        #[arg(long, env = "CHARON_DOCTOR_SLUG", default_value = DEFAULT_USER_SERVICE_SLUG)]
        slug: String,
        #[arg(long, env = "CHARON_NYXID_BASE_URL", default_value = DEFAULT_NYXID_ISSUER)]
        base_url: String,
    },
    /// WebSocket diagnostics.
    Ws {
        #[command(subcommand)]
        sub: WsCmd,
    },
}

#[derive(Subcommand, Debug)]
enum WsCmd {
    /// End-to-end terminal smoke: create workspace + terminal, run `echo`, verify output.
    ProbeTerminal {
        #[arg(long, env = "CHARON_NYXID_BASE_URL", default_value = DEFAULT_NYXID_ISSUER)]
        base_url: String,
        #[arg(long, env = "CHARON_DOCTOR_SLUG", default_value = DEFAULT_USER_SERVICE_SLUG)]
        slug: String,
        /// Project root for the throwaway workspace (auto git-init'd if needed).
        /// Defaults to a fresh per-invocation tempdir, removed on exit.
        #[arg(long)]
        project_root: Option<PathBuf>,
    },
}

#[derive(Subcommand, Debug)]
enum DaemonCmd {
    /// Start the daemon in the foreground (M2 will add launchd/systemd install).
    Start {
        #[arg(long, env = "CHARON_BIND", default_value = DEFAULT_DAEMON_BIND)]
        bind: String,
        #[arg(long, env = "CHARON_EXPECTED_AUD", default_value = DEFAULT_EXPECTED_AUD)]
        expected_aud: String,
        #[arg(long, env = "CHARON_NYXID_ISSUER", default_value = DEFAULT_NYXID_ISSUER)]
        nyxid_issuer: String,
        #[arg(long, env = "CHARON_OWNER_USER_ID")]
        owner_user_id: String,
        /// Daemon state dir (workspaces.json, worktrees/, …). Defaults to ~/.charon.
        #[arg(long, env = "CHARON_HOME")]
        home: Option<PathBuf>,
    },
    /// Hit the daemon's /api/v1/health endpoint.
    Status {
        #[arg(long, default_value = "http://127.0.0.1:18789")]
        endpoint: String,
    },
}

struct NyxidToken {
    inner: Zeroizing<String>,
}

impl NyxidToken {
    fn new(token: String) -> Self {
        Self {
            inner: Zeroizing::new(token),
        }
    }

    fn as_str(&self) -> &str {
        self.inner.as_str()
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Daemon {
            sub:
                DaemonCmd::Start {
                    bind,
                    expected_aud,
                    nyxid_issuer,
                    owner_user_id,
                    home,
                },
        } => start_daemon(bind, expected_aud, nyxid_issuer, owner_user_id, home).await,
        Cmd::Daemon {
            sub: DaemonCmd::Status { endpoint },
        } => status(&endpoint).await,
        Cmd::Doctor {
            endpoint,
            slug,
            base_url,
        } => doctor(&endpoint, &slug, &base_url).await,
        Cmd::Ws {
            sub:
                WsCmd::ProbeTerminal {
                    base_url,
                    slug,
                    project_root,
                },
        } => probe_terminal(&base_url, &slug, project_root).await,
    }
}

async fn start_daemon(
    bind: String,
    expected_aud: String,
    nyxid_issuer: String,
    owner_user_id: String,
    home: Option<PathBuf>,
) -> Result<()> {
    let home = match home {
        Some(p) => p,
        None => default_home()?,
    };
    let config = DaemonConfig {
        bind: bind
            .parse()
            .with_context(|| format!("invalid bind {bind}"))?,
        expected_aud,
        nyxid_issuer,
        owner_user_id,
        home,
    };
    let shutdown = CancellationToken::new();
    let signal_token = shutdown.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::info!("ctrl-c received, shutting down");
            signal_token.cancel();
        }
    });
    charon_daemon::serve(config, Some(shutdown)).await
}

async fn status(endpoint: &str) -> Result<()> {
    let url = format!("{}/api/v1/health", endpoint.trim_end_matches('/'));
    let resp = reqwest::get(&url)
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    println!("{status} {body}");
    if !status.is_success() {
        bail!("daemon returned non-2xx");
    }
    Ok(())
}

async fn doctor(local_endpoint: &str, slug: &str, base_url: &str) -> Result<()> {
    println!("== charon doctor ==\n");
    let mut failures: Vec<&str> = Vec::new();

    println!("[1] local charon-daemon at {local_endpoint}");
    match local_health(local_endpoint).await {
        Ok(version) => println!("  ✓ charon-daemon {version}\n"),
        Err(e) => {
            println!("  ✗ {e:#}");
            println!("  hint: run `charon daemon start` or `cargo run -p charon-daemon`\n");
            failures.push("local-daemon");
        }
    }

    println!("[2] nyxid node daemon");
    match nyxid_node_daemon_pid() {
        Ok(pid) => println!("  ✓ running (PID {pid})\n"),
        Err(e) => {
            println!("  ✗ {e:#}");
            println!("  hint: run `nyxid node daemon start`\n");
            failures.push("nyxid-node-daemon");
        }
    }

    let proxy_url = format!(
        "{}/api/v1/proxy/s/{}/api/v1/whoami",
        base_url.trim_end_matches('/'),
        slug
    );
    println!("[3] end-to-end /whoami via NyxID proxy");
    println!("    GET {proxy_url}");
    let http_identity = match e2e_whoami(&proxy_url).await {
        Ok(identity) => {
            println!(
                "  ✓ user_id={} email={}",
                identity.user_id,
                identity.email.as_deref().unwrap_or("?")
            );
            println!(
                "    roles={:?} permissions={} groups={} expires_at={}\n",
                identity.roles,
                identity.permissions.len(),
                identity.groups.len(),
                identity.expires_at
            );
            Some(identity)
        }
        Err(e) => {
            println!("  ✗ {e:#}\n");
            failures.push("end-to-end");
            None
        }
    };

    let ws_url = build_ws_url(base_url, slug);
    println!("[4] WS handshake via NyxID proxy");
    println!("    {ws_url}");
    match ws_handshake_probe(&ws_url).await {
        Ok(ws_identity) => {
            let suffix = match &http_identity {
                Some(http) if http.user_id == ws_identity.user_id => " (matches /whoami)",
                Some(_) => " (DIFFERENT user_id from /whoami!)",
                None => "",
            };
            println!(
                "  ✓ Hello received: user_id={}{}\n",
                ws_identity.user_id, suffix
            );
        }
        Err(e) => {
            println!("  ✗ {e:#}\n");
            failures.push("ws-handshake");
        }
    }

    if failures.is_empty() {
        println!("All checks passed.");
        Ok(())
    } else {
        bail!(
            "{} check(s) failed: {}",
            failures.len(),
            failures.join(", ")
        )
    }
}

fn build_ws_url(base_url: &str, slug: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    let scheme_swapped = if let Some(rest) = trimmed.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        trimmed.to_string()
    };
    format!("{scheme_swapped}/api/v1/proxy/s/{slug}/api/v1/ws")
}

async fn ws_handshake_probe(ws_url: &str) -> Result<NyxIdentity> {
    let token = read_nyxid_token().context("read NyxID access token")?;
    let mut req = ws_url
        .into_client_request()
        .with_context(|| format!("invalid ws url {ws_url}"))?;
    req.headers_mut()
        .insert("Authorization", bearer_header_value(&token)?);
    let (mut socket, _resp) = connect_async(req)
        .await
        .with_context(|| format!("WS connect {ws_url}"))?;

    // 1. Hello (server-pushed on connect).
    let identity = match recv_server_frame(&mut socket).await? {
        ServerFrame::Hello { payload } => payload.identity,
        other => bail!("expected Hello, got {other:?}"),
    };

    // 2. Round-trip Workspace.List ↔ Workspace.Listed.
    let req_id = "charon-doctor-probe".to_string();
    let req_frame = ClientFrame::WorkspaceList {
        id: req_id.clone(),
        payload: WorkspaceListPayload {
            include_archived: true,
        },
    };
    send_client_frame(&mut socket, &req_frame)
        .await
        .context("send Workspace.List")?;
    match recv_server_frame(&mut socket).await? {
        ServerFrame::WorkspaceListed { id, .. } if id == req_id => {}
        ServerFrame::WorkspaceListed { id, .. } => {
            bail!("Workspace.Listed id mismatch: sent {req_id}, got {id}");
        }
        other => bail!("expected Workspace.Listed, got {other:?}"),
    }

    let _ = socket.close(None).await;
    Ok(identity)
}

async fn recv_server_frame<S>(
    socket: &mut tokio_tungstenite::WebSocketStream<S>,
) -> Result<ServerFrame>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    loop {
        let msg = socket
            .next()
            .await
            .ok_or_else(|| anyhow!("WS closed before frame received"))?
            .context("WS recv")?;
        match msg {
            TungsteniteMessage::Text(text) => {
                return serde_json::from_str(text.as_str())
                    .with_context(|| format!("parse server frame: {text}"));
            }
            TungsteniteMessage::Ping(p) => {
                let _ = socket.send(TungsteniteMessage::Pong(p)).await;
            }
            TungsteniteMessage::Pong(_) => {}
            TungsteniteMessage::Close(_) => bail!("WS closed during recv"),
            TungsteniteMessage::Binary(_) | TungsteniteMessage::Frame(_) => continue,
        }
    }
}

async fn send_client_frame<S>(
    socket: &mut tokio_tungstenite::WebSocketStream<S>,
    frame: &ClientFrame,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let text = serde_json::to_string(frame).context("serialize client frame")?;
    socket
        .send(TungsteniteMessage::Text(text.into()))
        .await
        .context("WS send")?;
    Ok(())
}

async fn ensure_git_repo(path: &Path) -> Result<()> {
    tokio::fs::create_dir_all(path)
        .await
        .with_context(|| format!("mkdir {}", path.display()))?;
    let probe = tokio::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .await
        .context("exec `git rev-parse`")?;
    if probe.status.success() {
        return Ok(());
    }
    let init = tokio::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["init", "-q", "-b", "main"])
        .output()
        .await
        .context("exec `git init`")?;
    if !init.status.success() {
        bail!(
            "git init failed: {}",
            String::from_utf8_lossy(&init.stderr).trim()
        );
    }
    let commit = tokio::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .args([
            "-c",
            "user.email=probe@charon",
            "-c",
            "user.name=probe",
            "commit",
            "--allow-empty",
            "-q",
            "-m",
            "init",
        ])
        .output()
        .await
        .context("exec `git commit`")?;
    if !commit.status.success() {
        bail!(
            "git commit failed: {}",
            String::from_utf8_lossy(&commit.stderr).trim()
        );
    }
    Ok(())
}

async fn probe_terminal(base_url: &str, slug: &str, project_root: Option<PathBuf>) -> Result<()> {
    println!("== charon ws probe-terminal ==\n");

    let (project_root, _temp_guard) = match project_root {
        Some(p) => (p, None),
        None => {
            let td = tempfile::tempdir().context("create probe tempdir")?;
            (td.path().to_path_buf(), Some(td))
        }
    };
    let project_root = project_root.as_path();

    ensure_git_repo(project_root)
        .await
        .with_context(|| format!("ensure {} is a git repo", project_root.display()))?;
    println!("[1] project_root {} ready", project_root.display());

    let token = read_nyxid_token().context("read NyxID access token")?;
    let ws_url = build_ws_url(base_url, slug);
    let mut req = ws_url
        .as_str()
        .into_client_request()
        .with_context(|| format!("invalid ws url {ws_url}"))?;
    req.headers_mut()
        .insert("Authorization", bearer_header_value(&token)?);
    let (mut socket, _) = connect_async(req)
        .await
        .with_context(|| format!("WS connect {ws_url}"))?;
    println!("[2] WS connected to {ws_url}");

    let identity = match recv_server_frame(&mut socket).await? {
        ServerFrame::Hello { payload } => payload.identity,
        other => bail!("expected Hello, got {other:?}"),
    };
    println!("[3] Hello user_id={}", identity.user_id);

    send_client_frame(
        &mut socket,
        &ClientFrame::WorkspaceCreate {
            id: "probe-ws-create".into(),
            payload: CreateWorkspaceRequest {
                project_root: project_root.to_path_buf(),
                title: Some("probe-terminal".into()),
                base_branch: None,
                new_branch: None,
            },
        },
    )
    .await?;
    let workspace = loop {
        match recv_server_frame(&mut socket).await? {
            ServerFrame::WorkspaceCreated { result, .. } => break result,
            ServerFrame::Error { error, .. } => {
                bail!("WorkspaceCreate failed: {} {}", error.code, error.message);
            }
            _ => continue,
        }
    };
    println!(
        "[4] Workspace.Created id={} branch={}",
        workspace.id, workspace.branch
    );

    send_client_frame(
        &mut socket,
        &ClientFrame::TerminalCreate {
            id: "probe-term-create".into(),
            payload: CreateTerminalPayload {
                workspace_id: workspace.id.clone(),
                command: Some("/bin/sh".into()),
                env: Default::default(),
                cols: 80,
                rows: 24,
            },
        },
    )
    .await?;
    let terminal = loop {
        match recv_server_frame(&mut socket).await? {
            ServerFrame::TerminalCreated { result, .. } => break result,
            ServerFrame::Error { error, .. } => {
                bail!("TerminalCreate failed: {} {}", error.code, error.message);
            }
            _ => continue,
        }
    };
    println!("[5] Terminal.Created id={}", terminal.id);

    let cmd = "echo M24_SMOKE_OK; exit\n";
    send_client_frame(
        &mut socket,
        &ClientFrame::TerminalSendKeys {
            id: "probe-term-keys".into(),
            payload: TerminalSendKeysPayload {
                terminal_id: terminal.id.clone(),
                data_b64: BASE64.encode(cmd.as_bytes()),
            },
        },
    )
    .await?;
    println!("[6] sent {} bytes to PTY stdin", cmd.len());

    let mut accumulated: Vec<u8> = Vec::new();
    let mut saw_magic = false;
    let mut exited_code: Option<Option<i32>> = None;
    let deadline = Instant::now() + Duration::from_secs(5);
    while exited_code.is_none() && Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let frame = match tokio::time::timeout(remaining, recv_server_frame(&mut socket)).await {
            Ok(Ok(f)) => f,
            Ok(Err(e)) => return Err(e.context("recv during terminal output")),
            Err(_) => break,
        };
        match frame {
            ServerFrame::TerminalOutput {
                terminal_id,
                data_b64,
                ..
            } if terminal_id == terminal.id => {
                let bytes = BASE64
                    .decode(data_b64.as_bytes())
                    .context("decode output")?;
                accumulated.extend_from_slice(&bytes);
                if !saw_magic && String::from_utf8_lossy(&accumulated).contains("M24_SMOKE_OK") {
                    saw_magic = true;
                    println!("[7] saw M24_SMOKE_OK in PTY output");
                }
            }
            ServerFrame::TerminalExited {
                terminal_id,
                exit_code,
            } if terminal_id == terminal.id => {
                exited_code = Some(exit_code);
                println!("[8] Terminal.Exited exit_code={:?}", exit_code);
            }
            ServerFrame::Error { error, .. } => {
                bail!("WS error during stream: {} {}", error.code, error.message);
            }
            _ => {}
        }
    }

    if !saw_magic {
        bail!(
            "never saw M24_SMOKE_OK in {} bytes of output",
            accumulated.len()
        );
    }
    if exited_code.is_none() {
        send_client_frame(
            &mut socket,
            &ClientFrame::TerminalKill {
                id: "probe-term-kill".into(),
                payload: TerminalIdPayload {
                    terminal_id: terminal.id.clone(),
                },
            },
        )
        .await?;
        println!("(timeout: sent Terminal.Kill as fallback)");
    }

    send_client_frame(
        &mut socket,
        &ClientFrame::WorkspaceArchive {
            id: "probe-ws-archive".into(),
            payload: WorkspaceIdPayload {
                workspace_id: workspace.id.clone(),
            },
        },
    )
    .await?;
    // best-effort: drain a frame so the daemon log isn't surprised
    let _ = tokio::time::timeout(Duration::from_secs(2), recv_server_frame(&mut socket)).await;
    println!("[9] workspace archived (cleanup)");

    let _ = socket.close(None).await;
    println!("\nAll probe-terminal checks passed.");
    Ok(())
}

async fn local_health(endpoint: &str) -> Result<String> {
    let url = format!("{}/api/v1/health", endpoint.trim_end_matches('/'));
    let resp = reqwest::get(&url)
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("HTTP {status}: {body}");
    }
    let parsed: HealthResponse =
        serde_json::from_str(&body).with_context(|| format!("parse HealthResponse from {body}"))?;
    if !parsed.ok {
        bail!("daemon reported ok=false");
    }
    Ok(parsed.version)
}

fn nyxid_node_daemon_pid() -> Result<String> {
    let out = Command::new("nyxid")
        .args(["node", "daemon", "status"])
        .output()
        .context("exec `nyxid node daemon status`")?;
    if !out.status.success() {
        bail!(
            "nyxid CLI exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    for line in stdout.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Running:") {
            let rest = rest.trim();
            if !rest.starts_with("yes") {
                bail!("node daemon installed but reports: '{rest}'");
            }
            let pid = rest
                .split_once('(')
                .and_then(|(_, after)| after.strip_suffix(')'))
                .and_then(|s| s.strip_prefix("PID "))
                .unwrap_or("?");
            return Ok(pid.to_string());
        }
    }
    bail!("could not parse `nyxid node daemon status` output")
}

async fn e2e_whoami(url: &str) -> Result<NyxIdentity> {
    let token = read_nyxid_token().context("read NyxID access token")?;
    let resp = reqwest::Client::new()
        .get(url)
        .header(reqwest::header::AUTHORIZATION, bearer_header_value(&token)?)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("HTTP {status}: {body}");
    }
    let parsed: WhoAmIResponse =
        serde_json::from_str(&body).with_context(|| format!("parse WhoAmIResponse from {body}"))?;
    Ok(parsed.identity)
}

fn bearer_header_value(token: &NyxidToken) -> Result<HeaderValue> {
    let mut value = Zeroizing::new(String::with_capacity(
        "Bearer ".len() + token.as_str().len(),
    ));
    value.push_str("Bearer ");
    value.push_str(token.as_str());
    value.as_str().parse().context("Bearer header parse")
}

fn read_nyxid_token() -> Result<NyxidToken> {
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME env var not set"))?;
    read_nyxid_token_from_home(&PathBuf::from(home))
}

fn read_nyxid_token_from_home(home: &Path) -> Result<NyxidToken> {
    let path = home.join(".nyxid").join("access_token");
    read_nyxid_token_from_path(&path)
}

fn read_nyxid_token_from_path(path: &Path) -> Result<NyxidToken> {
    ensure_private_token_file(path)?;
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    Ok(NyxidToken::new(raw.trim().to_string()))
}

#[cfg(unix)]
fn ensure_private_token_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    let mode = metadata.permissions().mode();
    if mode & 0o077 != 0 {
        bail!(
            "refusing to read {} because permissions {:03o} allow group/other access; run `chmod 0600 {}`",
            path.display(),
            mode & 0o777,
            path.display()
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_private_token_file(_path: &Path) -> Result<()> {
    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("warn,charon_cli=info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn write_token_with_mode(home: &Path, mode: u32) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let nyxid_dir = home.join(".nyxid");
        std::fs::create_dir_all(&nyxid_dir).expect("create .nyxid");
        let token_path = nyxid_dir.join("access_token");
        std::fs::write(&token_path, "secret-token\n").expect("write access_token");
        let mut permissions = std::fs::metadata(&token_path)
            .expect("stat access_token")
            .permissions();
        permissions.set_mode(mode);
        std::fs::set_permissions(&token_path, permissions).expect("chmod access_token");
        token_path
    }

    #[test]
    #[cfg(unix)]
    fn read_nyxid_token_rejects_group_or_world_accessible_file() {
        let home = tempfile::tempdir().expect("tempdir");
        write_token_with_mode(home.path(), 0o644);

        let err = match read_nyxid_token_from_home(home.path()) {
            Ok(_) => panic!("expected insecure token file to be rejected"),
            Err(err) => err,
        };
        let message = format!("{err:#}");

        assert!(message.contains("allow group/other access"));
        assert!(message.contains("chmod 0600"));
    }

    #[test]
    #[cfg(unix)]
    fn read_nyxid_token_accepts_owner_only_file() {
        let home = tempfile::tempdir().expect("tempdir");
        write_token_with_mode(home.path(), 0o600);

        let token = read_nyxid_token_from_home(home.path()).expect("read token");

        assert_eq!(token.as_str(), "secret-token");
    }

    #[test]
    fn reqwest_authorization_header_uses_bearer_header_value() {
        let token = NyxidToken::new("secret-token".to_string());
        let request = reqwest::Client::new()
            .get("http://127.0.0.1/whoami")
            .header(
                reqwest::header::AUTHORIZATION,
                bearer_header_value(&token).expect("bearer header"),
            )
            .build()
            .expect("build request");

        let authorization = request
            .headers()
            .get(reqwest::header::AUTHORIZATION)
            .expect("authorization header");

        assert_eq!(authorization, "Bearer secret-token");
    }
}
