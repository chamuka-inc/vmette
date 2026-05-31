//! The MCP tool-router for vmette.
//!
//! Two families of tools:
//!
//! * one-shot / workspace (each call boots a fresh microVM, direct subprocess):
//!   * `execute`           — one-shot `python` / `node` / `shell` code
//!   * `fetch_url`         — HTTPS GET with byte cap, returns body
//!   * `workspace_create`  — allocate a scratch dir + per-call image
//!   * `workspace_write`   — write a file inside a workspace
//!   * `workspace_read`    — read a file from a workspace
//!   * `workspace_run`     — shell command inside a microVM, workspace mounted at /mnt/work
//!   * `workspace_destroy` — tear down a workspace
//! * desktop computer use (persistent session, routed through `vmetted`):
//!   * `desktop_start` / `desktop_stop` — lifecycle
//!   * `desktop_screenshot` — capture (returns a PNG image content block)
//!   * `desktop_move` / `desktop_click` / `desktop_double_click` /
//!     `desktop_right_click` / `desktop_cursor_position` — pointer
//!   * `desktop_type` / `desktop_key` / `desktop_scroll` / `desktop_exec` — input
//!   * `desktop_launch` — start a GUI app and return its first painted frame
//!
//! Most tools return their result as a single plain-text MCP content
//! block (`desktop_screenshot` returns an image block). Structured
//! JSON-result returns are also possible via the rmcp `Json` wrapper but
//! plain text is what most agent UIs render sensibly today.

