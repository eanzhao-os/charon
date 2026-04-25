use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthResponse {
    pub ok: bool,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WhoAmIResponse {
    pub ok: bool,
    pub version: String,
    pub identity: NyxIdentity,
}

/// Identity propagated by NyxID via the `X-NyxID-Identity-Token` header,
/// extracted from RS256 JWT claims after JWKS verification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NyxIdentity {
    pub user_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub permissions: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nyx_service_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    pub issued_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

// ---------- workspaces (M2.1) ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub project_root: PathBuf,
    pub base_branch: String,
    pub branch: String,
    pub worktree_path: PathBuf,
    pub created_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateWorkspaceRequest {
    /// Absolute path to the source repo. Must be a git working tree.
    pub project_root: PathBuf,
    #[serde(default)]
    pub title: Option<String>,
    /// If omitted, defaults to the source repo's current HEAD branch.
    #[serde(default)]
    pub base_branch: Option<String>,
    /// If omitted, defaults to `charon/<workspace-id 前 8 位>`.
    #[serde(default)]
    pub new_branch: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListWorkspacesResponse {
    pub workspaces: Vec<Workspace>,
}

// ---------- WS frames (M2.2) ----------
//
// JSON envelopes on `wss://.../api/v1/ws`. Request/response correlation via
// `id` (client-chosen). Server-pushed frames (Hello, future subscriptions)
// have no `id`. Errors carry the originating request `id` when there was one.
//
// Binary attachments (36-byte UUID prefix on a binary frame, referenced by
// `binary_attachment_ref` in the JSON envelope) are reserved for M2.3 (file
// blobs) and M2.4 (terminal output). M2.2 ships text frames only.

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClientFrame {
    #[serde(rename = "Workspace.List")]
    WorkspaceList {
        id: String,
        #[serde(default)]
        payload: WorkspaceListPayload,
    },
    #[serde(rename = "Workspace.Create")]
    WorkspaceCreate {
        id: String,
        payload: CreateWorkspaceRequest,
    },
    #[serde(rename = "Workspace.Get")]
    WorkspaceGet {
        id: String,
        payload: WorkspaceIdPayload,
    },
    #[serde(rename = "Workspace.Archive")]
    WorkspaceArchive {
        id: String,
        payload: WorkspaceIdPayload,
    },
    #[serde(rename = "File.List")]
    FileList {
        id: String,
        payload: FileTreePayload,
    },
    #[serde(rename = "File.Read")]
    FileRead {
        id: String,
        payload: FilePathPayload,
    },
    #[serde(rename = "File.Write")]
    FileWrite {
        id: String,
        payload: FileWritePayload,
    },
    #[serde(rename = "Diff.Get")]
    DiffGet { id: String, payload: DiffPayload },
    #[serde(rename = "Terminal.Create")]
    TerminalCreate {
        id: String,
        payload: CreateTerminalPayload,
    },
    #[serde(rename = "Terminal.SendKeys")]
    TerminalSendKeys {
        id: String,
        payload: TerminalSendKeysPayload,
    },
    #[serde(rename = "Terminal.Resize")]
    TerminalResize {
        id: String,
        payload: TerminalResizePayload,
    },
    #[serde(rename = "Terminal.Kill")]
    TerminalKill {
        id: String,
        payload: TerminalIdPayload,
    },
    #[serde(rename = "Terminal.List")]
    TerminalList {
        id: String,
        #[serde(default)]
        payload: TerminalListPayload,
    },
    #[serde(rename = "Terminal.Scrollback")]
    TerminalScrollback {
        id: String,
        payload: TerminalIdPayload,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ServerFrame {
    #[serde(rename = "Hello")]
    Hello { payload: HelloPayload },
    #[serde(rename = "Workspace.Listed")]
    WorkspaceListed {
        id: String,
        result: ListWorkspacesResponse,
    },
    #[serde(rename = "Workspace.Created")]
    WorkspaceCreated { id: String, result: Workspace },
    #[serde(rename = "Workspace.Got")]
    WorkspaceGot { id: String, result: Workspace },
    #[serde(rename = "Workspace.Archived")]
    WorkspaceArchived { id: String, result: Workspace },
    #[serde(rename = "File.Listed")]
    FileListed {
        id: String,
        result: FileTreeResponse,
    },
    #[serde(rename = "File.Content")]
    FileContent { id: String, result: FileContent },
    #[serde(rename = "File.Written")]
    FileWritten { id: String, result: FileContent },
    #[serde(rename = "Diff.Snapshot")]
    DiffSnapshot { id: String, result: DiffResponse },
    #[serde(rename = "Terminal.Created")]
    TerminalCreated { id: String, result: Terminal },
    #[serde(rename = "Terminal.KeysSent")]
    TerminalKeysSent { id: String, result: TerminalKeysAck },
    #[serde(rename = "Terminal.Resized")]
    TerminalResized {
        id: String,
        result: TerminalResizeAck,
    },
    #[serde(rename = "Terminal.Killed")]
    TerminalKilled { id: String, result: TerminalKillAck },
    #[serde(rename = "Terminal.Listed")]
    TerminalListed {
        id: String,
        result: TerminalListResponse,
    },
    #[serde(rename = "Terminal.ScrollbackSnapshot")]
    TerminalScrollbackSnapshot {
        id: String,
        result: TerminalScrollbackResponse,
    },
    /// Server-pushed (no `id`): raw PTY output chunk, base64-encoded.
    #[serde(rename = "Terminal.Output")]
    TerminalOutput {
        terminal_id: String,
        data_b64: String,
        seq: u64,
    },
    /// Server-pushed (no `id`): child process exited.
    #[serde(rename = "Terminal.Exited")]
    TerminalExited {
        terminal_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
    },
    #[serde(rename = "Error")]
    Error {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        error: WsError,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkspaceListPayload {
    #[serde(default)]
    pub include_archived: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceIdPayload {
    pub workspace_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloPayload {
    pub server: ServerInfo,
    pub identity: NyxIdentity,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInfo {
    pub version: String,
    pub server_time: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WsError {
    pub code: String,
    pub message: String,
}

// ---------- file / diff (M2.3) ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    /// Path relative to workspace root.
    pub path: String,
    pub kind: FileKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileKind {
    File,
    Dir,
    Symlink,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileTreeResponse {
    pub workspace_id: String,
    pub path: String,
    pub entries: Vec<FileEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileContent {
    pub workspace_id: String,
    pub path: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteFileRequest {
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffResponse {
    pub workspace_id: String,
    pub base_branch: String,
    pub changed: Vec<FileDiff>,
    pub untracked: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileDiff {
    pub path: String,
    pub status: DiffStatus,
    pub unified_diff: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiffStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Other,
}

// WS payloads for file / diff frames
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileTreePayload {
    pub workspace_id: String,
    #[serde(default)]
    pub path: String,
    #[serde(default = "default_tree_depth")]
    pub depth: u32,
}

fn default_tree_depth() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilePathPayload {
    pub workspace_id: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileWritePayload {
    pub workspace_id: String,
    pub path: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffPayload {
    pub workspace_id: String,
}

// ---------- terminal (M2.4) ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Terminal {
    pub id: String,
    pub workspace_id: String,
    pub command: String,
    pub cols: u16,
    pub rows: u16,
    pub status: TerminalStatus,
    pub created_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exited_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TerminalStatus {
    Running,
    Exited,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalListResponse {
    pub terminals: Vec<Terminal>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalScrollbackResponse {
    pub terminal_id: String,
    /// base64-encoded raw PTY bytes (may include ANSI escapes / partial UTF-8).
    pub data_b64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalKeysAck {
    pub terminal_id: String,
    pub bytes_written: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalResizeAck {
    pub terminal_id: String,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalKillAck {
    pub terminal_id: String,
}

// WS payloads (terminal)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateTerminalPayload {
    pub workspace_id: String,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default = "default_cols")]
    pub cols: u16,
    #[serde(default = "default_rows")]
    pub rows: u16,
}

fn default_cols() -> u16 {
    80
}
fn default_rows() -> u16 {
    24
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalSendKeysPayload {
    pub terminal_id: String,
    /// base64-encoded raw bytes to write to the PTY's stdin.
    pub data_b64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalResizePayload {
    pub terminal_id: String,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalIdPayload {
    pub terminal_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TerminalListPayload {
    #[serde(default)]
    pub workspace_id: Option<String>,
}
