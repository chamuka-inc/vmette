//! Desktop session registry — the daemon's **stateful** subsystem.
//!
//! This is deliberately separate from the stateless per-request dispatch in
//! `main.rs` (which forks a `vmette` subprocess and forgets about it). Here we
//! hold *live* [`vmette::Session`] VMs in-process so a desktop persists across
//! many client requests: a single VM boots Xvfb + a WM + the computer-use
//! agent once, then services screenshot/click/type round-trips until it is
//! explicitly stopped.
//!
//! ## Threading
//!
//! A [`vmette::Session`] is `!Send` — it owns objc2 `Retained` handles and
//! drives its VM on a private dispatch queue. So each session gets its own
//! dedicated OS thread that:
//!   1. boots the `Session`,
//!   2. hands the daemon the `Send` [`SessionClient`] + [`StopHandle`],
//!   3. blocks in `Session::wait()` until the VM ends, then drops the
//!      `Session` (tearing the VM down).
//!
//! The registry itself only ever stores the `Send` handles + the thread's
//! `JoinHandle`, so it lives happily inside the multi-threaded tokio runtime.
//! All VM-control hops off the async threads: blocking `request`/`stop`/`join`
//! calls go through `spawn_blocking`.
//!
//! ## Lifecycle guardrails
//!
//! - **max-live cap**: each session is a ~2 GB VM, so [`start`] refuses past
//!   [`Registry::max_sessions`].
//! - **idle eviction**: [`sweep_idle`] force-stops sessions untouched for
//!   longer than the idle TTL (run periodically by a background task).
//! - **shutdown**: [`stop_all`] tears every session down on daemon exit.
//!
//! Sessions are owned by the registry, not by any one client connection
//! (connections are one-request-each), so a session outlives the connection
//! that created it and is freed only by `desktop_stop`, idle eviction, or
//! daemon shutdown.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context as _, Result};
use rand::Rng;
use vmette::provider::Context;
use vmette::{Action, Config, Session, SessionClient, SessionEnd, StopHandle};
use vmette_proto::Rect;

use vmette_daemon::settle::{Frame, SettleConfig, SettleDetector, SettleState};

/// How often the settle poll re-captures the screen. Needs to be long enough
/// that a playing video actually changes between polls (so churn is detected),
/// short enough that a settled screen is returned promptly.
const SETTLE_POLL_INTERVAL: Duration = Duration::from_millis(120);

/// Default OCI ref for the desktop rootfs image, baked in the same way the
/// MCP/CLI default `python:3.12-alpine` etc. Overridable per `start` request.
pub const DEFAULT_DESKTOP_IMAGE: &str = "ghcr.io/chamuka-inc/vmette-desktop:latest";
/// Default vCPUs for a desktop session when the request omits `vcpus`.
pub const DEFAULT_DESKTOP_VCPUS: u8 = 2;
/// Default RAM (MiB) for a desktop session when the request omits `mem_mib`.
pub const DEFAULT_DESKTOP_MEM_MIB: u64 = 2048;
/// Default settle-poll timeout when `desktop_screenshot_settled` omits it.
pub const DEFAULT_SETTLE_TIMEOUT_MS: u64 = 10_000;

/// A live desktop session's host-side handles. The `Session` itself lives on
/// `thread`; we keep only the `Send` control handles here.
struct Entry {
    client: SessionClient,
    stop: StopHandle,
    /// `Some` until joined; the thread yields the terminal [`SessionEnd`].
    thread: Option<JoinHandle<SessionEnd>>,
    /// Per-session pixel-settle detector. Shared (`Arc<Mutex>`) so a poll loop
    /// can hold it across many screenshots without pinning the registry map
    /// lock. Its rolling churn/stable state carries across requests on purpose:
    /// a region already known to be a video stays known, and `what_changed`
    /// reports damage *since the previous capture*.
    detector: Arc<Mutex<SettleDetector>>,
    last_used: Instant,
}

/// Parameters for booting a desktop session. The kernel + initramfs are the
/// ordinary vmette assets (the desktop-ness comes from the rootfs image +
/// Agent workload), supplied by the client so the daemon stays asset-agnostic.
pub struct StartParams {
    pub kernel: PathBuf,
    pub initramfs: PathBuf,
    pub image: String,
    pub width: u32,
    pub height: u32,
    pub net: bool,
    pub offline: bool,
    pub vcpus: u8,
    pub mem_mib: u64,
}

/// The result of a desktop action: the agent's response header fields plus an
/// optional PNG payload (present for `screenshot`).
pub struct ActionResult {
    pub ok: bool,
    pub error: Option<String>,
    pub x: Option<i32>,
    pub y: Option<i32>,
    pub png: Option<Vec<u8>>,
}