use std::sync::Arc;
use std::time::{Duration, Instant};

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{CallToolResult, Content, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tracing::warn;
use vmette_proto::agent::{Action, ScrollDirection};
use vmette_proto::daemon::ActionReply;

use crate::daemon_client::DaemonClient;
use crate::sandbox::{RunReply, RunRequest, Sandbox, Share};
use crate::workspace::{open_for_read, open_for_write, WorkspaceState};

const DEFAULT_TIMEOUT_S: u32 = 30;
const DEFAULT_WORKSPACE_TIMEOUT_S: u32 = 60;
const DEFAULT_FETCH_MAX_BYTES: usize = 20_000;

/// `desktop_launch` readiness budget: how long to wait for the launched app to
/// first paint before giving up and returning the latest frame. Cold-starting
/// a GUI app under software rendering can take tens of seconds.
const DEFAULT_LAUNCH_TIMEOUT_MS: u64 = 60_000;
/// Interval between first-paint probes in `desktop_launch`.
const LAUNCH_POLL_MS: u64 = 2_000;
/// After first paint, how long to let the app finish drawing and settle.
const LAUNCH_SETTLE_MS: u64 = 15_000;
/// How long the screen must stay continuously settled before `desktop_launch`
/// accepts it as the final frame. A browser paints its chrome, then sits on a
/// blank page while it fetches over the network — a settle the app isn't
/// actually done with. A hold this long bridges that chrome-then-content gap so
/// launch returns the loaded page, not the half-loaded one. Larger than the
/// daemon's per-action default (`DEFAULT_SETTLE_HOLD_MS`) for that reason.
const LAUNCH_SETTLE_HOLD_MS: u64 = 2_500;
/// Where `desktop_launch` redirects the launched app's stdout/stderr in-guest.
/// Chatty GUI apps (a browser emits hundreds of dbus error lines) would
/// otherwise block on a full stdio pipe before painting; the redirect drains
/// them to a file that's inspectable from in-session.
const LAUNCH_LOG_PATH: &str = "/tmp/vmette-launch.log";

/// Tool inputs ----------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExecuteArgs {
    /// Language: "python", "node", or "shell". Each maps to a canonical
    /// OCI image (python:3.12-alpine, node:20-alpine, alpine:3.20).
    pub language: String,
    /// Source code to execute. Quoting is handled for you.
    pub code: String,
    /// Allow outbound network from the guest (default: false). Subject
    /// to the server's --allow-network policy — denied calls return
    /// an error rather than silently running offline.
    #[serde(default)]
    pub network: bool,
    /// Wall-clock timeout in seconds (default: 30). Cap is enforced
    /// inside vmette via --timeout; a timed-out guest returns exit 124.
    pub timeout: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FetchUrlArgs {
    /// The URL to fetch. Only http/https are supported; redirects are
    /// followed by urllib.
    pub url: String,
    /// Cap the returned body length in bytes (default: 20000). Useful
    /// for context-window management.
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WorkspaceCreateArgs {
    /// OCI image used by `workspace_run` against this workspace.
    /// Default: alpine:3.20.
    pub image: Option<String>,
    /// Enable network for `workspace_run` calls. Subject to the
    /// server's --allow-network policy.
    #[serde(default)]
    pub network: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WorkspaceWriteArgs {
    pub workspace_id: String,
    /// Relative path inside the workspace. May not be absolute or
    /// contain `..`.
    pub path: String,
    pub content: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WorkspaceReadArgs {
    pub workspace_id: String,
    pub path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WorkspaceRunArgs {
    pub workspace_id: String,
    /// Shell command — runs as `sh -c "$command"` inside the guest
    /// with the workspace mounted read-write at /mnt/work and cwd
    /// initially set to /mnt/work.
    pub command: String,
    /// Wall-clock timeout in seconds (default: 60).
    pub timeout: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WorkspaceDestroyArgs {
    pub workspace_id: String,
}

// --- desktop computer-use tool inputs ------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DesktopStartArgs {
    /// Rootfs spec for the desktop image (OCI ref / tar+file:// / path). When
    /// omitted, resolves to `$VMETTE_DESKTOP_IMAGE`, else a locally built
    /// `assets/vmette-desktop-rootfs.tar`, else the registry fallback. Must be
    /// x86_64 and ship the desktop agent.
    pub image: Option<String>,
    /// Display size as "WIDTHxHEIGHT" (default: 1280x800).
    pub size: Option<String>,
    /// Allow outbound network from the desktop VM (default: false).
    /// Subject to the server's --allow-network policy.
    #[serde(default)]
    pub network: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DesktopSessionArgs {
    /// Session id returned by desktop_start.
    pub session_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DesktopPointArgs {
    pub session_id: String,
    /// X coordinate in pixels from the left edge.
    pub x: i32,
    /// Y coordinate in pixels from the top edge.
    pub y: i32,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DesktopTypeArgs {
    pub session_id: String,
    /// UTF-8 text to type at the current focus.
    pub text: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DesktopKeyArgs {
    pub session_id: String,
    /// Key chord, e.g. "ctrl+c", "Return", "alt+Tab".
    pub keys: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DesktopScrollArgs {
    pub session_id: String,
    /// X coordinate to scroll at.
    pub x: i32,
    /// Y coordinate to scroll at.
    pub y: i32,
    /// Scroll direction: "up", "down", "left", or "right".
    pub direction: String,
    /// Number of scroll clicks.
    pub amount: i32,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DesktopExecArgs {
    pub session_id: String,
    /// Shell command launched inside the guest, e.g. "xterm &".
    pub command: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DesktopLaunchArgs {
    pub session_id: String,
    /// Shell command that starts a GUI app, e.g. "xterm", "gimp /mnt/a.png",
    /// or "chromium https://example.com". No trailing '&' needed — the call
    /// backgrounds it for you and waits for the app to paint.
    pub command: String,
    /// Max time to wait for the app to first paint, in milliseconds
    /// (default: 60000). Cold-starting a GUI app under software rendering
    /// can take tens of seconds; the latest frame is returned either way.
    pub wait_ms: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DesktopSettleArgs {
    /// Session id returned by desktop_start.
    pub session_id: String,
    /// Max time to wait for the screen to stop changing before returning the
    /// latest frame anyway (in milliseconds). Defaults to 10000 (10s).
    pub timeout_ms: Option<u64>,
}

// --- structured returns for the workspace_create tool --------------------

#[derive(Debug, Serialize, JsonSchema)]
pub struct WorkspaceCreateResult {
    pub workspace_id: String,
    pub image: String,
    // No `host_path`: the agent must never see a host filesystem path (it
    // operates on the workspace only via `workspace_id`). Exposing it would
    // contradict the isolation boundary the server documents.
}

// --- the server ----------------------------------------------------------

#[derive(Clone)]
pub struct VmetteServer {
    sandbox: Arc<Sandbox>,
    workspaces: Arc<WorkspaceState>,
    daemon: Arc<DaemonClient>,
    default_image: String,
    allow_network: bool,
    /// Populated by `Self::tool_router()` from the `#[tool_router]`
    /// macro; the `#[tool_handler]` macro reads it via the macro's
    /// generated code, which the dead-code lint can't see.
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

impl VmetteServer {
    pub fn new(
        sandbox: Sandbox,
        workspaces: WorkspaceState,
        daemon: DaemonClient,
        default_image: String,
        allow_network: bool,
    ) -> Self {
        Self {
            sandbox: Arc::new(sandbox),
            workspaces: Arc::new(workspaces),
            daemon: Arc::new(daemon),
            default_image,
            allow_network,
            tool_router: Self::tool_router(),
        }
    }

    /// Reject a network=true call when the server was started without
    /// --allow-network. Returns the gated `net` value (`false` when
    /// the server forbids network entirely; otherwise the request).
    fn gate_network(&self, requested: bool) -> Result<bool, ErrorData> {
        if requested && !self.allow_network {
            return Err(ErrorData::invalid_params(
                "this MCP server was started without --allow-network; \
                 set network=false or restart the server with --allow-network"
                    .to_string(),
                None,
            ));
        }
        Ok(requested)
    }
}

#[tool_router]
impl VmetteServer {
    #[tool(
        description = "Execute a code snippet (python/node/shell) in a fresh microVM. Returns stdout, stderr, and exit code. Each call boots a clean kernel; nothing persists across calls. Use workspace_* tools when you need state between turns."
    )]
    async fn execute(
        &self,
        Parameters(args): Parameters<ExecuteArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let net = self.gate_network(args.network)?;
        let (rootfs, exec) = build_execute_command(&args.language, &args.code)?;
        let req = RunRequest {
            rootfs,
            exec,
            shares: Vec::new(),
            net,
            timeout_seconds: Some(args.timeout.unwrap_or(DEFAULT_TIMEOUT_S)),
            offline: false,
        };
        let reply = self
            .sandbox
            .run(&req)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(format_reply(
            &reply,
        ))]))
    }

    #[tool(
        description = "Fetch a URL via HTTP(S) GET from inside a microVM, returning status and body. Use this instead of giving the model raw web access to your host. The tool is always registered, but calls fail with an explicit error if the server was started without --allow-network. Only http:// and https:// schemes are accepted; file:// / ftp:// / data:// are rejected."
    )]
    async fn fetch_url(
        &self,
        Parameters(args): Parameters<FetchUrlArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let _ = self.gate_network(true)?;

        // Scheme validation: reject anything other than http/https. Without
        // this, an agent could pass `file:///etc/shadow` and urllib would
        // happily open the path inside the guest's rootfs.
        crate::weburl::validate_web_url(&args.url)
            .map_err(|e| ErrorData::invalid_params(e, None))?;

        // Build the Python source. The URL is serialised with
        // `serde_json::to_string` because JSON string syntax is a
        // valid Python string literal — `\uXXXX` (4 hex) is what
        // Python expects, while Rust's `{:?}` would emit `\u{XXXX}`
        // (braced) which Python's tokenizer rejects.
        let max_bytes = args.max_bytes.unwrap_or(DEFAULT_FETCH_MAX_BYTES);
        let url_lit = serde_json::to_string(&args.url)
            .map_err(|e| ErrorData::internal_error(format!("url to json: {e}"), None))?;
        let py = format!(
            "import urllib.request, json; r = urllib.request.urlopen({url_lit}, timeout=10); print(json.dumps({{'status': r.status, 'body': r.read({max_bytes}).decode('utf-8','replace')}}))"
        );
        let req = RunRequest {
            rootfs: "python:3.12-alpine".into(),
            exec: shell_quoted_python(&py),
            shares: Vec::new(),
            net: true,
            timeout_seconds: Some(DEFAULT_TIMEOUT_S),
            offline: false,
        };
        let reply = self
            .sandbox
            .run(&req)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(format_reply(
            &reply,
        ))]))
    }

    #[tool(
        description = "Create a per-task workspace: a host directory owned by this MCP server that the agent can read/write via workspace_write/read and operate on with workspace_run. Returns a workspace_id token to pass to subsequent calls."
    )]
    async fn workspace_create(
        &self,
        Parameters(args): Parameters<WorkspaceCreateArgs>,
    ) -> Result<Json<WorkspaceCreateResult>, ErrorData> {
        let net = self.gate_network(args.network)?;
        let image = args.image.unwrap_or_else(|| self.default_image.clone());
        let ws = self
            .workspaces
            .create(image.clone(), net)
            .map_err(|e| ErrorData::invalid_params(e.to_string(), None))?;
        Ok(Json(WorkspaceCreateResult {
            workspace_id: ws.id,
            image,
        }))
    }

    #[tool(
        description = "Write a file inside a workspace. Relative path only; absolute paths and '..' rejected. Path safety: every component is opened via openat with O_DIRECTORY|O_NOFOLLOW, so an agent that creates symlinks via workspace_run (e.g. ws/escape -> /etc) cannot trick subsequent writes into following them. Overwrites existing files; creates missing parent dirs."
    )]
    async fn workspace_write(
        &self,
        Parameters(args): Parameters<WorkspaceWriteArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let ws = self
            .workspaces
            .get(&args.workspace_id)
            .map_err(|e| ErrorData::invalid_params(e.to_string(), None))?;
        let fd = open_for_write(&ws.dir, &args.path).map_err(|e| {
            // Path safety / not-found errors are caller-side: surface as invalid_params.
            ErrorData::invalid_params(e.to_string(), None)
        })?;
        let mut file = std::fs::File::from(fd);
        use std::io::Write;
        file.write_all(args.content.as_bytes())
            .map_err(|e| ErrorData::internal_error(format!("write {}: {}", args.path, e), None))?;
        let written = args.content.len();
        Ok(CallToolResult::success(vec![Content::text(format!(
            "wrote {} ({} bytes)",
            args.path, written
        ))]))
    }

    #[tool(
        description = "Read a file from a workspace. Relative path only. Same openat+NOFOLLOW path-safety as workspace_write."
    )]
    async fn workspace_read(
        &self,
        Parameters(args): Parameters<WorkspaceReadArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let ws = self
            .workspaces
            .get(&args.workspace_id)
            .map_err(|e| ErrorData::invalid_params(e.to_string(), None))?;
        let fd = open_for_read(&ws.dir, &args.path)
            .map_err(|e| ErrorData::invalid_params(e.to_string(), None))?;
        let mut file = std::fs::File::from(fd);
        use std::io::Read;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|e| ErrorData::internal_error(format!("read {}: {}", args.path, e), None))?;
        Ok(CallToolResult::success(vec![Content::text(
            String::from_utf8_lossy(&bytes).into_owned(),
        )]))
    }

    #[tool(
        description = "Run a shell command inside the workspace's microVM. The workspace directory is mounted at /mnt/work (read-write) and is the initial cwd. The exact exec is `sh -c \"cd /mnt/work && <your-command>\"`, so don't begin `command` with a shell operator like `&&` or `||`. Image and network policy were set at workspace_create time."
    )]
    async fn workspace_run(
        &self,
        Parameters(args): Parameters<WorkspaceRunArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let ws = self
            .workspaces
            .get(&args.workspace_id)
            .map_err(|e| ErrorData::invalid_params(e.to_string(), None))?;
        // Run from /mnt/work so relative paths in the command DTRT.
        let exec = format!("cd /mnt/work && {}", args.command);
        let req = RunRequest {
            rootfs: ws.image.clone(),
            exec,
            shares: vec![Share {
                tag: "work".into(),
                path: ws.dir.clone(),
            }],
            net: ws.net,
            timeout_seconds: Some(args.timeout.unwrap_or(DEFAULT_WORKSPACE_TIMEOUT_S)),
            offline: false,
        };
        let reply = self
            .sandbox
            .run(&req)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(format_reply(
            &reply,
        ))]))
    }

    #[tool(
        description = "Destroy a workspace: remove its host directory and forget the id. Idempotent."
    )]
    async fn workspace_destroy(
        &self,
        Parameters(args): Parameters<WorkspaceDestroyArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Err(e) = self.workspaces.destroy(&args.workspace_id) {
            warn!(error = %e, id = %args.workspace_id, "destroy failed");
        }
        Ok(CallToolResult::success(vec![Content::text(format!(
            "workspace {} destroyed",
            args.workspace_id
        ))]))
    }

    // --- desktop computer-use tools (routed through vmetted) -------------

    #[tool(
        description = "Start a persistent graphical Linux desktop session (Xvfb + window manager) inside a microVM, driven via screenshots and synthetic mouse/keyboard. Returns a session_id to pass to the other desktop_* tools. The session outlives individual tool calls and must be torn down with desktop_stop (idle sessions are evicted after 30 min). Requires the vmette daemon (vmetted) to be running."
    )]
    async fn desktop_start(
        &self,
        Parameters(args): Parameters<DesktopStartArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let net = self.gate_network(args.network)?;
        // `offline: false` always: pulling the rootfs image is a host-side
        // operation (like the one-shot/workspace tools, which never run
        // offline), independent of whether the guest VM gets network. Tying
        // offline to `net` would make the default (network=false) unable to
        // fetch the baked-in desktop image on first use.
        let session_id = self
            .daemon
            .start(args.image, args.size, net, false)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(session_id)]))
    }

    #[tool(
        description = "Capture the desktop framebuffer and return it as a PNG image content block. This is the agent's primary way to see the desktop state before deciding on the next action."
    )]
    async fn desktop_screenshot(
        &self,
        Parameters(args): Parameters<DesktopSessionArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let reply = self.action(&args.session_id, Action::Screenshot).await?;
        let png = reply.png_base64.ok_or_else(|| {
            ErrorData::internal_error("screenshot reply had no PNG payload".to_string(), None)
        })?;
        Ok(CallToolResult::success(vec![Content::image(
            png,
            "image/png".to_string(),
        )]))
    }

    #[tool(
        description = "Capture the desktop only once it has stopped changing, then return it as a PNG plus a note about any regions still in motion. Prefer this over desktop_screenshot right after an action that triggers loading/animation (navigating a page, opening a menu): it polls the framebuffer and waits for the screen to settle, so you see the final state instead of a mid-transition frame. A playing video or spinner won't block it — those are reported as 'still moving' rectangles you can reason about. If the screen never settles within timeout_ms, the latest frame is returned with a note that it had not settled."
    )]
    async fn desktop_screenshot_when_settled(
        &self,
        Parameters(args): Parameters<DesktopSettleArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let reply = self
            .daemon
            .screenshot_when_settled(&args.session_id, args.timeout_ms, None)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let note = if reply.settled {
            if reply.moving.is_empty() {
                "screen settled; nothing moving".to_string()
            } else {
                let regions: Vec<String> = reply
                    .moving
                    .iter()
                    .map(|r| format!("{}x{}+{}+{}", r.w, r.h, r.x, r.y))
                    .collect();
                format!("screen settled; still moving: {}", regions.join(", "))
            }
        } else {
            "screen did not settle within timeout; latest frame returned".to_string()
        };
        Ok(CallToolResult::success(vec![
            Content::text(note),
            Content::image(reply.png_base64, "image/png".to_string()),
        ]))
    }

    #[tool(
        description = "Capture the desktop and report what changed since the previous capture in this session, as a PNG plus the bounding box of the change (or a note that nothing changed). Useful for confirming an action had a localized effect, or detecting that the screen is now static."
    )]
    async fn desktop_what_changed(
        &self,
        Parameters(args): Parameters<DesktopSessionArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let reply = self
            .daemon
            .what_changed(&args.session_id)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let note = match reply.changed {
            Some(r) => format!("changed region: {}x{}+{}+{}", r.w, r.h, r.x, r.y),
            None => "nothing changed since the previous capture".to_string(),
        };
        Ok(CallToolResult::success(vec![
            Content::text(note),
            Content::image(reply.png_base64, "image/png".to_string()),
        ]))
    }

    #[tool(description = "Report the current pointer position as 'x y'.")]
    async fn desktop_cursor_position(
        &self,
        Parameters(args): Parameters<DesktopSessionArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let reply = self
            .action(&args.session_id, Action::CursorPosition)
            .await?;
        let x = reply.x.unwrap_or(0);
        let y = reply.y.unwrap_or(0);
        Ok(CallToolResult::success(vec![Content::text(format!(
            "{x} {y}"
        ))]))
    }

    #[tool(description = "Move the pointer to (x, y) without clicking.")]
    async fn desktop_move(
        &self,
        Parameters(args): Parameters<DesktopPointArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.action(
            &args.session_id,
            Action::MouseMove {
                x: args.x,
                y: args.y,
            },
        )
        .await?;
        Ok(ok_text(format!("moved to {} {}", args.x, args.y)))
    }

    #[tool(description = "Left-click at (x, y).")]
    async fn desktop_click(
        &self,
        Parameters(args): Parameters<DesktopPointArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.click_at(&args.session_id, args.x, args.y, Action::LeftClick)
            .await
    }

    #[tool(description = "Double left-click at (x, y).")]
    async fn desktop_double_click(
        &self,
        Parameters(args): Parameters<DesktopPointArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.click_at(&args.session_id, args.x, args.y, Action::DoubleClick)
            .await
    }

    #[tool(description = "Right-click at (x, y).")]
    async fn desktop_right_click(
        &self,
        Parameters(args): Parameters<DesktopPointArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.click_at(&args.session_id, args.x, args.y, Action::RightClick)
            .await
    }

    #[tool(description = "Middle-click (button 2 / paste-selection / open-in-new-tab) at (x,y).")]
    async fn desktop_middle_click(
        &self,
        Parameters(args): Parameters<DesktopPointArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.click_at(&args.session_id, args.x, args.y, Action::MiddleClick)
            .await
    }

    #[tool(
        description = "Press the left button, move to (x,y), and release — a drag. The drag STARTS at the current pointer position, so desktop_move there first. Use for text selection, sliders/scrollbars, drag-and-drop, and canvas drawing."
    )]
    async fn desktop_drag(
        &self,
        Parameters(args): Parameters<DesktopPointArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.action(
            &args.session_id,
            Action::LeftClickDrag {
                x: args.x,
                y: args.y,
            },
        )
        .await?;
        Ok(ok_text(format!("dragged to {} {}", args.x, args.y)))
    }

    #[tool(description = "Type a UTF-8 string at the current keyboard focus.")]
    async fn desktop_type(
        &self,
        Parameters(args): Parameters<DesktopTypeArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.action(&args.session_id, Action::Type { text: args.text })
            .await?;
        Ok(ok_text("typed".to_string()))
    }

    #[tool(
        description = "Press a key chord such as 'ctrl+c', 'Return', or 'alt+Tab'. Use desktop_type for ordinary text."
    )]
    async fn desktop_key(
        &self,
        Parameters(args): Parameters<DesktopKeyArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.action(&args.session_id, Action::Key { keys: args.keys })
            .await?;
        Ok(ok_text("key sent".to_string()))
    }

    #[tool(description = "Scroll at (x, y). direction is up|down|left|right; amount is clicks.")]
    async fn desktop_scroll(
        &self,
        Parameters(args): Parameters<DesktopScrollArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let direction = match args.direction.as_str() {
            "up" => ScrollDirection::Up,
            "down" => ScrollDirection::Down,
            "left" => ScrollDirection::Left,
            "right" => ScrollDirection::Right,
            other => {
                return Err(ErrorData::invalid_params(
                    format!("direction must be up|down|left|right, got {other:?}"),
                    None,
                ));
            }
        };
        self.action(
            &args.session_id,
            Action::Scroll {
                x: args.x,
                y: args.y,
                direction,
                amount: args.amount,
            },
        )
        .await?;
        Ok(ok_text("scrolled".to_string()))
    }

    #[tool(
        description = "Launch a shell command inside the desktop guest, e.g. 'xterm &' or 'chromium &'. Use trailing '&' for GUI apps so the call returns immediately."
    )]
    async fn desktop_exec(
        &self,
        Parameters(args): Parameters<DesktopExecArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.action(
            &args.session_id,
            Action::Exec {
                command: args.command,
            },
        )
        .await?;
        Ok(ok_text("launched".to_string()))
    }

    #[tool(
        description = "Launch a GUI app in the desktop session and return its first painted frame as a screenshot. The one-call way to start an app and see it: it backgrounds the command, waits for the screen to change (the window mapping and drawing) and then settle, and returns that frame. Prefer this over desktop_exec + polling when you want to start something and immediately look at it — e.g. command='chromium https://example.com', 'gimp /mnt/a.png', or 'xterm'. The command runs as given; supply whatever flags the app needs (the desktop image bakes sensible defaults for the browser it ships). Network-dependent apps only reach the network if the session was started with network=true."
    )]
    async fn desktop_launch(
        &self,
        Parameters(args): Parameters<DesktopLaunchArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        // Record the pre-launch frame as the diff baseline so the readiness
        // loop can distinguish "the app painted" from "still the bare
        // desktop". Keep the frame itself for a final reconciliation below.
        let baseline_png = self
            .daemon
            .what_changed(&args.session_id)
            .await
            .ok()
            .map(|c| c.png_base64);

        // Background the command so the exec returns immediately (the readiness
        // loop below is what waits for the window, not the exec call), and
        // redirect its stdio to a guest log so a chatty app can't block on a
        // full stdout/stderr pipe before it paints.
        let command = format!(
            "{} >{LAUNCH_LOG_PATH} 2>&1 &",
            args.command.trim_end_matches(['&', ' '])
        );
        self.action(&args.session_id, Action::Exec { command })
            .await?;

        // Wait for first paint. A freshly launched app maps a *black* window
        // that the settle detector reads as "settled; nothing moving", so
        // settling immediately would hand back a black frame. Instead poll
        // what_changed until the frame differs from the baseline (window
        // mapped / started drawing), bounded by wait_ms.
        let budget = Duration::from_millis(args.wait_ms.unwrap_or(DEFAULT_LAUNCH_TIMEOUT_MS));
        let deadline = Instant::now() + budget;
        let mut painted = false;
        while Instant::now() < deadline {
            match self.daemon.what_changed(&args.session_id).await {
                Ok(reply) if reply.changed.is_some() => {
                    painted = true;
                    break;
                }
                Ok(_) => {}
                Err(e) => return Err(ErrorData::internal_error(e.to_string(), None)),
            }
            tokio::time::sleep(Duration::from_millis(LAUNCH_POLL_MS)).await;
        }

        // Then let the app finish drawing and the screen stop moving, and
        // return that final frame.
        let settle = self
            .daemon
            .screenshot_when_settled(
                &args.session_id,
                Some(LAUNCH_SETTLE_MS),
                Some(LAUNCH_SETTLE_HOLD_MS),
            )
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        // The polling loop only catches a change that lands *between two
        // polls*; a short wait_ms can miss an app that first-paints after the
        // last poll but before the settle capture, yielding a false "did it
        // start?" on a frame that plainly shows the app. Reconcile against the
        // pre-launch baseline: if the final settled frame differs, it painted.
        // A *missing* baseline (the pre-launch capture errored) is no evidence
        // either way, so it must not upgrade the verdict — fall back to the
        // poll-loop result, or we'd report a false "launched" for an app that
        // never painted whenever that one capture happened to fail.
        let started = painted
            || match baseline_png.as_ref() {
                Some(b) => *b != settle.png_base64,
                None => false,
            };
        let note = if started {
            format!("launched {}", args.command)
        } else {
            format!(
                "ran {:?} but the screen did not change within {}s — did it start? \
                 (is the binary on PATH in this desktop image, and does it need flags? \
                 check {} in-guest)",
                args.command,
                budget.as_secs(),
                LAUNCH_LOG_PATH,
            )
        };
        Ok(CallToolResult::success(vec![
            Content::text(note),
            Content::image(settle.png_base64, "image/png".to_string()),
        ]))
    }

    #[tool(
        description = "Stop a desktop session and tear its VM down. Idempotent-ish: errors if the id is unknown."
    )]
    async fn desktop_stop(
        &self,
        Parameters(args): Parameters<DesktopSessionArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.daemon
            .stop(&args.session_id)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(ok_text(format!("stopped {}", args.session_id)))
    }
}

