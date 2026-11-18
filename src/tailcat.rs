//! tailcat (custom mesh tunnel) process management.
//!
//! The tailcat binary is bundled with the app (installed next to the main
//! executable by the Windows CMake install step; `res/tailcat/fetch.ps1`
//! downloads it at build time). Two usages:
//!
//! * controlled side: `tailcat serve <port>` exposes the local listening port
//!   through the tunnel and prints a one-time tailcat address (`tcXXX...`) on
//!   stderr: `# <cat> Server listening with new address: tcXXX`.
//!   The address is exchanged with the controlling side through the
//!   rendezvous server channel.
//!
//! * controlling side: `tailcat forward <tcXXX> 0:<port>` maps the remote port
//!   to a free local port, printing on stderr:
//!   `# forwarding 127.0.0.1:<local> -> remote localhost:<port>`.
//!   The remote-control session then connects to 127.0.0.1:<local> so both
//!   control and media flows ride the WireGuard tunnel.
//!
//! Verified against tailcat v0.7.0 (windows_amd64), 2026-11.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::process::{Child, ChildStderr, Command, Stdio};
use std::sync::atomic::{AtomicI64, AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use hbb_common::{anyhow::anyhow, bail, log, ResultType};

const SPAWN_TIMEOUT_SECS: u64 = 30;

// Option keys shared with the Flutter side (flutter/lib/consts.dart).
const OPT_TUNNEL_SWITCH: &str = "custom-mesh-tunnel";
const OPT_TUNNEL_PROMPT: &str = "custom-mesh-tunnel-prompt";

// Per-connection encrypt/direct decision. Kept in a process-local static on
// purpose: option storage is shared with the service process and can be
// replayed over by the heartbeat strategy sync (server-pushed options), so a
// transient decision must never live there.
static TUNNEL_PENDING: AtomicU8 = AtomicU8::new(0); // 0 unknown, 1 encrypt, 2 direct

/// Record the controlling side's choice for the upcoming (and reconnected)
/// session. Called from main_set_option("mesh-tunnel-pending").
pub fn set_tunnel_pending(encrypt: bool) {
    TUNNEL_PENDING.store(if encrypt { 1 } else { 2 }, Ordering::SeqCst);
}

/// Path of the bundled tailcat binary next to the running executable.
pub fn binary_path() -> Option<std::path::PathBuf> {
    #[cfg(target_os = "android")]
    {
        // On Android the process executable is app_process, so `current_exe`
        // tells nothing about the app layout. tailcat ships as
        // jniLibs/libtailcat.so — the only file pattern the packaging system
        // keeps from the APK — and is extracted (useLegacyPackaging) into the
        // app's nativeLibraryDir, which stays executable on Android 10+.
        return android_binary_path();
    }
    #[cfg(not(target_os = "android"))]
    {
        let exe = std::env::current_exe().ok()?;
        let dir = exe.parent()?;
        let name = if cfg!(windows) { "tailcat.exe" } else { "tailcat" };
        let p = dir.join(name);
        if p.exists() {
            Some(p)
        } else {
            None
        }
    }
}

/// Resolve `<nativeLibraryDir>/libtailcat.so` through the Android
/// ApplicationInfo of the running app.
#[cfg(target_os = "android")]
fn android_binary_path() -> Option<std::path::PathBuf> {
    use jni::objects::JObject;
    let ctx = ndk_context::android_context();
    let vm = unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }.ok()?;
    let mut env = vm.attach_current_thread().ok()?;
    let context = unsafe { JObject::from_raw(ctx.context().cast()) };
    let app_info = env
        .call_method(
            &context,
            "getApplicationInfo",
            "()Landroid/content/pm/ApplicationInfo;",
            &[],
        )
        .ok()?
        .l()
        .ok()?;
    let dir_obj = env
        .get_field(&app_info, "nativeLibraryDir", "Ljava/lang/String;")
        .ok()?
        .l()
        .ok()?;
    let dir: String = env.get_string(&dir_obj.into()).ok()?.into();
    let p = std::path::PathBuf::from(dir).join("libtailcat.so");
    if p.exists() {
        Some(p)
    } else {
        None
    }
}

/// Whether the bundled tailcat binary is available on this machine.
pub fn is_available() -> bool {
    binary_path().is_some()
}

