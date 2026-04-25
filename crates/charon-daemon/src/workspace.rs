//! Workspace manager (M2.1).
//!
//! Each workspace == a git worktree under `<charon_home>/worktrees/<uuid>/`,
//! plus a JSON record in `<charon_home>/workspaces.json`. Worktree creation
//! shells out to the system `git` binary (gix's worktree story is incomplete;
//! git2 would pull in libgit2 native deps).
//!
//! Archive is soft-delete only: `archived_at` is set, the worktree stays on
//! disk. Hard delete + cascade kill of agents/terminals lands in M3.

use std::{
    collections::HashSet,
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use charon_core::{CreateWorkspaceRequest, Workspace};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};
use uuid::Uuid;

const STORE_VERSION: u32 = 1;
const STORE_FILE: &str = "workspaces.json";
const WORKTREES_DIR: &str = "worktrees";
const LOCK_FILE: &str = ".lock";

#[derive(Debug, Clone, Serialize)]
struct WorkspaceStore {
    version: u32,
    workspaces: Vec<WorkspaceRecord>,
}

impl Default for WorkspaceStore {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            workspaces: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkspaceRecord {
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    project_root: PathBuf,
    base_branch: String,
    branch: String,
    worktree_path: PathBuf,
    created_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    archived_at: Option<DateTime<Utc>>,
}

impl From<Workspace> for WorkspaceRecord {
    fn from(workspace: Workspace) -> Self {
        Self {
            id: workspace.id,
            title: workspace.title,
            project_root: workspace.project_root,
            base_branch: workspace.base_branch,
            branch: workspace.branch,
            worktree_path: workspace.worktree_path,
            created_at: workspace.created_at,
            archived_at: workspace.archived_at,
        }
    }
}

impl From<&WorkspaceRecord> for Workspace {
    fn from(record: &WorkspaceRecord) -> Self {
        Self {
            id: record.id.clone(),
            title: record.title.clone(),
            project_root: record.project_root.clone(),
            base_branch: record.base_branch.clone(),
            branch: record.branch.clone(),
            worktree_path: record.worktree_path.clone(),
            created_at: record.created_at,
            archived_at: record.archived_at,
        }
    }
}

pub struct WorkspaceManager {
    home: PathBuf,
    _lock_file: File,
    _orphaned_worktrees: Vec<PathBuf>,
    store: RwLock<WorkspaceStore>,
}

impl WorkspaceManager {
    pub async fn new(home: PathBuf) -> Result<Self> {
        tokio::fs::create_dir_all(&home)
            .await
            .with_context(|| format!("create_dir_all {}", home.display()))?;
        let lock_file = acquire_home_lock(&home)?;
        tokio::fs::create_dir_all(home.join(WORKTREES_DIR))
            .await
            .with_context(|| format!("create_dir_all {}", home.display()))?;
        let store = load_store(&home.join(STORE_FILE)).await?;
        let orphaned_worktrees = detect_orphaned_worktrees(&home.join(WORKTREES_DIR), &store)
            .await
            .with_context(|| format!("scan orphaned worktrees under {}", home.display()))?;
        if !orphaned_worktrees.is_empty() {
            warn!(
                count = orphaned_worktrees.len(),
                paths = ?orphaned_worktrees,
                "orphaned workspace worktrees detected"
            );
        }
        info!(
            home = %home.display(),
            workspaces = store.workspaces.len(),
            "WorkspaceManager loaded"
        );
        Ok(Self {
            home,
            _lock_file: lock_file,
            _orphaned_worktrees: orphaned_worktrees,
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
            project_root: project_root.clone(),
            base_branch,
            branch: new_branch,
            worktree_path,
            created_at: Utc::now(),
            archived_at: None,
        };

        let mut store = self.store.write().await;
        let mut next_store = store.clone();
        next_store
            .workspaces
            .push(WorkspaceRecord::from(workspace.clone()));
        if let Err(save_err) = save_store(&self.home.join(STORE_FILE), &next_store).await {
            match rollback_created_worktree(
                &project_root,
                &workspace.worktree_path,
                &workspace.branch,
            )
            .await
            {
                Ok(()) => {
                    return Err(save_err).with_context(|| {
                        format!(
                            "persist workspace {}; rolled back git worktree and branch",
                            workspace.id
                        )
                    });
                }
                Err(rollback_err) => {
                    return Err(save_err).with_context(|| {
                        format!(
                            "persist workspace {}; rollback failed: {rollback_err}",
                            workspace.id
                        )
                    });
                }
            }
        }
        *store = next_store;
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
            .map(Workspace::from)
            .collect()
    }

    pub async fn get(&self, id: &str) -> Option<Workspace> {
        let store = self.store.read().await;
        store
            .workspaces
            .iter()
            .find(|w| w.id == id)
            .map(Workspace::from)
    }

    pub async fn archive(&self, id: &str) -> Result<Workspace> {
        let mut store = self.store.write().await;
        let idx = store
            .workspaces
            .iter()
            .position(|w| w.id == id)
            .with_context(|| format!("workspace {id} not found"))?;
        let mut next_store = store.clone();
        let record = &mut next_store.workspaces[idx];
        if record.archived_at.is_none() {
            record.archived_at = Some(Utc::now());
        }
        let cloned = Workspace::from(&*record);
        save_store(&self.home.join(STORE_FILE), &next_store).await?;
        *store = next_store;
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
    let disk: WorkspaceStoreFile =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
    disk.into_store()
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

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum WorkspaceStoreFile {
    Versioned(VersionedWorkspaceStore),
    LegacyObject(LegacyWorkspaceStore),
    LegacyArray(Vec<WorkspaceRecord>),
}

impl WorkspaceStoreFile {
    fn into_store(self) -> Result<WorkspaceStore> {
        match self {
            Self::Versioned(store) => {
                if store.version != STORE_VERSION {
                    bail!(
                        "unsupported workspace store version {}; expected {}",
                        store.version,
                        STORE_VERSION
                    );
                }
                Ok(WorkspaceStore {
                    version: store.version,
                    workspaces: store.workspaces,
                })
            }
            Self::LegacyObject(store) => Ok(WorkspaceStore {
                version: STORE_VERSION,
                workspaces: store.workspaces,
            }),
            Self::LegacyArray(workspaces) => Ok(WorkspaceStore {
                version: STORE_VERSION,
                workspaces,
            }),
        }
    }
}

#[derive(Debug, Deserialize)]
struct VersionedWorkspaceStore {
    version: u32,
    workspaces: Vec<WorkspaceRecord>,
}

#[derive(Debug, Deserialize)]
struct LegacyWorkspaceStore {
    workspaces: Vec<WorkspaceRecord>,
}

fn acquire_home_lock(home: &Path) -> Result<File> {
    let lock_path = home.join(LOCK_FILE);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&lock_path)
        .with_context(|| format!("open {}", lock_path.display()))?;
    file.try_lock_exclusive()
        .with_context(|| format!("lock {}", lock_path.display()))?;
    Ok(file)
}

async fn detect_orphaned_worktrees(
    worktrees_dir: &Path,
    store: &WorkspaceStore,
) -> Result<Vec<PathBuf>> {
    if !worktrees_dir.exists() {
        return Ok(Vec::new());
    }

    let expected_ids: HashSet<String> = store.workspaces.iter().map(|w| w.id.clone()).collect();
    let expected_paths: HashSet<PathBuf> = store
        .workspaces
        .iter()
        .map(|w| w.worktree_path.clone())
        .collect();
    let mut orphans = Vec::new();
    let mut entries = tokio::fs::read_dir(worktrees_dir)
        .await
        .with_context(|| format!("read_dir {}", worktrees_dir.display()))?;
    while let Some(entry) = entries
        .next_entry()
        .await
        .with_context(|| format!("read_dir entry {}", worktrees_dir.display()))?
    {
        let ty = entry
            .file_type()
            .await
            .with_context(|| format!("file_type {}", entry.path().display()))?;
        if !ty.is_dir() {
            continue;
        }
        let path = entry.path();
        let file_name = entry.file_name();
        let id_is_recorded = file_name
            .to_str()
            .map(|name| expected_ids.contains(name))
            .unwrap_or(false);
        if !id_is_recorded && !expected_paths.contains(&path) {
            orphans.push(path);
        }
    }
    orphans.sort();
    Ok(orphans)
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

async fn rollback_created_worktree(
    project_root: &Path,
    worktree_path: &Path,
    branch: &str,
) -> Result<()> {
    let mut failures = Vec::new();

    if worktree_path.exists()
        && let Err(err) = git_worktree_remove(project_root, worktree_path).await
    {
        failures.push(format!("remove worktree: {err:#}"));
    }

    if let Err(err) = git_branch_delete(project_root, branch).await {
        failures.push(format!("delete branch {branch}: {err:#}"));
    }

    if failures.is_empty() {
        info!(
            branch = %branch,
            path = %worktree_path.display(),
            "rolled back workspace git artifacts"
        );
        Ok(())
    } else {
        bail!("{}", failures.join("; "))
    }
}

async fn git_worktree_remove(project_root: &Path, worktree_path: &Path) -> Result<()> {
    let out = Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(["worktree", "remove", "--force"])
        .arg(worktree_path)
        .output()
        .await
        .context("exec `git worktree remove --force`")?;
    if !out.status.success() {
        bail!(
            "git worktree remove failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

async fn git_branch_delete(project_root: &Path, branch: &str) -> Result<()> {
    let refname = format!("refs/heads/{branch}");
    let exists = Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(["show-ref", "--verify", "--quiet", &refname])
        .status()
        .await
        .context("exec `git show-ref --verify --quiet`")?;
    if !exists.success() {
        return Ok(());
    }

    let out = Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(["branch", "-D", branch])
        .output()
        .await
        .context("exec `git branch -D`")?;
    if !out.status.success() {
        bail!(
            "git branch -D failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
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

    async fn git_stdout(cwd: &Path, args: &[&str]) -> String {
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
        String::from_utf8(out.stdout).unwrap()
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
        let stored: serde_json::Value =
            serde_json::from_slice(&tokio::fs::read(charon_home.join(STORE_FILE)).await.unwrap())
                .unwrap();
        assert_eq!(stored["version"], STORE_VERSION);

        // Persistence: a fresh manager loading the same home should see the workspace.
        drop(mgr);
        let mgr2 = WorkspaceManager::new(charon_home).await.unwrap();
        let after_reload = mgr2.list(true).await;
        assert_eq!(after_reload.len(), 1);
        assert_eq!(after_reload[0].id, ws.id);
        assert!(after_reload[0].archived_at.is_some());
    }

    #[tokio::test]
    async fn migrates_legacy_workspace_store_shape() {
        let tmp = TempDir::new().unwrap();
        let project = tmp.path().join("project");
        let charon_home = tmp.path().join("charon-home");
        let worktrees = charon_home.join(WORKTREES_DIR);
        let worktree_path = worktrees.join("legacy-id");
        init_repo(&project).await;
        tokio::fs::create_dir_all(&worktree_path).await.unwrap();

        let legacy = serde_json::json!({
            "workspaces": [{
                "id": "legacy-id",
                "title": "legacy",
                "project_root": project,
                "base_branch": "main",
                "branch": "legacy/branch",
                "worktree_path": worktree_path,
                "created_at": Utc::now(),
                "archived_at": null
            }]
        });
        tokio::fs::create_dir_all(&charon_home).await.unwrap();
        tokio::fs::write(
            charon_home.join(STORE_FILE),
            serde_json::to_vec_pretty(&legacy).unwrap(),
        )
        .await
        .unwrap();

        let mgr = WorkspaceManager::new(charon_home).await.unwrap();
        let listed = mgr.list(true).await;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "legacy-id");
        assert_eq!(listed[0].branch, "legacy/branch");
    }

    #[tokio::test]
    async fn reports_orphaned_worktree_dirs_on_startup() {
        let tmp = TempDir::new().unwrap();
        let charon_home = tmp.path().join("charon-home");
        let orphan = charon_home.join(WORKTREES_DIR).join("orphan-id");
        tokio::fs::create_dir_all(&orphan).await.unwrap();

        let mgr = WorkspaceManager::new(charon_home).await.unwrap();
        assert_eq!(mgr._orphaned_worktrees, vec![orphan]);
    }

    #[tokio::test]
    async fn failed_save_rolls_back_worktree_branch_and_memory() {
        let tmp = TempDir::new().unwrap();
        let project = tmp.path().join("project");
        let charon_home = tmp.path().join("charon-home");
        init_repo(&project).await;

        let mgr = WorkspaceManager::new(charon_home.clone()).await.unwrap();
        tokio::fs::create_dir(charon_home.join("workspaces.json.tmp"))
            .await
            .unwrap();
        let err = mgr
            .create(CreateWorkspaceRequest {
                project_root: project.clone(),
                title: Some("rollback".into()),
                base_branch: Some("main".into()),
                new_branch: Some("feat/rollback".into()),
            })
            .await
            .unwrap_err();

        assert!(
            err.to_string().contains("rolled back"),
            "expected rollback context, got: {err:#}"
        );
        assert!(mgr.list(true).await.is_empty());

        let mut entries = tokio::fs::read_dir(charon_home.join(WORKTREES_DIR))
            .await
            .unwrap();
        assert!(entries.next_entry().await.unwrap().is_none());

        let branches = git_stdout(&project, &["branch", "--list", "feat/rollback"]).await;
        assert!(
            branches.trim().is_empty(),
            "branch still exists: {branches}"
        );
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
