//! Workspace manager (M2.1).
//!
//! Each workspace == a git worktree under `<charon_home>/worktrees/<uuid>/`,
//! plus a JSON record in `<charon_home>/workspaces.json`. Worktree creation
//! shells out to the system `git` binary (gix's worktree story is incomplete;
//! git2 would pull in libgit2 native deps).
//!
//! Archive is soft-delete only: `archived_at` is set, the worktree stays on
//! disk. Hard delete + cascade kill of agents/terminals lands in M3.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use charon_core::{CreateWorkspaceRequest, Workspace};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio::sync::RwLock;
use tracing::{debug, info};
use uuid::Uuid;

const STORE_FILE: &str = "workspaces.json";
const WORKTREES_DIR: &str = "worktrees";

#[derive(Debug, Default, Serialize, Deserialize)]
struct WorkspaceStore {
    workspaces: Vec<Workspace>,
}

pub struct WorkspaceManager {
    home: PathBuf,
    store: RwLock<WorkspaceStore>,
}

impl WorkspaceManager {
    pub async fn new(home: PathBuf) -> Result<Self> {
        tokio::fs::create_dir_all(home.join(WORKTREES_DIR))
            .await
            .with_context(|| format!("create_dir_all {}", home.display()))?;
        let store = load_store(&home.join(STORE_FILE)).await?;
        info!(
            home = %home.display(),
            workspaces = store.workspaces.len(),
            "WorkspaceManager loaded"
        );
        Ok(Self {
            home,
            store: RwLock::new(store),
        })
    }

    pub async fn create(&self, req: CreateWorkspaceRequest) -> Result<Workspace> {
        if !req.project_root.is_absolute() {
            bail!("project_root must be an absolute path");
        }
        let project_root = tokio::fs::canonicalize(&req.project_root)
            .await
            .with_context(|| format!("canonicalize {}", req.project_root.display()))?;
        ensure_git_repo(&project_root).await?;

        let base_branch = match req.base_branch {
            Some(b) => b,
            None => detect_head_branch(&project_root).await?,
        };

        let id = Uuid::new_v4().to_string();
        let worktree_path = self.home.join(WORKTREES_DIR).join(&id);
        let new_branch = req
            .new_branch
            .unwrap_or_else(|| format!("charon/{}", &id[..8]));

        git_worktree_add(&project_root, &worktree_path, &new_branch, &base_branch).await?;

        let workspace = Workspace {
            id,
            title: req.title,
            project_root,
            base_branch,
            branch: new_branch,
            worktree_path,
            created_at: Utc::now(),
            archived_at: None,
        };

        let mut store = self.store.write().await;
        store.workspaces.push(workspace.clone());
        save_store(&self.home.join(STORE_FILE), &store).await?;
        info!(
            id = %workspace.id,
            branch = %workspace.branch,
            path = %workspace.worktree_path.display(),
            "workspace created"
        );
        Ok(workspace)
    }

    pub async fn list(&self, include_archived: bool) -> Vec<Workspace> {
        let store = self.store.read().await;
        store
            .workspaces
            .iter()
            .filter(|w| include_archived || w.archived_at.is_none())
            .cloned()
            .collect()
    }

    pub async fn get(&self, id: &str) -> Option<Workspace> {
        let store = self.store.read().await;
        store.workspaces.iter().find(|w| w.id == id).cloned()
    }

    pub async fn archive(&self, id: &str) -> Result<Workspace> {
        let mut store = self.store.write().await;
        let workspace = store
            .workspaces
            .iter_mut()
            .find(|w| w.id == id)
            .with_context(|| format!("workspace {id} not found"))?;
        if workspace.archived_at.is_none() {
            workspace.archived_at = Some(Utc::now());
        }
        let cloned = workspace.clone();
        save_store(&self.home.join(STORE_FILE), &store).await?;
        info!(id = %cloned.id, "workspace archived");
        Ok(cloned)
    }
}

async fn load_store(path: &Path) -> Result<WorkspaceStore> {
    if !path.exists() {
        return Ok(WorkspaceStore::default());
    }
    let bytes = tokio::fs::read(path)
        .await
        .with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
}

async fn save_store(path: &Path, store: &WorkspaceStore) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(store)?;
    let tmp = path.with_extension("json.tmp");
    tokio::fs::write(&tmp, bytes)
        .await
        .with_context(|| format!("write {}", tmp.display()))?;
    tokio::fs::rename(&tmp, path)
        .await
        .with_context(|| format!("rename {} → {}", tmp.display(), path.display()))?;
    Ok(())
}