/// Wait for the first stderr line containing `prefix` and return the remainder.
/// A background thread keeps draining the pipe so the child never blocks on a
/// full buffer.
fn wait_for_line(stderr: ChildStderr, prefix: &str) -> ResultType<String> {
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines().flatten() {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(SPAWN_TIMEOUT_SECS);
    loop {
        let remain = deadline.saturating_duration_since(Instant::now());
        if remain.is_zero() {
            break;
        }
        match rx.recv_timeout(remain) {
            Ok(line) => {
                if let Some(pos) = line.find(prefix) {
                    return Ok(line[pos + prefix.len()..].to_string());
                }
            }
            Err(_) => break,
        }
    }
    bail!("tailcat: timed out waiting for '{}'", prefix);
}

/// Spawn a tailcat child with its console hidden. The tunnel children run
/// under the service/session-0 or tray context; without CREATE_NO_WINDOW
/// every (re)start would flash a black console window at the user.
fn spawn_tailcat(bin: &std::path::Path, args: &[&str]) -> ResultType<Child> {
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW
        cmd.creation_flags(0x0800_0000);
    }
    Ok(cmd.spawn()?)
}

/// `tailcat serve <port>`: expose a local TCP port through the tunnel.
pub struct TunnelServe {
    child: Child,
    /// One-time tailcat address (`tcXXX...`) to share with the other side.
    pub addr: String,
}

impl TunnelServe {
    pub fn start(port: u16) -> ResultType<Self> {
        let bin = binary_path().ok_or_else(|| anyhow!("tailcat binary not found"))?;
        let mut child = spawn_tailcat(&bin, &["serve", &port.to_string()])?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("tailcat: no stderr"))?;
        let addr = wait_for_line(stderr, "Server listening with new address:")?
            .trim()
            .to_owned();
        if !addr.starts_with("tc") {
            bail!("tailcat: unexpected address '{}'", addr);
        }
        log::info!("tailcat serve {} -> {}", port, addr);
        Ok(Self { child, addr })
    }
}

impl Drop for TunnelServe {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `tailcat forward <addr> 0:<port>`: map the remote port onto a free local
/// port; the session connects to the returned local port.
pub struct TunnelForward {
    child: Child,
    pub local_port: u16,
}

impl TunnelForward {
    pub fn start(addr: &str, remote_port: u16) -> ResultType<Self> {
        let bin = binary_path().ok_or_else(|| anyhow!("tailcat binary not found"))?;
        let mut child =
            spawn_tailcat(&bin, &["forward", addr, &format!("0:{}", remote_port)])?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("tailcat: no stderr"))?;
        let rest = wait_for_line(stderr, "forwarding 127.0.0.1:")?;
        // "<local> -> remote localhost:<port>"
        let local = rest
            .split("->")
            .next()
            .unwrap_or_default()
            .trim()
            .to_owned();
        let local_port: u16 = local
            .parse()
            .map_err(|_| anyhow!("tailcat: bad local port '{}'", local))?;
        log::info!("tailcat forward {} -> 127.0.0.1:{}", addr, local_port);
        Ok(Self {
            child,
            local_port,
        })
    }
}

impl Drop for TunnelForward {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---- controlled side: keep one long-lived serve tunnel while enabled ----

static TUNNEL_SERVE: OnceLock<Mutex<Option<(TunnelServe, u16)>>> = OnceLock::new();

fn serve_slot() -> &'static Mutex<Option<(TunnelServe, u16)>> {
    TUNNEL_SERVE.get_or_init(|| Mutex::new(None))
}

fn child_alive(child: &mut Child) -> bool {
    child.try_wait().map(|s| s.is_none()).unwrap_or(false)
}

/// Read a mesh-tunnel option on the controlling side (client session, UI
/// process). The Flutter side writes through mainSetOption, which updates
/// ui_interface's OPTIONS cache; hbb_common's Config can lag behind or miss
/// UI-only writes entirely on desktop builds, so the cache is authoritative
/// here.
fn get_tunnel_option(key: &str) -> String {
    crate::ui_interface::get_option(key)
}