/// The result of a "screenshot once settled" request: the captured frame plus
/// the verdict. `settled` is false only when the poll timed out before the
/// screen quiesced (the most recent frame is still returned, best-effort).
/// `moving` lists regions still animating at capture time (video/spinner).
pub struct SettleResult {
    pub png: Vec<u8>,
    pub settled: bool,
    pub moving: Vec<Rect>,
}

/// The result of a `what_changed` probe: a fresh capture plus the bounding box
/// of what moved since this session's previous capture (`None` if nothing did).
pub struct WhatChangedResult {
    pub png: Vec<u8>,
    pub changed: Option<Rect>,
}

/// Shared registry of live desktop sessions.
pub struct Registry {
    sessions: Mutex<HashMap<String, Entry>>,
    /// In-flight boots that have passed the cap check but aren't yet in
    /// `sessions`. Counted alongside `sessions.len()` against `max_sessions`
    /// so concurrent `start` calls can't over-admit during the (slow) boot.
    reserving: AtomicUsize,
    max_sessions: usize,
    idle_ttl: Duration,
    cache_root: PathBuf,
    guest_helpers_dir: Option<PathBuf>,
}

impl Registry {
    pub fn new(max_sessions: usize, idle_ttl: Duration, cache_root: PathBuf) -> Arc<Self> {
        let guest_helpers_dir = locate_guest_helpers();
        Arc::new(Self {
            sessions: Mutex::new(HashMap::new()),
            reserving: AtomicUsize::new(0),
            max_sessions,
            idle_ttl,
            cache_root,
            guest_helpers_dir,
        })
    }

    /// Boot a desktop session and register it. Returns its id. Blocking
    /// (boots a VM + resolves the rootfs image) — call from `spawn_blocking`.
    pub fn start(&self, params: StartParams) -> Result<String> {
        // Reserve a slot before the (slow) boot. Checking `live + reserving`
        // and bumping `reserving` while holding the map lock makes the cap
        // check-and-reserve atomic, so N concurrent starts can't all pass a
        // bare `len()` check and over-admit. The guard releases the slot if we
        // bail before inserting; on success we commit *after* the insert.
        let reservation = {
            let map = self.sessions.lock().unwrap();
            let in_flight = self.reserving.load(Ordering::Acquire);
            if map.len() + in_flight >= self.max_sessions {
                bail!(
                    "session cap reached ({} live); stop one before starting another",
                    self.max_sessions
                );
            }
            self.reserving.fetch_add(1, Ordering::AcqRel);
            SlotReservation {
                reserving: &self.reserving,
                committed: false,
            }
        };

        // Resolve the rootfs image to a directory via the shared provider
        // registry, exactly as the CLI does for --rootfs.
        let provider = vmette_providers::default_registry();
        let ctx = Context::new(self.cache_root.clone())
            .offline(params.offline)
            .guest_helpers_dir(self.guest_helpers_dir.clone());
        let artifact = provider.resolve(&params.image, &ctx).map_err(|e| {
            if params.image == DEFAULT_DESKTOP_IMAGE {
                anyhow!(
                    "resolving default desktop image {}: {e}\n\
                     If the published image is unreachable, build the rootfs \
                     locally with scripts/build-desktop-image.sh and pass it as \
                     --image tar+file:///path/to/rootfs.tar (see docs/DESKTOP.md).",
                    params.image
                )
            } else {
                anyhow!("resolving desktop image {}: {e}", params.image)
            }
        })?;

        let mut cfg = Config::new(params.kernel, params.initramfs);
        cfg.workload = vmette::WorkloadStrategy::Agent;
        cfg.display_size = (params.width, params.height);
        cfg.net = params.net;
        cfg.vcpus = params.vcpus;
        cfg.mem_mib = params.mem_mib;
        // Writable share: the entrypoint writes Xvfb/openbox logs under /var.
        cfg.set_rootfs_artifact(artifact, false);
        // vsock_port stays Auto (Session resolves it); the cmdline emits
        // vmette.desktop=1 + vmette.display + vmette.vsock_port for the agent.

        // Boot the !Send Session on its own thread; it hands us the Send
        // handles, then owns the VM's lifetime via wait().
        let (tx, rx) =
            std::sync::mpsc::sync_channel::<Result<(SessionClient, StopHandle), String>>(1);
        let thread = std::thread::Builder::new()
            .name("vmette-session".into())
            .spawn(move || match Session::start(&cfg) {
                Ok(session) => {
                    let _ = tx.send(Ok((session.client(), session.stop_handle())));
                    let end = session.wait();
                    // session drops here → VM teardown.
                    end
                }
                Err(e) => {
                    let _ = tx.send(Err(e.to_string()));
                    SessionEnd::Error(format!("session start failed: {e}"))
                }
            })
            .context("spawning session thread")?;

        let (client, stop) = rx
            .recv()
            .context("session thread exited before reporting readiness")?
            .map_err(|e| anyhow!("session failed to start: {e}"))?;

        let detector = Arc::new(Mutex::new(SettleDetector::new(
            params.width,
            params.height,
            SettleConfig::default(),
        )));

        let id = new_session_id();
        {
            let mut map = self.sessions.lock().unwrap();
            map.insert(
                id.clone(),
                Entry {
                    client,
                    stop,
                    thread: Some(thread),
                    detector,
                    last_used: Instant::now(),
                },
            );
        }
        // The session now counts via `sessions.len()`; release the reservation
        // (done after the insert so the slot is never momentarily uncounted).
        reservation.commit();
        Ok(id)
    }

