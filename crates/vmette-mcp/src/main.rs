//! vmette-mcp — Model Context Protocol server exposing vmette to AI agents.
//!
//! Plugs into any MCP-aware host (Claude Desktop, Cursor, Cline, Zed, …).
//! Each tool call boots a fresh Linux microVM via Apple's
//! Virtualization.framework, runs the agent's request, and tears the VM
//! down on return. The server itself is long-lived; it dies when the
//! MCP client closes its stdio connection.
//!
//! Usage (manual): `vmette-mcp [--allow-network] [--default-image alpine:3.20]`
//! Typical: pointed at by a Claude Desktop / Cursor `mcpServers` config.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use rmcp::transport::stdio;
use rmcp::ServiceExt;
use tracing_subscriber::EnvFilter;

mod daemon_client;
mod sandbox;
mod server;
mod workspace;

use daemon_client::DaemonClient;
use sandbox::Sandbox;
use server::VmetteServer;
use workspace::{reap_orphans, WorkspaceState};

/// How often the background task re-scans TMPDIR for orphan
/// `vmette-mcp-<pid>/` dirs from peer instances that died while we
/// were running. Six hours strikes a balance between disk pressure
/// and wakeup overhead for a process that may legitimately sit idle
/// for hours between agent calls.
const REAPER_INTERVAL_SECS: u64 = 6 * 3600;

const DEFAULT_IMAGE: &str = "alpine:3.20";
const DEFAULT_WORKSPACE_CAP: usize = 8;

struct Args {
    default_image: String,
    allow_network: bool,
    workspace_cap: usize,
    kernel: Option<PathBuf>,
    initramfs: Option<PathBuf>,
    vmette_bin: Option<PathBuf>,
    daemon_socket: Option<PathBuf>,
}

fn usage() -> ! {
    eprintln!(
        "vmette-mcp — MCP server exposing vmette as a sandbox for AI agents\n\n\
         usage: vmette-mcp [options]\n\n\
         options:\n  \
           --default-image REF   default rootfs for execute / workspaces (default: {DEFAULT_IMAGE})\n  \
           --allow-network       permit tool calls with network=true (default: deny)\n  \
           --workspace-cap N     max concurrent workspaces per session (default: {DEFAULT_WORKSPACE_CAP})\n  \
           --kernel PATH         override autodiscovered vmlinuz path\n  \
           --initramfs PATH      override autodiscovered initramfs path\n  \
           --vmette PATH         override autodiscovered `vmette` binary path\n  \
           --socket PATH         vmetted socket for desktop_* tools (default ~/Library/Caches/vmette/vmette.sock)\n  \
           -h, --help            this message\n\n\
         the server speaks MCP over stdio; configure your client to launch this binary.\n"
    );
    std::process::exit(2);
}

fn parse_args() -> Args {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut a = Args {
        default_image: DEFAULT_IMAGE.into(),
        allow_network: false,
        workspace_cap: DEFAULT_WORKSPACE_CAP,
        kernel: None,
        initramfs: None,
        vmette_bin: None,
        daemon_socket: None,
    };
    let take = |i: usize, flag: &str| -> String {
        if i + 1 >= raw.len() {
            eprintln!("error: {flag} needs a value");
            usage();
        }
        raw[i + 1].clone()
    };
    let mut i = 0;
    while i < raw.len() {
        match raw[i].as_str() {
            "--default-image" => {
                a.default_image = take(i, "--default-image");
                i += 2;
            }
            "--allow-network" => {
                a.allow_network = true;
                i += 1;
            }
            "--workspace-cap" => {
                let v = take(i, "--workspace-cap");
                a.workspace_cap = v.parse().unwrap_or_else(|_| {
                    eprintln!("error: --workspace-cap expects an integer, got {v:?}");
                    usage();
                });
                i += 2;
            }
            "--kernel" => {
                a.kernel = Some(take(i, "--kernel").into());
                i += 2;
            }
            "--initramfs" => {
                a.initramfs = Some(take(i, "--initramfs").into());
                i += 2;
            }
            "--vmette" => {
                a.vmette_bin = Some(take(i, "--vmette").into());
                i += 2;
            }
            "--socket" => {
                a.daemon_socket = Some(take(i, "--socket").into());
                i += 2;
            }
            "-h" | "--help" => usage(),
            other => {
                eprintln!("unknown arg: {other}");
                usage();
            }
        }
    }
    a
}

#[tokio::main]
async fn main() -> ExitCode {
    // CRITICAL: tracing must go to STDERR, never stdout — stdout is the
    // MCP frame channel and any non-JSON byte on it desyncs the client.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "vmette_mcp=info,rmcp=warn".into()),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_target(false)
        .try_init();

    let args = parse_args();

    let result: Result<()> = async {
        let sandbox = Sandbox::new(args.vmette_bin, args.kernel, args.initramfs)
            .context("initialising vmette sandbox")?;
        let workspaces =
            WorkspaceState::new(args.workspace_cap).context("initialising workspace state")?;
        // Desktop tools route through vmetted; reuse the sandbox's already
        // discovered kernel/initramfs assets for desktop_start.
        let daemon = DaemonClient::new(
            args.daemon_socket,
            sandbox.kernel().to_path_buf(),
            sandbox.initramfs().to_path_buf(),
        );
        let server = VmetteServer::new(
            sandbox,
            workspaces,
            daemon,
            args.default_image,
            args.allow_network,
        );
        tracing::info!(
            allow_network = args.allow_network,
            workspace_cap = args.workspace_cap,
            "vmette-mcp ready; serving stdio"
        );

        // Background reaper: WorkspaceState::new ran reap_orphans once
        // at startup, but a long-lived server should keep collecting
        // peer-instance leaks that accumulate during its uptime.
        //
        // Reliability notes:
        // - `MissedTickBehavior::Skip` so a laptop-sleep doesn't fire N
        //   back-to-back catch-up ticks on wake.
        // - Each iteration runs under `spawn_blocking` (reap_orphans is
        //   sync fs IO) AND its JoinError is logged but the loop
        //   continues. A panic inside reap_orphans no longer silently
        //   kills the reaper for the rest of the server's uptime.
        tokio::spawn(async {
            let mut tick =
                tokio::time::interval(std::time::Duration::from_secs(REAPER_INTERVAL_SECS));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // First tick fires immediately — skip it, startup reap
            // already happened.
            tick.tick().await;
            loop {
                tick.tick().await;
                if let Err(e) = tokio::task::spawn_blocking(reap_orphans).await {
                    tracing::error!(error = %e, "orphan reaper iteration panicked; loop continues");
                }
            }
        });

        let service = server.serve(stdio()).await.context("serving stdio")?;
        service
            .waiting()
            .await
            .context("waiting for client shutdown")?;
        Ok(())
    }
    .await;

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = ?e, "vmette-mcp aborting");
            ExitCode::from(1)
        }
    }
}