/// Whether the mesh-tunnel switch (settings > network) is on, as seen from
/// the CONTROLLED side (rendezvous_mediator). That code runs in the service
/// process, where option updates arrive over ipc and are written straight to
/// hbb_common's Config (the process-local OPTIONS cache is NOT refreshed by
/// the ipc handler), so Config is the authoritative layer there. Default on:
/// the feature ships enabled, "N" is the explicit opt-out.
pub fn tunnel_switch_on() -> bool {
    hbb_common::config::Config::get_option(OPT_TUNNEL_SWITCH) != "N"
}

/// Whether the controlling side should route this remote-control session
/// through the tunnel. Single source of truth shared with the Flutter
/// connect dialog (keep both sides in sync):
///   switch "N"   -> never the tunnel, never a dialog (switch defaults on);
///   prompt "encrypt" -> encrypt by default, no dialog (the only remembered
///                       state; "direct + remember" turns the switch off
///                       instead, legacy values fall through to ask);
///   otherwise    -> ask-mode: use the pending per-connection choice the
///                       dialog just wrote (kept in-process, reconnects reuse).
pub fn should_use_tunnel() -> bool {
    if get_tunnel_option(OPT_TUNNEL_SWITCH) == "N" {
        return false;
    }
    match get_tunnel_option(OPT_TUNNEL_PROMPT).as_str() {
        "encrypt" => true,
        _ => {
            let pending = TUNNEL_PENDING.load(Ordering::SeqCst);
            log::info!(
                "mesh tunnel ask mode, pending decision: {}",
                if pending == 1 { "encrypt" } else { "direct" }
            );
            pending == 1
        }
    }
}

/// The (tailcat address, served port) of the running serve tunnel, if any.
/// The address is tied to the child: once it dies the address is dead too,
/// so report None. Used by the punch response to advertise the tunnel.
pub fn current_serve_addr() -> Option<(String, u16)> {
    let mut guard = serve_slot().lock().ok()?;
    let (t, port) = guard.as_mut()?;
    if child_alive(&mut t.child) {
        Some((t.addr.clone(), *port))
    } else {
        None
    }
}

// Time of the last serve child start attempt; start attempts are rate-
// limited so a serve that dies instantly is not respawned once per
// direct-server tick (1s), spamming processes (and, before the console was
// hidden, flashing windows).
static SERVE_LAST_START: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
const SERVE_RESTART_COOLDOWN: Duration = Duration::from_secs(15);

fn serve_last_start() -> &'static Mutex<Option<Instant>> {
    SERVE_LAST_START.get_or_init(|| Mutex::new(None))
}

/// Ensure a serve tunnel is running for `port`; returns its tailcat address.
/// Restarts the child if it died or `port` changed (the direct-access listen
/// port is configurable). Returns None when the binary is missing or the
/// tunnel cannot be established (caller falls back to the normal path).
pub fn ensure_tunnel_serve(port: u16) -> Option<String> {
    let mut guard = serve_slot().lock().ok()?;
    let reusable = match guard.as_mut() {
        Some((t, served_port)) => child_alive(&mut t.child) && *served_port == port,
        None => false,
    };
    if reusable {
        return Some(guard.as_ref().map(|(t, _)| t.addr.clone()).unwrap());
    }
    if let Some(last) = *serve_last_start().lock().ok()? {
        if last.elapsed() < SERVE_RESTART_COOLDOWN {
            return None;
        }
    }
    if let Ok(mut last) = serve_last_start().lock() {
        *last = Some(Instant::now());
    }
    // Dead child or a different port: drop the old serve (Drop kills it).
    *guard = None;
    match TunnelServe::start(port) {
        Ok(t) => {
            let addr = t.addr.clone();
            *guard = Some((t, port));
            Some(addr)
        }
        Err(e) => {
            log::error!("tailcat serve failed: {}", e);
            *guard = None;
            None
        }
    }
}

/// Stop the serve tunnel (called when the mesh-tunnel switch turns off).
pub fn stop_tunnel_serve() {
    if let Ok(mut guard) = serve_slot().lock() {
        *guard = None; // Drop kills the child
    }
}

// ---- controlling side: keep one forward tunnel alive across sessions ----

static TUNNEL_FORWARD: OnceLock<Mutex<Option<(TunnelForward, String, u16)>>> = OnceLock::new();

