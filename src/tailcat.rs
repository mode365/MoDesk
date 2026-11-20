//! tailcat (custom mesh tunnel) — in-process integration.
//!
//! The tunnel is provided by libtailcat, the c-shared build of tailcat's
//! `cmd/libtailcat` (upstream C ABI v1). The library is dlopen'ed into the
//! current process — there is no child process, which is what makes the
//! tunnel work on Android (apps may not exec binaries) and keeps one code
//! path on every platform:
//!
//! * controlled side (`ensure_tunnel_serve`): `tc_server_new/start/listen`
//!   expose the local direct-connection listening port through the tunnel
//!   and yield a one-time tailcat address (`tcXXX...`), which is exchanged
//!   with the controlling side through the rendezvous server channel.
//!   Each accepted tunnel connection is bridged to `127.0.0.1:<port>` by a
//!   pair of pump threads.
//!
//! * controlling side (`ensure_tunnel_forward`): `tc_client_new/dial` reach
//!   the peer's served port. The tunnel connection is bridged to a local
//!   `127.0.0.1:<free>` TCP listener, which the remote-control session
//!   connects to — so both control and media flows ride the WireGuard
//!   tunnel while `client.rs` keeps its plain localhost dials.
//!
//! Library resolution:
//!   * Windows: `libtailcat.dll` next to the executable
//!   * macOS:   `libtailcat.dylib` next to the executable
//!   * Linux:   `libtailcat.so` next to the executable
//!   * Android: soname `libtailcat.so` — the app linker namespace resolves
//!     it from nativeLibraryDir (jniLibs packaging)
//! The `TAILCAT_LIB` environment variable overrides the path (debugging /
//! tests).
//!
//! Locked against tailcat main / C ABI v1 (`c14b171db436`, 2026-11-19).

use std::collections::HashMap;
use std::ffi::{c_char, c_void, CStr, CString};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use hbb_common::{anyhow::anyhow, bail, libloading::Library, log, ResultType};

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

// ---- tailcat C ABI (cmd/libtailcat, ABI v1) ----

type TcHandle = u64;
type TcToken = u64;

const TC_OK: i32 = 0;
const TC_EOF: i32 = 6;
const TC_TCP: i32 = 1;
/// Sentinel token: block without a deadline.
const TC_NO_CANCEL: TcToken = 0;

// Seconds a single tunnel setup call may take (server start / listen / dial).
const OP_TIMEOUT_SECS: i64 = 30;
// Accept-poll cadence on the controlled side; also the stop-check interval.
const ACCEPT_POLL_SECS: i64 = 5;
const SERVE_RESTART_COOLDOWN: Duration = Duration::from_secs(15);
const BUF_LEN: usize = 64 * 1024;

type FnAbiVersion = unsafe extern "C" fn() -> u32;
type FnTokenNew = unsafe extern "C" fn(i64, *mut TcToken, *mut *mut c_char) -> i32;
type FnClose = unsafe extern "C" fn(TcHandle, *mut *mut c_char) -> i32;
type FnFree = unsafe extern "C" fn(*mut c_void);
type FnNew = unsafe extern "C" fn(*const c_char, *mut TcHandle, *mut *mut c_char) -> i32;
type FnInfo = unsafe extern "C" fn(TcHandle, TcToken, *mut *mut c_char, *mut *mut c_char) -> i32;
type FnServerStart = unsafe extern "C" fn(TcHandle, TcToken, *mut *mut c_char) -> i32;
type FnServerListen = unsafe extern "C" fn(TcHandle, TcToken, u16, i32, *mut TcHandle, *mut *mut c_char) -> i32;
type FnListenerAccept = unsafe extern "C" fn(TcHandle, TcToken, *mut TcHandle, *mut *mut c_char) -> i32;
type FnClientDial = unsafe extern "C" fn(TcHandle, TcToken, u16, i32, *mut TcHandle, *mut *mut c_char) -> i32;
type FnConnRead = unsafe extern "C" fn(TcHandle, TcToken, *mut c_void, usize, *mut usize, *mut *mut c_char) -> i32;
type FnConnWrite = unsafe extern "C" fn(TcHandle, TcToken, *const c_void, usize, *mut usize, *mut *mut c_char) -> i32;
type FnConnCloseWrite = unsafe extern "C" fn(TcHandle, TcToken, *mut *mut c_char) -> i32;