impl VmetteServer {
    /// Send one desktop action, surfacing an `ok:false` agent reply as an
    /// error so tools don't silently report success on a failed action.
    async fn action(&self, session_id: &str, action: Action) -> Result<ActionReply, ErrorData> {
        let reply = self
            .daemon
            .action(session_id, action)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        if !reply.ok {
            let msg = reply
                .error
                .unwrap_or_else(|| "desktop action failed".to_string());
            return Err(ErrorData::internal_error(msg, None));
        }
        Ok(reply)
    }

    /// Move to (x, y) then click. The agent's click actions fire at the
    /// current pointer position, so we position first.
    async fn click_at(
        &self,
        session_id: &str,
        x: i32,
        y: i32,
        click: Action,
    ) -> Result<CallToolResult, ErrorData> {
        let label = click_label(&click);
        self.action(session_id, Action::MouseMove { x, y }).await?;
        self.action(session_id, click).await?;
        Ok(ok_text(format!("{label} at {x} {y}")))
    }
}

#[tool_handler]
impl ServerHandler for VmetteServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "vmette MCP server: sandboxed code execution and per-task workspaces \
                 on macOS via Apple's Virtualization.framework. Each tool call boots \
                 a fresh Linux microVM (~1s) and tears it down when the call returns. \
                 Use `execute` for one-shot code; use `workspace_*` for state across turns."
                .to_string(),
        )
    }
}