fn forward_slot() -> &'static Mutex<Option<(TunnelForward, String, u16)>> {
    TUNNEL_FORWARD.get_or_init(|| Mutex::new(None))
}

/// Ensure a forward tunnel exists for `addr` -> `remote_port`; returns the
/// local port to connect to. The child outlives a single session and is
/// reused while it matches (the peer's address is stable while its serve
/// tunnel lives), so reconnects skip the DERP round trip.
pub fn ensure_tunnel_forward(addr: &str, remote_port: u16) -> Option<u16> {
    let mut guard = forward_slot().lock().ok()?;
    let reusable = match guard.as_mut() {
        Some((f, fwd_addr, fwd_port)) => {
            child_alive(&mut f.child) && fwd_addr == addr && *fwd_port == remote_port
        }
        None => false,
    };
    if reusable {
        return Some(guard.as_ref().map(|(f, _, _)| f.local_port).unwrap());
    }
    *guard = None; // Drop kills the stale child
    match TunnelForward::start(addr, remote_port) {
        Ok(f) => {
            let local_port = f.local_port;
            *guard = Some((f, addr.to_owned(), remote_port));
            Some(local_port)
        }
        Err(e) => {
            log::error!("tailcat forward failed: {}", e);
            *guard = None;
            None
        }
    }
}

// ---- cross-language fallback confirmation (Rust asks, Flutter answers) ----
//
// When the user opted into the mesh tunnel but it cannot be used (the peer
// advertised none, or the forward/connect attempt failed), the client asks
// before falling back to the classic transports. The question travels the
// session event stream; the answer comes back through mainSetOption with the
// transient key "mesh-tunnel-fallback" and value "Y-<id>"/"N-<id>"/"R-<id>",
// so no new FFI surface is needed.

/// The user's answer to a mesh-tunnel fallback confirmation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FallbackDecision {
    /// Skip the tunnel and connect with the classic transports.
    Direct,
    /// End this session (the user chose Cancel).
    Cancel,
    /// Re-attempt the tunnel (the user chose Retry).
    Retry,
}

static FALLBACK_NEXT_ID: AtomicI64 = AtomicI64::new(1);
static FALLBACK_REPLIES: OnceLock<
    Mutex<HashMap<i64, hbb_common::futures::channel::oneshot::Sender<FallbackDecision>>>,
> = OnceLock::new();

fn fallback_replies(
) -> &'static Mutex<HashMap<i64, hbb_common::futures::channel::oneshot::Sender<FallbackDecision>>> {
    FALLBACK_REPLIES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Fresh id for one fallback-confirmation round.
pub fn next_fallback_id() -> i64 {
    FALLBACK_NEXT_ID.fetch_add(1, Ordering::SeqCst)
}

/// Register the waiter BEFORE the UI event goes out, so a fast answer can
/// never race the wait.
pub fn begin_fallback_confirm(
    id: i64,
) -> hbb_common::futures::channel::oneshot::Receiver<FallbackDecision> {
    let (tx, rx) = hbb_common::futures::channel::oneshot::channel();
    fallback_replies().lock().unwrap().insert(id, tx);
    rx
}

/// Drop a registration the UI never took (no interactive handler present).
pub fn cancel_fallback_confirm(id: i64) {
    fallback_replies().lock().unwrap().remove(&id);
}

/// Deliver the UI's answer (from mainSetOption). Unknown or stale ids are
/// ignored: a reply after the timeout, or one for a cancelled round.
pub fn reply_fallback(id: i64, decision: FallbackDecision) {
    if let Some(tx) = fallback_replies().lock().unwrap().remove(&id) {
        let _ = tx.send(decision);
    }
}

/// Wait for the answer; None on timeout or a dropped sender. Callers treat
/// None as Direct ("proceed with the fallback") so the connection never hangs.
pub async fn wait_fallback_confirm(
    rx: hbb_common::futures::channel::oneshot::Receiver<FallbackDecision>,
    timeout_secs: u64,
) -> Option<FallbackDecision> {
    // hbb_common::timeout takes milliseconds.
    match hbb_common::timeout(timeout_secs * 1000, rx).await {
        Ok(Ok(v)) => Some(v),
        _ => None,
    }
}
