//! File operations within a workspace's worktree (M2.3).
//!
//! All paths are sandboxed via lexical normalization (`..` traversal beyond
//! workspace root is rejected, absolute paths are rejected). Read/write opens
//! are performed relative to a held workspace-root fd without following
//! symlinks, so the final syscall cannot race against a symlink swap.
//!
//! M2.3 ships UTF-8 only — non-UTF-8 read/write returns a clear error pointing
//! at M2.4 when binary attachment plumbing lands with terminals.

use std::collections::VecDeque;
#[cfg(not(unix))]
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};
#[cfg(unix)]
use std::{ffi::OsString, fs::File, io::Read, io::Write};

use anyhow::{Context, Result, anyhow, bail};
use charon_core::{FileContent, FileEntry, FileKind, FileTreeResponse, Workspace};
#[cfg(target_os = "linux")]
use rustix::fs::{ResolveFlags, openat2};
#[cfg(unix)]
use rustix::{
    fd::{AsFd, BorrowedFd, OwnedFd},
    fs::{CWD, Mode, OFlags, mkdirat, openat},
    io::Errno,
};

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

async fn canonical_base(base: &Path) -> Result<PathBuf> {
    tokio::fs::canonicalize(base)
        .await
        .with_context(|| format!("canonicalize workspace {}", base.display()))
}

fn ensure_in_workspace(canonical_base: &Path, canonical_path: &Path, rel: &str) -> Result<()> {
    if canonical_path.starts_with(canonical_base) {
        Ok(())
    } else {
        bail!("path '{}' escapes workspace", rel);
    }
}

async fn resolve_existing_path(base: &Path, rel: &str) -> Result<PathBuf> {
    let abs = safe_join(base, rel)?;
    let canonical_base = canonical_base(base).await?;
    let canonical_path = tokio::fs::canonicalize(&abs)
        .await
        .with_context(|| format!("canonicalize {}", abs.display()))?;
    ensure_in_workspace(&canonical_base, &canonical_path, rel)?;
    Ok(abs)
}

#[cfg(not(unix))]
async fn ensure_creatable_path(
    base: &Path,
    canonical_base: &Path,
    abs: &Path,
    rel: &str,
) -> Result<()> {
    let parent = abs
        .parent()
        .with_context(|| format!("path '{}' has no parent", rel))?;
    let mut ancestor = parent.to_path_buf();
    loop {
        if !ancestor.starts_with(base) {
            bail!("path '{}' escapes workspace", rel);
        }
        match tokio::fs::symlink_metadata(&ancestor).await {
            Ok(_) => {
                let canonical_ancestor = tokio::fs::canonicalize(&ancestor)
                    .await
                    .with_context(|| format!("canonicalize {}", ancestor.display()))?;
                ensure_in_workspace(canonical_base, &canonical_ancestor, rel)?;
                return Ok(());
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {
                if !ancestor.pop() {
                    bail!("path '{}' escapes workspace", rel);
                }
            }
            Err(e) => {
                return Err(e).with_context(|| format!("stat {}", ancestor.display()));
            }
        }
    }
}

#[cfg(not(unix))]
async fn resolve_write_path(base: &Path, rel: &str) -> Result<(PathBuf, PathBuf)> {
    let abs = safe_join(base, rel)?;
    let canonical_base = canonical_base(base).await?;
    match tokio::fs::canonicalize(&abs).await {
        Ok(canonical_path) => ensure_in_workspace(&canonical_base, &canonical_path, rel)?,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            ensure_creatable_path(base, &canonical_base, &abs, rel).await?;
        }
        Err(e) => {
            return Err(e).with_context(|| format!("canonicalize {}", abs.display()));
        }
    }
    Ok((abs, canonical_base))
}

#[cfg(unix)]
struct SecureRelPath {
    components: Vec<OsString>,
    path: PathBuf,
}

#[cfg(unix)]
fn secure_relative_path(rel: &str) -> Result<SecureRelPath> {
    let mut components = Vec::new();
    for comp in Path::new(rel).components() {
        match comp {
            Component::Normal(s) => components.push(s.to_os_string()),
            Component::CurDir => continue,
            Component::ParentDir => {
                if components.pop().is_none() {
                    bail!("path '{}' escapes workspace", rel);
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                bail!("path '{}' must be relative", rel);
            }
        }
    }

    if components.is_empty() {
        bail!("path '{}' must name a file", rel);
    }

    let mut path = PathBuf::new();
    for component in &components {
        path.push(component);
    }

    Ok(SecureRelPath { components, path })
}

#[cfg(unix)]
fn component_path(component: &OsString) -> &Path {
    Path::new(component.as_os_str())
}

#[cfg(unix)]
fn file_mode() -> Mode {
    Mode::RUSR | Mode::WUSR | Mode::RGRP | Mode::WGRP | Mode::ROTH | Mode::WOTH
}

#[cfg(unix)]
fn dir_mode() -> Mode {
    Mode::RWXU | Mode::RWXG | Mode::RWXO
}

#[cfg(unix)]
fn dir_open_flags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW
}