// --- helpers -------------------------------------------------------------

/// Translate (language, code) → (rootfs OCI ref, shell exec). The
/// shell exec uses single-quote escaping (via the embedded helper)
/// so embedded quotes, $, backticks, and newlines all round-trip
/// safely as one shell argument.
fn build_execute_command(language: &str, code: &str) -> Result<(String, String), ErrorData> {
    let (rootfs, runner) = match language {
        "python" => ("python:3.12-alpine", "python3 -c"),
        "node" => ("node:20-alpine", "node -e"),
        "shell" => ("alpine:3.20", "sh -c"),
        other => {
            return Err(ErrorData::invalid_params(
                format!("unknown language {other:?}; supported: python, node, shell"),
                None,
            ));
        }
    };
    let exec = format!("{runner} {}", single_quote(code));
    Ok((rootfs.into(), exec))
}

/// Shell single-quote a string for safe use as one argument to /bin/sh.
/// Single quotes preserve everything literally except embedded single
/// quotes, which we close, escape, and reopen.
fn single_quote(s: &str) -> String {
    let escaped = s.replace('\'', "'\\''");
    format!("'{escaped}'")
}

/// Specialised wrapper that builds `python3 -c '<code>'` shell args.
fn shell_quoted_python(code: &str) -> String {
    format!("python3 -c {}", single_quote(code))
}

