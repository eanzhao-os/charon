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