#[cfg(unix)]
fn secure_path_error(err: Errno, rel: &str, action: String) -> anyhow::Error {
    if err == Errno::LOOP || err == Errno::NOTDIR {
        anyhow!("path '{}' escapes workspace or contains symlink", rel)
    } else {
        anyhow::Error::new(std::io::Error::from(err)).context(action)
    }
}

#[cfg(unix)]
fn open_workspace_root(base: &Path) -> Result<OwnedFd> {
    openat(CWD, base, dir_open_flags(), Mode::empty()).map_err(|err| {
        anyhow::Error::new(std::io::Error::from(err))
            .context(format!("open workspace {}", base.display()))
    })
}

#[cfg(unix)]
fn try_open_dir_component(
    parent: BorrowedFd<'_>,
    component: &OsString,
) -> rustix::io::Result<OwnedFd> {
    openat(
        parent,
        component_path(component),
        dir_open_flags(),
        Mode::empty(),
    )
}

#[cfg(unix)]
fn open_dir_component(parent: BorrowedFd<'_>, component: &OsString, rel: &str) -> Result<OwnedFd> {
    try_open_dir_component(parent, component).map_err(|err| {
        secure_path_error(
            err,
            rel,
            format!(
                "open directory component {}",
                component_path(component).display()
            ),
        )
    })
}

#[cfg(unix)]
fn open_or_create_dir(parent: BorrowedFd<'_>, component: &OsString, rel: &str) -> Result<OwnedFd> {
    match try_open_dir_component(parent, component) {
        Ok(fd) => Ok(fd),
        Err(Errno::NOENT) => match mkdirat(parent, component_path(component), dir_mode()) {
            Ok(()) | Err(Errno::EXIST) => open_dir_component(parent, component, rel),
            Err(err) => Err(secure_path_error(
                err,
                rel,
                format!("mkdir {}", component_path(component).display()),
            )),
        },
        Err(err) => Err(secure_path_error(
            err,
            rel,
            format!(
                "open directory component {}",
                component_path(component).display()
            ),
        )),
    }
}

#[cfg(unix)]
fn ensure_parent_dirs_no_symlinks(
    root: &OwnedFd,
    components: &[OsString],
    rel: &str,
) -> Result<()> {
    let mut current: Option<OwnedFd> = None;
    for component in components.iter().take(components.len().saturating_sub(1)) {
        let parent = current
            .as_ref()
            .map_or_else(|| root.as_fd(), |fd| fd.as_fd());
        current = Some(open_or_create_dir(parent, component, rel)?);
    }
    Ok(())
}

#[cfg(unix)]
fn open_parent_dir(root: &OwnedFd, components: &[OsString], rel: &str) -> Result<Option<OwnedFd>> {
    let mut current: Option<OwnedFd> = None;
    for component in components.iter().take(components.len().saturating_sub(1)) {
        let parent = current
            .as_ref()
            .map_or_else(|| root.as_fd(), |fd| fd.as_fd());
        current = Some(open_dir_component(parent, component, rel)?);
    }
    Ok(current)
}

#[cfg(target_os = "linux")]
fn open_read_no_symlinks(root: &OwnedFd, rel_path: &SecureRelPath, rel: &str) -> Result<OwnedFd> {
    openat2(
        root,
        &rel_path.path,
        OFlags::RDONLY | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::NO_SYMLINKS | ResolveFlags::BENEATH,
    )
    .map_err(|err| secure_path_error(err, rel, format!("open {}", rel_path.path.display())))
}

#[cfg(all(unix, not(target_os = "linux")))]
fn open_read_no_symlinks(root: &OwnedFd, rel_path: &SecureRelPath, rel: &str) -> Result<OwnedFd> {
    let parent = open_parent_dir(root, &rel_path.components, rel)?;
    let parent = parent
        .as_ref()
        .map_or_else(|| root.as_fd(), |fd| fd.as_fd());
    let file_name = rel_path.components.last().expect("non-empty secure path");
    openat(
        parent,
        component_path(file_name),
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|err| secure_path_error(err, rel, format!("open {}", rel_path.path.display())))
}