    /// Run one desktop action against a live session. Blocking (round-trips
    /// over vsock) — call from `spawn_blocking`.
    pub fn action(&self, id: &str, action: &Action) -> Result<ActionResult> {
        // Clone the cheap Send client out under the lock, then release it so
        // the (potentially slow) GUI round-trip doesn't serialize the whole
        // registry.
        let client = {
            let mut map = self.sessions.lock().unwrap();
            let entry = map
                .get_mut(id)
                .ok_or_else(|| anyhow!("no such session: {id}"))?;
            entry.last_used = Instant::now();
            entry.client.clone()
        };
        let (header, payload) = client
            .request(action)
            .with_context(|| format!("desktop action on session {id}"))?;
        Ok(ActionResult {
            ok: header.ok,
            error: header.error,
            x: header.x,
            y: header.y,
            png: if payload.is_empty() {
                None
            } else {
                Some(payload)
            },
        })
    }

    /// Poll the desktop until it *settles*, then return that frame plus the
    /// regions still moving. Captures a screenshot, decodes it, feeds the
    /// per-session settle detector, and repeats every [`SETTLE_POLL_INTERVAL`]
    /// until the detector reports `Settled` or `timeout` elapses. On timeout
    /// the most recent frame is still returned with `settled = false`, so the
    /// caller always gets a usable screenshot. Blocking (sleeps + round-trips)
    /// — call from `spawn_blocking`.
    pub fn screenshot_when_settled(&self, id: &str, timeout: Duration) -> Result<SettleResult> {
        let (client, detector) = self.client_and_detector(id)?;
        let deadline = Instant::now() + timeout;
        loop {
            let (header, payload) = client
                .request(&Action::Screenshot)
                .with_context(|| format!("screenshot on session {id}"))?;
            if !header.ok {
                bail!(
                    "agent screenshot failed: {}",
                    header.error.as_deref().unwrap_or("unknown error")
                );
            }
            let frame = decode_png(&payload).context("decoding screenshot PNG")?;
            let state = detector.lock().unwrap().push(frame);
            if let SettleState::Settled { moving } = state {
                return Ok(SettleResult {
                    png: payload,
                    settled: true,
                    moving,
                });
            }
            if Instant::now() >= deadline {
                return Ok(SettleResult {
                    png: payload,
                    settled: false,
                    moving: Vec::new(),
                });
            }
            // Keep the idle timer fresh: a long settle wait is active use, not
            // idleness, so the background sweep must not reap this session
            // out from under the poll.
            self.touch(id);
            std::thread::sleep(SETTLE_POLL_INTERVAL);
        }
    }

    /// Capture one frame and report the bounding box of what changed since this
    /// session's previous capture. Blocking — call from `spawn_blocking`.
    pub fn what_changed(&self, id: &str) -> Result<WhatChangedResult> {
        let (client, detector) = self.client_and_detector(id)?;
        let (header, payload) = client
            .request(&Action::Screenshot)
            .with_context(|| format!("screenshot on session {id}"))?;
        if !header.ok {
            bail!(
                "agent screenshot failed: {}",
                header.error.as_deref().unwrap_or("unknown error")
            );
        }
        let frame = decode_png(&payload).context("decoding screenshot PNG")?;
        let changed = {
            let mut d = detector.lock().unwrap();
            d.push(frame);
            d.last_damage()
        };
        Ok(WhatChangedResult {
            png: payload,
            changed,
        })
    }

    /// Clone the `Send` client + the shared detector out under the map lock,
    /// touching `last_used`, so the (slow) poll round-trips don't serialize the
    /// registry.
    fn client_and_detector(&self, id: &str) -> Result<(SessionClient, Arc<Mutex<SettleDetector>>)> {
        let mut map = self.sessions.lock().unwrap();
        let entry = map
            .get_mut(id)
            .ok_or_else(|| anyhow!("no such session: {id}"))?;
        entry.last_used = Instant::now();
        Ok((entry.client.clone(), entry.detector.clone()))
    }