struct TcLib {
    _lib: Library,
    abi_version: FnAbiVersion,
    token_new: FnTokenNew,
    close: FnClose,
    free: FnFree,
    client_new: FnNew,
    server_new: FnNew,
    info: FnInfo,
    server_start: FnServerStart,
    server_listen: FnServerListen,
    listener_accept: FnListenerAccept,
    client_dial: FnClientDial,
    conn_read: FnConnRead,
    conn_write: FnConnWrite,
    conn_close_write: FnConnCloseWrite,
}

/// Loaded library, or None once loading has failed (missing file, bad ABI);
/// failures are permanent for the life of the process.
static TC_LIB: OnceLock<Option<TcLib>> = OnceLock::new();

fn tc() -> Option<&'static TcLib> {
    TC_LIB
        .get_or_init(|| {
            // Load the library and make its first call (Go runtime init) on a
            // plain dedicated thread with a large stack. The loading thread's
            // context (stack layout, TLS, exception handlers of the host
            // runtime) otherwise leaks into the Go runtime bootstrap, and the
            // Go runtime init / tailscale net stack needs headroom: the P1
            // probe was only stable on a 16MiB stack, and the production host
            // crashed (ACCESS_VIOLATION writing into the Go heap) on the
            // default small stack. A vanilla std::thread is the shape the
            // upstream CLI/tests use and is known-good.
            let handle = std::thread::Builder::new()
                .name("tailcat-loader".into())
                .stack_size(16 * 1024 * 1024)
                .spawn(|| match load_lib() {
                    Ok(lib) => {
                        let abi = unsafe { (lib.abi_version)() };
                        log::info!("libtailcat loaded (ABI {})", abi);
                        Some(lib)
                    }
                    Err(e) => {
                        log::warn!("libtailcat unavailable: {}", e);
                        None
                    }
                });
            match handle {
                Ok(h) => h.join().unwrap_or_else(|_| {
                    log::warn!("libtailcat load thread panicked");
                    None
                }),
                Err(e) => {
                    log::warn!("libtailcat load thread spawn failed: {}", e);
                    None
                }
            }
        })
        .as_ref()
}

fn lib_path() -> Option<std::path::PathBuf> {
    if let Ok(p) = std::env::var("TAILCAT_LIB") {
        let p = std::path::PathBuf::from(p);
        if p.exists() {
            return Some(p);
        }
        log::warn!("TAILCAT_LIB={} does not exist", p.display());
    }
    #[cfg(target_os = "android")]
    {
        // Plain soname: the app's linker namespace resolves it from
        // nativeLibraryDir. (Never derive a path via JNI here.)
        Some(std::path::PathBuf::from("libtailcat.so"))
    }
    #[cfg(not(target_os = "android"))]
    {
        let exe = std::env::current_exe().ok()?;
        let dir = exe.parent()?;
        let name = if cfg!(target_os = "windows") {
            "libtailcat.dll"
        } else if cfg!(target_os = "macos") {
            "libtailcat.dylib"
        } else {
            "libtailcat.so"
        };
        let p = dir.join(name);
        p.exists().then_some(p)
    }
}