#[cfg(target_os = "linux")]
fn open_write_no_symlinks(root: &OwnedFd, rel_path: &SecureRelPath, rel: &str) -> Result<OwnedFd> {
    openat2(
        root,
        &rel_path.path,
        OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC | OFlags::CLOEXEC,
        file_mode(),
        ResolveFlags::NO_SYMLINKS | ResolveFlags::BENEATH,
    )
    .map_err(|err| secure_path_error(err, rel, format!("open {}", rel_path.path.display())))
}

#[cfg(all(unix, not(target_os = "linux")))]
fn open_write_no_symlinks(root: &OwnedFd, rel_path: &SecureRelPath, rel: &str) -> Result<OwnedFd> {
    let parent = open_parent_dir(root, &rel_path.components, rel)?;
    let parent = parent
        .as_ref()
        .map_or_else(|| root.as_fd(), |fd| fd.as_fd());
    let file_name = rel_path.components.last().expect("non-empty secure path");
    openat(
        parent,
        component_path(file_name),
        OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        file_mode(),
    )
    .map_err(|err| secure_path_error(err, rel, format!("open {}", rel_path.path.display())))
}

#[cfg(unix)]
fn read_file_no_symlinks_blocking(
    base: &Path,
    rel_path: SecureRelPath,
    rel: &str,
) -> Result<Vec<u8>> {
    let root = open_workspace_root(base)?;
    let fd = open_read_no_symlinks(&root, &rel_path, rel)?;
    let mut file = File::from(fd);
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .with_context(|| format!("read {}", rel_path.path.display()))?;
    Ok(bytes)
}

#[cfg(unix)]
async fn read_file_no_symlinks(base: &Path, rel: &str) -> Result<Vec<u8>> {
    let rel_path = secure_relative_path(rel)?;
    let base = base.to_path_buf();
    let rel = rel.to_string();
    tokio::task::spawn_blocking(move || read_file_no_symlinks_blocking(&base, rel_path, &rel))
        .await
        .context("join no-symlink read")?
}

#[cfg(unix)]
fn write_file_no_symlinks_blocking<F>(
    base: &Path,
    rel_path: SecureRelPath,
    rel: &str,
    content: &str,
    before_open: F,
) -> Result<()>
where
    F: FnOnce(),
{
    let root = open_workspace_root(base)?;
    ensure_parent_dirs_no_symlinks(&root, &rel_path.components, rel)?;
    before_open();
    let fd = open_write_no_symlinks(&root, &rel_path, rel)?;
    let mut file = File::from(fd);
    file.write_all(content.as_bytes())
        .with_context(|| format!("write {}", rel_path.path.display()))?;
    Ok(())
}

#[cfg(unix)]
async fn write_file_no_symlinks_with_hook<F>(
    base: &Path,
    rel: &str,
    content: &str,
    before_open: F,
) -> Result<()>
where
    F: FnOnce() + Send + 'static,
{
    let rel_path = secure_relative_path(rel)?;
    let base = base.to_path_buf();
    let rel = rel.to_string();
    let content = content.to_string();
    tokio::task::spawn_blocking(move || {
        write_file_no_symlinks_blocking(&base, rel_path, &rel, &content, before_open)
    })
    .await
    .context("join no-symlink write")?
}

#[cfg(unix)]
async fn write_file_no_symlinks(base: &Path, rel: &str, content: &str) -> Result<()> {
    write_file_no_symlinks_with_hook(base, rel, content, || {}).await
}

#[cfg(not(unix))]
async fn read_file_no_symlinks(base: &Path, rel: &str) -> Result<Vec<u8>> {
    let abs = resolve_existing_path(base, rel).await?;
    tokio::fs::read(&abs)
        .await
        .with_context(|| format!("read {}", abs.display()))
}

#[cfg(not(unix))]
async fn write_file_no_symlinks(base: &Path, rel: &str, content: &str) -> Result<()> {
    let (abs, canonical_base) = resolve_write_path(base, rel).await?;
    if let Some(parent) = abs.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("create_dir_all {}", parent.display()))?;
        let canonical_parent = tokio::fs::canonicalize(parent)
            .await
            .with_context(|| format!("canonicalize {}", parent.display()))?;
        ensure_in_workspace(&canonical_base, &canonical_parent, rel)?;
    }
    tokio::fs::write(&abs, content)
        .await
        .with_context(|| format!("write {}", abs.display()))?;
    let canonical_path = tokio::fs::canonicalize(&abs)
        .await
        .with_context(|| format!("canonicalize {}", abs.display()))?;
    ensure_in_workspace(&canonical_base, &canonical_path, rel)
}

