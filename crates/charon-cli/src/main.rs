use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use charon_core::{
    DEFAULT_DAEMON_BIND, DEFAULT_NYXID_ISSUER, DEFAULT_USER_SERVICE_SLUG, HealthResponse,
    NyxIdentity, WhoAmIResponse,
};
use charon_daemon::{DEFAULT_EXPECTED_AUD, DaemonConfig, default_home};
use clap::{Parser, Subcommand};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

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
                    home,
                },
        } => start_daemon(bind, expected_aud, nyxid_issuer, home).await,
        Cmd::Daemon {
            sub: DaemonCmd::Status { endpoint },
        } => status(&endpoint).await,
        Cmd::Doctor {
            endpoint,
            slug,
            base_url,
        } => doctor(&endpoint, &slug, &base_url).await,
    }
}

async fn start_daemon(
    bind: String,
    expected_aud: String,
    nyxid_issuer: String,
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
    println!("[3] end-to-end via NyxID proxy");
    println!("    GET {proxy_url}");
    match e2e_whoami(&proxy_url).await {
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
        }
        Err(e) => {
            println!("  ✗ {e:#}\n");
            failures.push("end-to-end");
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
        .bearer_auth(&token)
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

fn read_nyxid_token() -> Result<String> {
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME env var not set"))?;
    let path = PathBuf::from(home).join(".nyxid").join("access_token");
    let raw = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    Ok(raw.trim().to_string())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("warn,charon_cli=info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}