fn load_lib() -> ResultType<TcLib> {
    let path = lib_path().ok_or_else(|| anyhow!("libtailcat not found"))?;
    unsafe {
        let lib = Library::new(&path)?;
        // Every symbol below is fetched with its real fn-pointer type, so a
        // missing or wrongly-typed export fails with a precise error naming
        // the exact symbol. (The previous u8-typed existence probe here
        // ALWAYS failed with libloading's IncompatibleSize — "requested type
        // cannot possibly work" — making libtailcat look unavailable in
        // every host: the actual root cause of "the peer does not provide
        // the mesh tunnel". The P1 standalone probe bypassed this path,
        // which is why it never reproduced.)
        let abi_version = *lib.get::<FnAbiVersion>(b"tc_abi_version\0")?;
        if abi_version() != 1 {
            bail!("unsupported tailcat ABI {}", abi_version());
        }
        Ok(TcLib {
            abi_version,
            token_new: *lib.get::<FnTokenNew>(b"tc_token_new\0")?,
            close: *lib.get::<FnClose>(b"tc_close\0")?,
            free: *lib.get::<FnFree>(b"tc_free\0")?,
            client_new: *lib.get::<FnNew>(b"tc_client_new\0")?,
            server_new: *lib.get::<FnNew>(b"tc_server_new\0")?,
            info: *lib.get::<FnInfo>(b"tc_info\0")?,
            server_start: *lib.get::<FnServerStart>(b"tc_server_start\0")?,
            server_listen: *lib.get::<FnServerListen>(b"tc_server_listen\0")?,
            listener_accept: *lib.get::<FnListenerAccept>(b"tc_listener_accept\0")?,
            client_dial: *lib.get::<FnClientDial>(b"tc_client_dial\0")?,
            conn_read: *lib.get::<FnConnRead>(b"tc_conn_read\0")?,
            conn_write: *lib.get::<FnConnWrite>(b"tc_conn_write\0")?,
            conn_close_write: *lib.get::<FnConnCloseWrite>(b"tc_conn_close_write\0")?,
            _lib: lib,
        })
    }
}

/// Take an allocated C string (freeing it); empty when null.
unsafe fn take_cstr(lib: &TcLib, p: *mut c_char) -> String {
    if p.is_null() {
        return Default::default();
    }
    let s = CStr::from_ptr(p).to_string_lossy().to_string();
    (lib.free)(p as *mut c_void);
    s
}

/// Turn a tc_* status + error slot into a Result.
unsafe fn tc_check(lib: &TcLib, code: i32, err: *mut c_char) -> ResultType<()> {
    if code == TC_OK {
        Ok(())
    } else {
        bail!("tailcat code {}: {}", code, take_cstr(lib, err))
    }
}

unsafe fn tc_close_quiet(lib: &TcLib, handle: TcHandle) {
    let mut err: *mut c_char = ptr::null_mut();
    let _ = (lib.close)(handle, &mut err);
    if !err.is_null() {
        (lib.free)(err as *mut c_void);
    }
}

unsafe fn token_new(lib: &TcLib, secs: i64) -> ResultType<TcToken> {
    let mut token: TcToken = 0;
    let mut err: *mut c_char = ptr::null_mut();
    let code = (lib.token_new)(secs * 1_000_000_000, &mut token, &mut err);
    tc_check(lib, code, err)?;
    Ok(token)
}

/// Bridged tunnel connection: one thread per direction. The second side to
/// finish closes the handle; a fatal error closes it immediately so the peer
/// direction's blocked call is interrupted (tc_close interrupts and waits).
fn spawn_bridge(lib: &'static TcLib, conn: TcHandle, local: TcpStream) {
    let Ok(local2) = local.try_clone() else {
        unsafe { tc_close_quiet(lib, conn) };
        return;
    };
    let done = Arc::new(AtomicUsize::new(0));
    {
        let done = done.clone();
        std::thread::spawn(move || pump_tunnel_to_local(lib, conn, local, done));
    }
    std::thread::spawn(move || pump_local_to_tunnel(lib, conn, local2, done));
}

fn bridge_finish(lib: &'static TcLib, conn: TcHandle, done: &AtomicUsize, fatal: bool) {
    if fatal {
        // Interrupt the other direction right away.
        unsafe { tc_close_quiet(lib, conn) };
    }
    let prev = done.fetch_add(1, Ordering::SeqCst);
    if prev >= 1 {
        // Both directions finished: release the connection handle.
        unsafe { tc_close_quiet(lib, conn) };
    }
}