pub async fn tree(workspace: &Workspace, rel: &str, depth: u32) -> Result<FileTreeResponse> {
    let abs = resolve_existing_path(&workspace.worktree_path, rel).await?;
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
    let bytes = read_file_no_symlinks(&workspace.worktree_path, rel).await?;
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
    write_file_no_symlinks(&workspace.worktree_path, rel, content).await?;
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

    #[cfg(unix)]
    #[tokio::test]
    async fn tree_reports_symlinks_without_recursing() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let ws = fake_workspace(tmp.path().to_path_buf());
        tokio::fs::create_dir_all(tmp.path().join("target"))
            .await
            .unwrap();
        tokio::fs::write(tmp.path().join("target/nested.txt"), "secret")
            .await
            .unwrap();
        symlink("target", tmp.path().join("link")).unwrap();

        let resp = tree(&ws, "", 5).await.unwrap();
        let link = resp.entries.iter().find(|e| e.path == "link").unwrap();
        assert_eq!(link.kind, FileKind::Symlink);
        assert!(!resp.entries.iter().any(|e| e.path == "link/nested.txt"));

        let link_resp = tree(&ws, "link", 5).await.unwrap();
        assert!(
            link_resp
                .entries
                .iter()
                .any(|e| e.path == "link/nested.txt")
        );
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

    #[cfg(unix)]
    #[tokio::test]
    async fn read_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let ws = fake_workspace(tmp.path().to_path_buf());
        symlink("/etc/passwd", tmp.path().join("secret")).unwrap();

        let err = read(&ws, "secret").await.unwrap_err();
        assert!(err.to_string().contains("escapes workspace"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_rejects_symlink_to_workspace_path() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let ws = fake_workspace(tmp.path().to_path_buf());
        tokio::fs::create_dir_all(tmp.path().join("target"))
            .await
            .unwrap();
        tokio::fs::write(tmp.path().join("target/inside.txt"), "ok")
            .await
            .unwrap();
        symlink("target", tmp.path().join("link")).unwrap();

        let err = read(&ws, "link/inside.txt").await.unwrap_err();
        assert!(err.to_string().contains("escapes workspace"));
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

    #[cfg(unix)]
    #[tokio::test]
    async fn write_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let outside_file = outside.path().join("secret.txt");
        tokio::fs::write(&outside_file, "original").await.unwrap();
        let ws = fake_workspace(tmp.path().to_path_buf());
        symlink(&outside_file, tmp.path().join("secret")).unwrap();

        let err = write(&ws, "secret", "changed").await.unwrap_err();
        assert!(err.to_string().contains("escapes workspace"));
        let outside_content = tokio::fs::read_to_string(&outside_file).await.unwrap();
        assert_eq!(outside_content, "original");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_rejects_symlink_to_workspace_path() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let ws = fake_workspace(tmp.path().to_path_buf());
        tokio::fs::create_dir_all(tmp.path().join("target"))
            .await
            .unwrap();
        symlink("target", tmp.path().join("link")).unwrap();

        let err = write(&ws, "link/new.txt", "ok").await.unwrap_err();
        assert!(err.to_string().contains("escapes workspace"));
        assert!(!tmp.path().join("target/new.txt").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_rejects_symlink_swap_before_final_open_without_touching_outside() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let outside_file = outside.path().join("target.txt");
        tokio::fs::write(&outside_file, "original").await.unwrap();

        let ws = fake_workspace(tmp.path().to_path_buf());
        let race_dir = tmp.path().join("race");
        tokio::fs::create_dir(&race_dir).await.unwrap();

        let race_dir_for_swap = race_dir.clone();
        let outside_dir = outside.path().to_path_buf();
        let err = write_file_no_symlinks_with_hook(
            &ws.worktree_path,
            "race/target.txt",
            "changed",
            move || {
                std::fs::remove_dir(&race_dir_for_swap).unwrap();
                symlink(&outside_dir, &race_dir_for_swap).unwrap();
            },
        )
        .await
        .unwrap_err();

        assert!(err.to_string().contains("escapes workspace"));
        let outside_content = tokio::fs::read_to_string(&outside_file).await.unwrap();
        assert_eq!(outside_content, "original");
    }
}
