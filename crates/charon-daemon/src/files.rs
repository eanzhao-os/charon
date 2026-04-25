//! File operations within a workspace's worktree (M2.3).
//!
//! All paths are sandboxed via lexical normalization (`..` traversal beyond
//! workspace root is rejected, absolute paths are rejected). Symlink-following
//! attacks would require a malicious source repo, which is the user's problem.
//!
//! M2.3 ships UTF-8 only — non-UTF-8 read/write returns a clear error pointing
//! at M2.4 when binary attachment plumbing lands with terminals.

use std::collections::VecDeque;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use charon_core::{FileContent, FileEntry, FileKind, FileTreeResponse, Workspace};

pub fn safe_join(base: &Path, rel: &str) -> Result<PathBuf> {
    let mut out = base.to_path_buf();
    for comp in Path::new(rel).components() {
        match comp {
            Component::Normal(s) => out.push(s),
            Component::CurDir => continue,
            Component::ParentDir => {
                let popped = out.pop();
                if !popped || !out.starts_with(base) {
                    bail!("path '{}' escapes workspace", rel);
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                bail!("path '{}' must be relative", rel);
            }
        }
    }
    Ok(out)
}

pub async fn tree(workspace: &Workspace, rel: &str, depth: u32) -> Result<FileTreeResponse> {
    let abs = safe_join(&workspace.worktree_path, rel)?;
    let metadata = tokio::fs::metadata(&abs)
        .await
        .with_context(|| format!("stat {}", abs.display()))?;
    if !metadata.is_dir() {
        bail!("{} is not a directory", rel);
    }
    let mut entries = Vec::new();
    walk(&abs, &workspace.worktree_path, depth, &mut entries).await?;
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(FileTreeResponse {
        workspace_id: workspace.id.clone(),
        path: rel.to_string(),
        entries,
    })
}

async fn walk(start: &Path, base: &Path, max_depth: u32, out: &mut Vec<FileEntry>) -> Result<()> {
    let mut queue: VecDeque<(PathBuf, u32)> = VecDeque::new();
    queue.push_back((start.to_path_buf(), 0));
    while let Some((dir, depth)) = queue.pop_front() {
        let mut entries = tokio::fs::read_dir(&dir)
            .await
            .with_context(|| format!("read_dir {}", dir.display()))?;
        while let Some(e) = entries.next_entry().await.context("read_dir entry")? {
            let name = e.file_name();
            // Skip git's worktree marker dir; users almost never want it in the tree.
            if name == ".git" {
                continue;
            }
            let p = e.path();
            let rel_path = p
                .strip_prefix(base)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| p.to_string_lossy().to_string());
            let ft = e.file_type().await.context("file_type")?;
            let kind = if ft.is_symlink() {
                FileKind::Symlink
            } else if ft.is_dir() {
                FileKind::Dir
            } else {
                FileKind::File
            };
            let size = if matches!(kind, FileKind::File) {
                e.metadata().await.ok().map(|m| m.len())
            } else {
                None
            };
            out.push(FileEntry {
                path: rel_path,
                kind,
                size,
            });
            if matches!(kind, FileKind::Dir) && depth + 1 < max_depth {
                queue.push_back((p, depth + 1));
            }
        }
    }
    Ok(())
}

pub async fn read(workspace: &Workspace, rel: &str) -> Result<FileContent> {
    let abs = safe_join(&workspace.worktree_path, rel)?;
    let bytes = tokio::fs::read(&abs)
        .await
        .with_context(|| format!("read {}", abs.display()))?;
    let content = String::from_utf8(bytes).map_err(|e| {
        anyhow!(
            "{} is not valid UTF-8 (first invalid byte at offset {}); binary read lands in M2.4",
            rel,
            e.utf8_error().valid_up_to()
        )
    })?;
    Ok(FileContent {
        workspace_id: workspace.id.clone(),
        path: rel.to_string(),
        content,
    })
}

