//! Shared types between charon-daemon, charon-cli, and (eventually) charon-desktop.
//!
//! M1 surface: daemon's HTTP wire shapes + a handful of constants. Workspace /
//! agent / terminal models land in M2.

pub mod wire;

pub use wire::{
    ClientFrame, CreateTerminalPayload, CreateWorkspaceRequest, DiffPayload, DiffResponse,
    DiffStatus, FileContent, FileDiff, FileEntry, FileKind, FilePathPayload, FileTreePayload,
    FileTreeResponse, FileWritePayload, HealthResponse, HelloPayload, ListWorkspacesResponse,
    NyxIdentity, ServerFrame, ServerInfo, Terminal, TerminalIdPayload, TerminalKeysAck,
    TerminalKillAck, TerminalListPayload, TerminalListResponse, TerminalRemoveAck,
    TerminalResizeAck, TerminalResizePayload, TerminalScrollbackResponse, TerminalSendKeysPayload,
    TerminalStatus, WhoAmIResponse, Workspace, WorkspaceIdPayload, WorkspaceListPayload,
    WriteFileRequest, WsError,
};

/// Default loopback bind for the daemon. Mirrored by the existing
/// `charon-echo-poc` UserService endpoint URL.
pub const DEFAULT_DAEMON_BIND: &str = "127.0.0.1:18789";

/// Default issuer (NyxID prod). Override with `CHARON_NYXID_ISSUER`.
pub const DEFAULT_NYXID_ISSUER: &str = "https://nyx-api.chrono-ai.fun";

/// Crate version, surfaced in `/api/v1/health` so clients can detect upgrades.
pub const DAEMON_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Header name carrying the per-request RS256 JWT minted by NyxID.
pub const IDENTITY_TOKEN_HEADER: &str = "X-NyxID-Identity-Token";

/// Default UserService slug for `charon doctor`'s end-to-end probe.
///
/// M1 reuses the slug the PoC echo daemon was registered under. Once `charon
/// link` lands (M2) and writes a per-host config, `doctor` should read the slug
/// from there instead.
pub const DEFAULT_USER_SERVICE_SLUG: &str = "charon-echo-poc";

/// HTTP path the daemon exposes its WebSocket upgrade on.
pub const WS_PATH: &str = "/api/v1/ws";