fn pump_tunnel_to_local(lib: &'static TcLib, conn: TcHandle, mut local: TcpStream, done: Arc<AtomicUsize>) {
    let mut buf = vec![0u8; BUF_LEN];
    loop {
        let mut n: usize = 0;
        let mut err: *mut c_char = ptr::null_mut();
        let code = unsafe {
            (lib.conn_read)(conn, TC_NO_CANCEL, buf.as_mut_ptr() as *mut c_void, buf.len(), &mut n, &mut err)
        };
        let msg = unsafe { take_cstr(lib, err) };
        if code == TC_OK && n > 0 {
            if let Err(e) = local.write_all(&buf[..n]) {
                log::debug!("tailcat bridge: local write failed ({})", e);
                bridge_finish(lib, conn, &done, true);
                return;
            }
        }
        if code == TC_EOF {
            // Peer finished sending; we may still be sending.
            let _ = local.shutdown(Shutdown::Write);
            bridge_finish(lib, conn, &done, false);
            return;
        }
        if code != TC_OK {
            if !msg.is_empty() {
                log::debug!("tailcat bridge: read failed ({})", msg);
            }
            bridge_finish(lib, conn, &done, true);
            return;
        }
    }
}

fn pump_local_to_tunnel(lib: &'static TcLib, conn: TcHandle, mut local: TcpStream, done: Arc<AtomicUsize>) {
    let mut buf = vec![0u8; BUF_LEN];
    loop {
        match local.read(&mut buf) {
            Ok(0) => {
                // Local side closed: send our FIN into the tunnel.
                let mut err: *mut c_char = ptr::null_mut();
                let code = unsafe { (lib.conn_close_write)(conn, TC_NO_CANCEL, &mut err) };
                let msg = unsafe { take_cstr(lib, err) };
                if code != TC_OK {
                    log::debug!("tailcat bridge: close_write failed ({})", msg);
                }
                bridge_finish(lib, conn, &done, false);
                return;
            }
            Ok(n) => {
                let mut w: usize = 0;
                let mut err: *mut c_char = ptr::null_mut();
                let code = unsafe {
                    (lib.conn_write)(conn, TC_NO_CANCEL, buf.as_ptr() as *const c_void, n, &mut w, &mut err)
                };
                let msg = unsafe { take_cstr(lib, err) };
                if code != TC_OK {
                    if !msg.is_empty() {
                        log::debug!("tailcat bridge: write failed ({})", msg);
                    }
                    bridge_finish(lib, conn, &done, true);
                    return;
                }
            }
            Err(e) => {
                log::debug!("tailcat bridge: local read failed ({})", e);
                bridge_finish(lib, conn, &done, true);
                return;
            }
        }
    }
}

// ---- controlled side: keep one long-lived serve tunnel while enabled ----

struct TunnelServe {
    server: TcHandle,
    addr: String,
    port: u16,
    stop: Arc<AtomicBool>,
}