async fn ensure_git_repo(path: &Path) -> Result<()> {
    let out = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .await
        .context("exec `git rev-parse --is-inside-work-tree`")?;
    if !out.status.success() {
        bail!(
            "{} is not a git working tree: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

async fn detect_head_branch(project_root: &Path) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .await
        .context("exec `git rev-parse --abbrev-ref HEAD`")?;
    if !out.status.success() {
        bail!(
            "could not detect HEAD branch: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let branch = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if branch == "HEAD" {
        bail!("project_root has detached HEAD; specify base_branch explicitly");
    }
    Ok(branch)
}

async fn git_worktree_add(
    project_root: &Path,
    worktree_path: &Path,
    new_branch: &str,
    base_branch: &str,
) -> Result<()> {
    if worktree_path.exists() {
        bail!("worktree path {} already exists", worktree_path.display());
    }
    let out = Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(["worktree", "add", "-b", new_branch])
        .arg(worktree_path)
        .arg(base_branch)
        .output()
        .await
        .context("exec `git worktree add`")?;
    if !out.status.success() {
        bail!(
            "git worktree add failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    debug!(
        stderr = %String::from_utf8_lossy(&out.stderr).trim(),
        "git worktree add ok"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Initialise a tiny git repo with one empty commit on `main`.
    async fn init_repo(path: &Path) {
        tokio::fs::create_dir_all(path).await.unwrap();
        run(path, &["init", "-q", "-b", "main"]).await;
        run(path, &["config", "user.email", "test@charon"]).await;
        run(path, &["config", "user.name", "test"]).await;
        run(path, &["commit", "--allow-empty", "-q", "-m", "init"]).await;
    }

    async fn run(cwd: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .await
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[tokio::test]
    async fn create_list_get_archive_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let project = tmp.path().join("project");
        let charon_home = tmp.path().join("charon-home");
        init_repo(&project).await;

        let mgr = WorkspaceManager::new(charon_home.clone()).await.unwrap();
        let ws = mgr
            .create(CreateWorkspaceRequest {
                project_root: project.clone(),
                title: Some("ws-1".into()),
                base_branch: Some("main".into()),
                new_branch: Some("feat/test".into()),
            })
            .await
            .unwrap();

        assert!(ws.worktree_path.exists());
        assert_eq!(ws.branch, "feat/test");
        assert_eq!(ws.base_branch, "main");
        assert!(ws.archived_at.is_none());

        let listed = mgr.list(false).await;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, ws.id);

        let fetched = mgr.get(&ws.id).await.unwrap();
        assert_eq!(fetched.id, ws.id);

        let archived = mgr.archive(&ws.id).await.unwrap();
        assert!(archived.archived_at.is_some());
        assert!(mgr.list(false).await.is_empty());
        assert_eq!(mgr.list(true).await.len(), 1);

        // Persistence: a fresh manager loading the same home should see the workspace.
        drop(mgr);
        let mgr2 = WorkspaceManager::new(charon_home).await.unwrap();
        let after_reload = mgr2.list(true).await;
        assert_eq!(after_reload.len(), 1);
        assert_eq!(after_reload[0].id, ws.id);
        assert!(after_reload[0].archived_at.is_some());
    }

    #[tokio::test]
    async fn rejects_relative_project_root() {
        let tmp = TempDir::new().unwrap();
        let mgr = WorkspaceManager::new(tmp.path().join("home"))
            .await
            .unwrap();
        let err = mgr
            .create(CreateWorkspaceRequest {
                project_root: PathBuf::from("./not-absolute"),
                title: None,
                base_branch: None,
                new_branch: None,
            })
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("absolute"),
            "expected absolute-path error, got: {err}"
        );
    }

    #[tokio::test]
    async fn rejects_non_git_dir() {
        let tmp = TempDir::new().unwrap();
        let project = tmp.path().join("not-a-repo");
        tokio::fs::create_dir_all(&project).await.unwrap();
        let mgr = WorkspaceManager::new(tmp.path().join("home"))
            .await
            .unwrap();
        let err = mgr
            .create(CreateWorkspaceRequest {
                project_root: project,
                title: None,
                base_branch: None,
                new_branch: None,
            })
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("not a git working tree"),
            "expected git-repo error, got: {err}"
        );
    }
}
