//! Workspace diff (M2.3).
//!
//! `git diff <base_branch>` for tracked changes plus
//! `git ls-files --others --exclude-standard` for untracked files. Each
//! changed file's unified diff is fetched separately for clean per-file
//! results. M2.3 doesn't structure hunks — frontends render the unified
//! diff text directly with whatever diff viewer they like.

use std::path::Path;

use anyhow::{Context, Result, bail};
use charon_core::{DiffResponse, DiffStatus, FileDiff, Workspace};
use tokio::process::Command;

pub async fn diff(workspace: &Workspace) -> Result<DiffResponse> {
    let workdir = workspace.worktree_path.as_path();
    let base = workspace.base_branch.as_str();

    let name_status = run_git(workdir, &["diff", "--name-status", base]).await?;
    let mut changed = Vec::new();
    for line in name_status.lines() {
        let mut parts = line.splitn(2, '\t');
        let raw_status = parts.next().unwrap_or("");
        let path = parts.next().unwrap_or("").trim().to_string();
        if path.is_empty() {
            continue;
        }
        let status = parse_status(raw_status);
        let unified = run_git(workdir, &["diff", base, "--", &path]).await?;
        changed.push(FileDiff {
            path,
            status,
            unified_diff: unified,
        });
    }

    let untracked_raw = run_git(workdir, &["ls-files", "--others", "--exclude-standard"]).await?;
    let untracked: Vec<String> = untracked_raw
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    Ok(DiffResponse {
        workspace_id: workspace.id.clone(),
        base_branch: workspace.base_branch.clone(),
        changed,
        untracked,
    })
}

async fn run_git(workdir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(workdir)
        .args(args)
        .output()
        .await
        .with_context(|| format!("exec `git {}`", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn parse_status(raw: &str) -> DiffStatus {
    match raw.chars().next() {
        Some('A') => DiffStatus::Added,
        Some('M') => DiffStatus::Modified,
        Some('D') => DiffStatus::Deleted,
        Some('R') => DiffStatus::Renamed,
        _ => DiffStatus::Other,
    }
}