impl TunnelServe {
    fn start(lib: &'static TcLib, port: u16) -> ResultType<Self> {
        let mut err: *mut c_char = ptr::null_mut();
        let token = unsafe { token_new(lib, OP_TIMEOUT_SECS) }?;
        let mut server: TcHandle = 0;
        let cfg = CString::new("{}")?;
        let code = unsafe { (lib.server_new)(cfg.as_ptr(), &mut server, &mut err) };
        unsafe { tc_check(lib, code, err) }?;
        let mut listener: TcHandle = 0;
        let res = unsafe {
            err = ptr::null_mut();
            let code = (lib.server_start)(server, token, &mut err);
            tc_check(lib, code, err)?;
            err = ptr::null_mut();
            let code = (lib.server_listen)(server, token, port, TC_TCP, &mut listener, &mut err);
            tc_check(lib, code, err)
        };
        if let Err(e) = res {
            unsafe { tc_close_quiet(lib, server) };
            return Err(e);
        }
        // The tailcat address is only exposed after a successful start.
        let mut out: *mut c_char = ptr::null_mut();
        let code = unsafe { (lib.info)(server, TC_NO_CANCEL, &mut out, &mut err) };
        let info_res = unsafe { tc_check(lib, code, err) };
        let info_json = unsafe { take_cstr(lib, out) };
        info_res?;
        let addr = serde_json::from_str::<serde_json::Value>(&info_json)
            .map_err(|e| anyhow!("tailcat info parse: {}", e))?
            .get("address")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("tailcat info: no address"))?
            .to_string();
        if !addr.starts_with("tc") {
            unsafe { tc_close_quiet(lib, server) };
            bail!("tailcat: unexpected address '{}'", addr);
        }
        unsafe { tc_close_quiet(lib, token) };
        let stop = Arc::new(AtomicBool::new(false));
        {
            let stop = stop.clone();
            std::thread::spawn(move || accept_loop(listener, port, stop));
        }
        log::info!("tailcat serve {} -> {}", port, addr);
        Ok(Self {
            server,
            addr,
            port,
            stop,
        })
    }
}

impl Drop for TunnelServe {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Closing the server also retires its listener (and interrupts the
        // blocked accept); already accepted connections drain on their own.
        if let Some(lib) = tc() {
            unsafe { tc_close_quiet(lib, self.server) };
        }
    }
}

fn accept_loop(listener: TcHandle, port: u16, stop: Arc<AtomicBool>) {
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let Some(lib) = tc() else { return };
        let Ok(token) = (unsafe { token_new(lib, ACCEPT_POLL_SECS) }) else {
            return;
        };
        let mut conn: TcHandle = 0;
        let mut err: *mut c_char = ptr::null_mut();
        let code = unsafe { (lib.listener_accept)(listener, token, &mut conn, &mut err) };
        let msg = unsafe { take_cstr(lib, err) };
        unsafe { tc_close_quiet(lib, token) };
        match code {
            TC_OK => match TcpStream::connect(("127.0.0.1", port)) {
                Ok(local) => spawn_bridge(lib, conn, local),
                Err(e) => {
                    log::warn!("tailcat serve: connect 127.0.0.1:{} failed ({})", port, e);
                    unsafe { tc_close_quiet(lib, conn) };
                }
            },
            // Poll timeout / cancelled: re-check the stop flag.
            3 | 4 => continue,
            // Listener or server closed (stop_tunnel_serve).
            2 => return,
            code => {
                log::warn!("tailcat accept failed: code {} ({})", code, msg);
                return;
            }
        }
    }
}

static TUNNEL_SERVE: OnceLock<Mutex<Option<TunnelServe>>> = OnceLock::new();

fn serve_slot() -> &'static Mutex<Option<TunnelServe>> {
    TUNNEL_SERVE.get_or_init(|| Mutex::new(None))
}

fn get_tunnel_option(key: &str) -> String {
    // See the option-handling note below — the UI process cache is
    // authoritative on the controlling side.
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
/// Used by the punch response to advertise the tunnel.
pub fn current_serve_addr() -> Option<(String, u16)> {
    let guard = serve_slot().lock().ok()?;
    guard.as_ref().map(|s| (s.addr.clone(), s.port))
}

// Time of the last serve start attempt; start attempts are rate-limited so
// repeated failures (no network, no DERRegion) are not retried once per
// direct-server tick (1s).
static SERVE_LAST_START: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();

fn serve_last_start() -> &'static Mutex<Option<Instant>> {
    SERVE_LAST_START.get_or_init(|| Mutex::new(None))
}