pub async fn write(workspace: &Workspace, rel: &str, content: &str) -> Result<FileContent> {
    let abs = safe_join(&workspace.worktree_path, rel)?;
    if let Some(parent) = abs.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("create_dir_all {}", parent.display()))?;
    }
    tokio::fs::write(&abs, content)
        .await
        .with_context(|| format!("write {}", abs.display()))?;
    Ok(FileContent {
        workspace_id: workspace.id.clone(),
        path: rel.to_string(),
        content: content.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use tempfile::TempDir;

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

    #[test]
    fn safe_join_rejects_traversal() {
        let base = Path::new("/tmp/ws");
        assert!(safe_join(base, "src/main.rs").is_ok());
        assert!(safe_join(base, "./src/main.rs").is_ok());
        assert!(safe_join(base, "src/../main.rs").is_ok());
        assert!(safe_join(base, "../escape").is_err());
        assert!(safe_join(base, "src/../../escape").is_err());
        assert!(safe_join(base, "/etc/passwd").is_err());
    }

    #[tokio::test]
    async fn tree_lists_files_and_dirs_skipping_dotgit() {
        let tmp = TempDir::new().unwrap();
        let ws = fake_workspace(tmp.path().to_path_buf());
        tokio::fs::create_dir_all(tmp.path().join("src"))
            .await
            .unwrap();
        tokio::fs::create_dir_all(tmp.path().join(".git"))
            .await
            .unwrap();
        tokio::fs::write(tmp.path().join("README.md"), "hi")
            .await
            .unwrap();
        tokio::fs::write(tmp.path().join("src/main.rs"), "fn main(){}")
            .await
            .unwrap();
        tokio::fs::write(tmp.path().join(".git/HEAD"), "ref")
            .await
            .unwrap();

        let resp = tree(&ws, "", 5).await.unwrap();
        let paths: Vec<_> = resp.entries.iter().map(|e| e.path.clone()).collect();
        assert!(paths.contains(&"README.md".to_string()));
        assert!(paths.contains(&"src".to_string()));
        assert!(paths.contains(&"src/main.rs".to_string()));
        assert!(!paths.iter().any(|p| p.starts_with(".git")));
    }

    #[tokio::test]
    async fn tree_respects_depth() {
        let tmp = TempDir::new().unwrap();
        let ws = fake_workspace(tmp.path().to_path_buf());
        tokio::fs::create_dir_all(tmp.path().join("a/b/c"))
            .await
            .unwrap();
        tokio::fs::write(tmp.path().join("a/b/c/deep.txt"), "x")
            .await
            .unwrap();

        let depth1 = tree(&ws, "", 1).await.unwrap();
        let paths1: Vec<_> = depth1.entries.iter().map(|e| e.path.clone()).collect();
        assert!(paths1.contains(&"a".to_string()));
        assert!(!paths1.iter().any(|p| p.contains('/')));

        let depth5 = tree(&ws, "", 5).await.unwrap();
        let paths5: Vec<_> = depth5.entries.iter().map(|e| e.path.clone()).collect();
        assert!(paths5.contains(&"a/b/c/deep.txt".to_string()));
    }

    #[tokio::test]
    async fn read_and_write_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let ws = fake_workspace(tmp.path().to_path_buf());

        let written = write(&ws, "src/foo.rs", "fn foo(){}").await.unwrap();
        assert_eq!(written.path, "src/foo.rs");
        assert_eq!(written.content, "fn foo(){}");
        assert!(tmp.path().join("src/foo.rs").exists());

        let read_back = read(&ws, "src/foo.rs").await.unwrap();
        assert_eq!(read_back.content, "fn foo(){}");
    }

    #[tokio::test]
    async fn read_rejects_non_utf8() {
        let tmp = TempDir::new().unwrap();
        let ws = fake_workspace(tmp.path().to_path_buf());
        tokio::fs::write(tmp.path().join("blob.bin"), [0xff, 0xfe, 0xfd])
            .await
            .unwrap();
        let err = read(&ws, "blob.bin").await.unwrap_err();
        assert!(err.to_string().contains("not valid UTF-8"));
    }

    #[tokio::test]
    async fn write_rejects_path_escape() {
        let tmp = TempDir::new().unwrap();
        let ws = fake_workspace(tmp.path().to_path_buf());
        let err = write(&ws, "../escape.txt", "x").await.unwrap_err();
        assert!(err.to_string().contains("escapes workspace"));
    }
}