    /// Best-effort refresh of a session's idle timer, used to keep a session
    /// that is being actively polled (a long [`screenshot_when_settled`] wait)
    /// from being reaped by [`sweep_idle`] mid-poll. No-ops if the session is
    /// already gone — the next round-trip will surface that.
    fn touch(&self, id: &str) {
        if let Some(entry) = self.sessions.lock().unwrap().get_mut(id) {
            entry.last_used = Instant::now();
        }
    }

    /// Stop and remove a session. Blocking (joins the session thread, which
    /// tears the VM down) — call from `spawn_blocking`.
    pub fn stop(&self, id: &str) -> Result<()> {
        let entry = {
            let mut map = self.sessions.lock().unwrap();
            map.remove(id)
                .ok_or_else(|| anyhow!("no such session: {id}"))?
        };
        finish(entry);
        Ok(())
    }

    /// Force-stop every session whose `last_used` is older than the idle TTL.
    /// Returns the ids evicted. Call periodically from a background task.
    pub fn sweep_idle(&self) -> Vec<String> {
        let now = Instant::now();
        let stale: Vec<(String, Entry)> = {
            let mut map = self.sessions.lock().unwrap();
            let ids: Vec<String> = map
                .iter()
                .filter(|(_, e)| now.duration_since(e.last_used) > self.idle_ttl)
                .map(|(id, _)| id.clone())
                .collect();
            ids.into_iter()
                .filter_map(|id| map.remove(&id).map(|e| (id, e)))
                .collect()
        };
        let evicted: Vec<String> = stale.iter().map(|(id, _)| id.clone()).collect();
        for (_, entry) in stale {
            finish(entry);
        }
        evicted
    }

    /// Stop every live session (daemon shutdown). Blocking.
    pub fn stop_all(&self) {
        let entries: Vec<Entry> = {
            let mut map = self.sessions.lock().unwrap();
            map.drain().map(|(_, e)| e).collect()
        };
        for entry in entries {
            finish(entry);
        }
    }

    pub fn len(&self) -> usize {
        self.sessions.lock().unwrap().len()
    }
}

/// Holds a reserved session slot against the cap for the duration of a boot.
/// Dropping it (a boot that bailed before inserting) releases the slot;
/// [`SlotReservation::commit`] releases it explicitly once the session is in
/// the map and counted via `sessions.len()` instead.
struct SlotReservation<'a> {
    reserving: &'a AtomicUsize,
    committed: bool,
}

impl SlotReservation<'_> {
    fn commit(mut self) {
        self.reserving.fetch_sub(1, Ordering::AcqRel);
        self.committed = true;
    }
}

impl Drop for SlotReservation<'_> {
    fn drop(&mut self) {
        if !self.committed {
            self.reserving.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

/// Issue the stop and join the session thread so the VM is fully torn down
/// before we return. `join()` is unbounded: a wedged teardown blocks the
/// caller (the sweeper task or `desktop_stop`) until the thread exits.
fn finish(mut entry: Entry) {
    entry.stop.stop();
    if let Some(handle) = entry.thread.take() {
        let _ = handle.join();
    }
}

/// Decode a screenshot PNG (the agent emits 8-bit RGB) into a [`Frame`] for the
/// settle detector. Accepts RGB or RGBA at 8-bit depth; anything else is an
/// error rather than a silent misread, since the agent's output is known.
fn decode_png(bytes: &[u8]) -> Result<Frame> {
    let decoder = png::Decoder::new(bytes);
    let mut reader = decoder.read_info().context("reading PNG header")?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).context("decoding PNG frame")?;
    if info.bit_depth != png::BitDepth::Eight {
        bail!("unsupported PNG bit depth {:?}", info.bit_depth);
    }
    let channels = match info.color_type {
        png::ColorType::Rgb => 3u8,
        png::ColorType::Rgba => 4u8,
        other => bail!("unsupported PNG color type {other:?}"),
    };
    buf.truncate(info.buffer_size());
    Ok(Frame::new(info.width, info.height, channels, buf))
}

/// Best-effort location of the static guest helpers (vsock-send/runner) so the
/// OCI provider can inject them into resolved rootfs trees, mirroring the CLI.
fn locate_guest_helpers() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(share) = exe
            .parent()
            .and_then(|d| d.parent())
            .map(|p| p.join("share/vmette/guest"))
        {
            if share.join("vsock-send").exists() {
                return Some(share);
            }
        }
    }
    let repo = std::env::current_dir()
        .ok()?
        .join("assets/alpine-rootfs/usr/local/bin");
    repo.join("vsock-send").exists().then_some(repo)
}

/// 16 hex chars of randomness — collision-free enough for a per-host daemon.
fn new_session_id() -> String {
    let mut rng = rand::thread_rng();
    let n: u64 = rng.gen();
    format!("{n:016x}")
}