/// Ensure a serve tunnel is running for `port`; returns its tailcat address.
/// Restarts the tunnel if `port` changed (the direct-access listen port is
/// configurable). Returns None when the library is missing or the tunnel
/// cannot be established (caller falls back to the normal path).
pub fn ensure_tunnel_serve(port: u16) -> Option<String> {
    let mut guard = serve_slot().lock().ok()?;
    if let Some(s) = guard.as_ref() {
        if s.port == port {
            return Some(s.addr.clone());
        }
    }
    if let Some(last) = *serve_last_start().lock().ok()? {
        if last.elapsed() < SERVE_RESTART_COOLDOWN {
            return None;
        }
    }
    if let Ok(mut last) = serve_last_start().lock() {
        *last = Some(Instant::now());
    }
    // A different port: drop the old serve (Drop closes the server).
    *guard = None;
    let lib = tc()?;
    match TunnelServe::start(lib, port) {
        Ok(t) => {
            let addr = t.addr.clone();
            *guard = Some(t);
            Some(addr)
        }
        Err(e) => {
            log::error!("tailcat serve failed: {}", e);
            None
        }
    }
}

/// Stop the serve tunnel (called when the mesh-tunnel switch turns off).
pub fn stop_tunnel_serve() {
    if let Ok(mut guard) = serve_slot().lock() {
        *guard = None; // Drop closes the server handle.
    }
}

// ---- controlling side: keep one forward tunnel alive across sessions ----

struct TunnelForward {
    client: TcHandle,
    local_port: u16,
    addr: String,
    remote_port: u16,
    /// Cleared by the accept loop when the local listener dies; the next
    /// ensure call then rebuilds the whole forward.
    alive: Arc<AtomicBool>,
}

impl TunnelForward {
    fn start(lib: &'static TcLib, addr: &str, remote_port: u16) -> ResultType<Self> {
        let cfg = CString::new(serde_json::json!({ "address": addr }).to_string())?;
        let mut client: TcHandle = 0;
        let mut err: *mut c_char = ptr::null_mut();
        let code = unsafe { (lib.client_new)(cfg.as_ptr(), &mut client, &mut err) };
        unsafe { tc_check(lib, code, err) }?;
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let local_port = listener
            .local_addr()?
            .port();
        let alive = Arc::new(AtomicBool::new(true));
        {
            let alive = alive.clone();
            std::thread::spawn(move || forward_loop(listener, client, remote_port, alive));
        }
        Ok(Self {
            client,
            local_port,
            addr: addr.to_owned(),
            remote_port,
            alive,
        })
    }
}

impl Drop for TunnelForward {
    fn drop(&mut self) {
        // Retire the client (interrupts pending dials); the local listener
        // and its thread are released by the accept loop's failure path.
        if let Some(lib) = tc() {
            unsafe { tc_close_quiet(lib, self.client) };
        }
    }
}

fn forward_loop(listener: TcpListener, client: TcHandle, remote_port: u16, alive: Arc<AtomicBool>) {
    for local in listener.incoming() {
        match local {
            Ok(tcp) => {
                let Some(lib) = tc() else { break };
                let Ok(token) = (unsafe { token_new(lib, OP_TIMEOUT_SECS) }) else {
                    drop(tcp);
                    break;
                };
                let mut conn: TcHandle = 0;
                let mut err: *mut c_char = ptr::null_mut();
                let code = unsafe {
                    (lib.client_dial)(client, token, remote_port, TC_TCP, &mut conn, &mut err)
                };
                let msg = unsafe { take_cstr(lib, err) };
                unsafe { tc_close_quiet(lib, token) };
                if code == TC_OK {
                    spawn_bridge(lib, conn, tcp);
                } else {
                    log::warn!("tailcat dial {} failed: {}", remote_port, msg);
                }
            }
            Err(e) => {
                log::error!("tailcat forward: local accept failed ({})", e);
                break;
            }
        }
    }
    alive.store(false, Ordering::SeqCst);
}

static TUNNEL_FORWARD: OnceLock<Mutex<Option<TunnelForward>>> = OnceLock::new();

fn forward_slot() -> &'static Mutex<Option<TunnelForward>> {
    TUNNEL_FORWARD.get_or_init(|| Mutex::new(None))
}