/// Wrap a short status string as a successful single text content block.
fn ok_text(msg: String) -> CallToolResult {
    CallToolResult::success(vec![Content::text(msg)])
}

/// Human-readable label for a click action, for the tool's success message.
fn click_label(a: &Action) -> &'static str {
    match a {
        Action::LeftClick => "left_click",
        Action::DoubleClick => "double_click",
        Action::RightClick => "right_click",
        Action::MiddleClick => "middle_click",
        _ => "click",
    }
}

fn format_reply(r: &RunReply) -> String {
    // A compact, model-readable summary. Stderr is included only when
    // non-empty since vmette's own banner is verbose and rarely useful
    // to the agent.
    let mut out = format!("exit: {}\n\nstdout:\n{}", r.exit, r.stdout);
    if !r.stderr.trim().is_empty() {
        out.push_str("\n\nstderr:\n");
        out.push_str(&r.stderr);
    }
    out
}

// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_quote_handles_quotes_and_meta() {
        assert_eq!(single_quote("hello"), "'hello'");
        assert_eq!(single_quote("it's"), "'it'\\''s'");
        assert_eq!(single_quote("$(foo)"), "'$(foo)'");
        assert_eq!(single_quote("`bar`"), "'`bar`'");
        assert_eq!(single_quote("a\nb"), "'a\nb'");
    }

    #[test]
    fn build_execute_command_known_languages() {
        let (rootfs, exec) = build_execute_command("python", "print(1)").unwrap();
        assert_eq!(rootfs, "python:3.12-alpine");
        assert!(exec.starts_with("python3 -c "));
        assert!(exec.contains("print(1)"));

        let (rootfs, exec) = build_execute_command("node", "console.log(1)").unwrap();
        assert_eq!(rootfs, "node:20-alpine");
        assert!(exec.starts_with("node -e "));

        let (rootfs, exec) = build_execute_command("shell", "echo hi").unwrap();
        assert_eq!(rootfs, "alpine:3.20");
        assert!(exec.starts_with("sh -c "));
    }

    #[test]
    fn build_execute_command_rejects_unknown_language() {
        assert!(build_execute_command("ruby", "puts 1").is_err());
    }

    #[test]
    fn format_reply_omits_empty_stderr() {
        let r = RunReply {
            stdout: "hi".into(),
            stderr: "".into(),
            exit: 0,
        };
        let f = format_reply(&r);
        assert!(f.contains("exit: 0"));
        assert!(f.contains("hi"));
        assert!(!f.contains("stderr"));
    }
}