/// Ensure a forward tunnel exists for `addr` -> `remote_port`; returns the
/// local port for the session to connect to. The forward outlives a single
/// session and is reused while it matches (the peer's address is stable
/// while its serve tunnel lives), so reconnects skip the DERP round trip.
pub fn ensure_tunnel_forward(addr: &str, remote_port: u16) -> Option<u16> {
    let mut guard = forward_slot().lock().ok()?;
    if let Some(f) = guard.as_ref() {
        if f.addr == addr && f.remote_port == remote_port && f.alive.load(Ordering::SeqCst) {
            return Some(f.local_port);
        }
    }
    // Stale or mismatched: drop the old forward (Drop closes the client).
    *guard = None;
    let lib = tc()?;
    match TunnelForward::start(lib, addr, remote_port) {
        Ok(f) => {
            let local_port = f.local_port;
            *guard = Some(f);
            Some(local_port)
        }
        Err(e) => {
            log::error!("tailcat forward failed: {}", e);
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
) -> &'static Mutex<HashMap<i64, hbb_common::futures::channel::oneshot::Sender<FallbackDecision>>>
{
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Live-network roundtrip through the in-process tunnel: serve exposes a
    /// local echo server, forward maps it back into the same process (both
    /// ends go out through magicsock/DERP like a real session), then a plain
    /// TCP client talks through 127.0.0.1:<forward port>.
    ///
    /// Not part of `cargo test` runs (`#[ignore]`): needs libtailcat next to
    /// the test binary (or $TAILCAT_LIB) and outbound network for the DERP
    /// bootstrap. Run manually:
    ///   set TAILCAT_LIB=path\to\libtailcat.dll
    ///   cargo test --lib --features flutter tailcat -- --ignored --nocapture
    #[test]
    #[ignore = "requires libtailcat + live DERP network"]
    fn tailcat_tunnel_e2e() {
        const ECHO_PORT: u16 = 45999;
        // Must be set before the first tc() call (OnceLock).
        if option_env!("TAILCAT_LIB").is_none() && std::env::var("TAILCAT_LIB").is_err() {
            panic!("set TAILCAT_LIB to a libtailcat build first");
        }

        // Local echo server = the "direct-connection listening port" side.
        let echo = TcpListener::bind(("127.0.0.1", ECHO_PORT)).unwrap();
        std::thread::spawn(move || {
            for c in echo.incoming().flatten() {
                std::thread::spawn(move || {
                    let mut c = c;
                    let _ = c.set_read_timeout(Some(Duration::from_secs(120)));
                    let mut buf = [0u8; 4096];
                    loop {
                        match c.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if c.write_all(&buf[..n]).is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });
            }
        });

        let addr = match ensure_tunnel_serve(ECHO_PORT) {
            Some(a) => a,
            None => {
                // Respect the 15s restart cooldown and try once more.
                std::thread::sleep(SERVE_RESTART_COOLDOWN);
                ensure_tunnel_serve(ECHO_PORT).expect("tailcat serve failed")
            }
        };
        assert!(addr.starts_with("tc"), "bad tailcat address {}", addr);
        println!("tunnel serve address: {}", addr);

        let local_port = ensure_tunnel_forward(&addr, ECHO_PORT).expect("tailcat forward failed");
        println!("tunnel forward local port: {}", local_port);

        let mut client = TcpStream::connect(("127.0.0.1", local_port)).unwrap();
        client.set_read_timeout(Some(Duration::from_secs(90))).unwrap();
        for payload in ["mesh-tunnel-echo-1", "second payload, a bit longer"] {
            client.write_all(payload.as_bytes()).unwrap();
            client.flush().unwrap();
            let mut got = vec![0u8; payload.len()];
            client.read_exact(&mut got).unwrap();
            assert_eq!(got, payload.as_bytes(), "echo mismatch through tunnel");
        }
        println!("tunnel echo roundtrip OK");

        stop_tunnel_serve();
        if let Some(f) = forward_slot().lock().unwrap().take() {
            drop(f);
        }
    }
}
