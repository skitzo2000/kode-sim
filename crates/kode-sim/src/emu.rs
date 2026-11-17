//! esp-emu, run as a Kode Dot: the P4 with its UART1 bridged to a socket and
//! the board's chips beside it.
//!
//! The chips (the expander and its buttons, touch, LED, haptic, charger,
//! gauge, codec) and the AMOLED are esp-emu's virtual parts, answered here
//! in-process over `--cosim-tcp` by [`crate::chips`]. The microSD is
//! esp-emu's own (`--sd`), and the C5 is a second esp-emu running Kode's C5
//! firmware, bridged to the P4 over SDIO (`--hosted`, [`crate::c5`]). What
//! kemu still does - the USB port, the console, the clock - travels as KD
//! frames over UART1.

use std::io::{self, BufRead, BufReader};
use std::net::TcpListener;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use kodedot_chips::{Battery, Motion};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::tcp::OwnedReadHalf;
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::sync::{mpsc, watch};

use crate::chips;
use crate::proto;

/// The esp-emu kode-sim runs: the first with MIPI-DSI, SDMMC and a C5 as
/// the ESP-Hosted slave. Kept in
/// kode-sim's tools folder, so an esp-emu on PATH (another version,
/// someone else's) is left be.
pub const ESP_EMU_VERSION: &str = "1.1.0";
const INSTALLER: &str = "https://raw.githubusercontent.com/espressif/esp-emulator/main/install.sh";
/// The Kode Dot's PSRAM: 32 MB on the board, 16 MB if esp-emu is left to its default.
const PSRAM: &str = "32M";
/// One CONSOLE_IN frame at most: well inside kemu's 8 KB link frames.
const CONSOLE_CHUNK: usize = 4096;
const UART_WAIT: Duration = Duration::from_secs(20);
const CHIPS_WAIT: Duration = Duration::from_secs(15);
const STOP_WAIT: Duration = Duration::from_secs(5);
const C5_WAIT: Duration = Duration::from_secs(10);
/// The P4's pin on the C5's EN (CONFIG_ESP_HOSTED_SDIO_GPIO_RESET_SLAVE).
const PIN_C5_RESET: &str = "23";

/// kode-sim's cache: built firmware, its own esp-emu, the card, the notes.
/// `$KODE_SIM_CACHE`, or the user's cache ([`user_cache`]).
pub fn cache_dir() -> PathBuf {
    match std::env::var_os("KODE_SIM_CACHE") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => user_cache(&home()),
    }
}

/// kode-sim's folder in the user's caches: ~/Library/Caches/kode-sim on the
/// Mac, $XDG_CACHE_HOME/kode-sim (or ~/.cache/kode-sim) elsewhere.
fn user_cache(home: &std::path::Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        return home.join("Library").join("Caches").join("kode-sim");
    }
    match std::env::var_os("XDG_CACHE_HOME").filter(|p| !p.is_empty()) {
        Some(x) => PathBuf::from(x).join("kode-sim"),
        None => home.join(".cache").join("kode-sim"),
    }
}

/// Where kode-sim keeps the tools it installs (its pinned esp-emu):
/// `$KODE_SIM_TOOLS`, or the user's cache - not `$KODE_SIM_CACHE`, which
/// a test points at a scratch folder and would install esp-emu again into.
pub fn tools_dir() -> PathBuf {
    tools_dir_in(std::env::var_os("KODE_SIM_TOOLS"), &home())
}

fn tools_dir_in(tools: Option<std::ffi::OsString>, home: &std::path::Path) -> PathBuf {
    match tools {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => user_cache(home),
    }
}

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

/// Is `exe` on PATH?
pub fn which(exe: &str) -> bool {
    std::env::var_os("PATH").map(|p| std::env::split_paths(&p).any(|d| d.join(exe).is_file())).unwrap_or(false)
}

/// esp-emu at [`ESP_EMU_VERSION`], installed with Espressif's own installer
/// the first time, into [`tools_dir`]. `$KODE_SIM_ESP_EMU` names another
/// executable to run instead (the tests use it for a stand-in).
pub fn ensure_esp_emu() -> Result<PathBuf, String> {
    if let Some(p) = std::env::var_os("KODE_SIM_ESP_EMU").filter(|p| !p.is_empty()) {
        return Ok(PathBuf::from(p));
    }
    let dir = tools_dir().join(format!("esp-emu-{ESP_EMU_VERSION}"));
    let exe = dir.join("esp-emu");
    if !exe.exists() {
        std::fs::create_dir_all(&dir).map_err(|e| format!("could not make {}: {e}", dir.display()))?;
        let script =
            format!("curl -fsSL {INSTALLER} | sh -s -- --version {ESP_EMU_VERSION} --bin-dir '{}'", dir.display());
        let ok = std::process::Command::new("sh").args(["-c", &script]).status().map(|s| s.success());
        if !matches!(ok, Ok(true)) || !exe.exists() {
            return Err(format!("could not install esp-emu {ESP_EMU_VERSION} into {}", dir.display()));
        }
    }
    Ok(exe)
}

/// Every esp-emu this process started that has not gone yet: what
/// [`stop_all`] stops, whoever started it.
static LIVE: Mutex<Vec<i32>> = Mutex::new(Vec::new());

/// Stop every esp-emu this process still has: SIGTERM, and SIGKILL for
/// one still there after 5 s. For a command stopped half-way (a signal),
/// before it exits - an esp-emu outlives the process that started it.
pub async fn stop_all() {
    let live = || LIVE.lock().unwrap_or_else(PoisonError::into_inner).clone();
    for pid in live() {
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
    let deadline = tokio::time::Instant::now() + STOP_WAIT;
    while !live().is_empty() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    for pid in live() {
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    while !live().is_empty() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // A C5 stopped this way never ran its own clean-up: its socket and its
    // run's flash, this process's alone.
    let mine = format!("ksc5-{}-", std::process::id());
    for dir in [std::env::temp_dir(), PathBuf::from("/tmp")] {
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            if e.file_name().to_string_lossy().starts_with(&mine) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

/// A port nobody listens on now.
pub fn free_port() -> io::Result<u16> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

/// esp-emu's user-mode network, with the Mac's localhost ports forwarded to
/// the Kode Dot's (host, guest) - the only way in from the Mac. With
/// `restrict` nothing goes out but DNS.
pub fn net_arg(forwards: &[(u16, u16)], restrict: bool) -> String {
    let mut parts = vec!["user".to_string()];
    if restrict {
        parts.push("restrict=yes".into());
    }
    parts.extend(forwards.iter().map(|(h, g)| format!("hostfwd=tcp:127.0.0.1:{h}-:{g}")));
    parts.join(",")
}

/// What to run, and how.
#[derive(Clone, Debug)]
pub struct Launch {
    pub firmware: PathBuf,
    /// The app's ELF, for esp-emu's symbols; an image from elsewhere (a
    /// factory flash) may come without one.
    pub elf: Option<PathBuf>,
    pub forwards: Vec<(u16, u16)>,
    pub restrict: bool,
    /// The flash back into the firmware file when it stops, so what was
    /// installed is there next time - and the C5's, its vault with it.
    pub save_state: bool,
    /// The card image in the microSD slot (SDMMC slot 0); None, an empty slot.
    pub sd: Option<PathBuf>,
    /// Does the C5 come up? Without it the P4's SDIO link finds no card, as
    /// on a board whose C5 is dead.
    pub wifi: bool,
}

/// What the page changes on the board itself, for its chips rather than kemu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Input {
    /// BTN_* bits.
    Buttons(u8),
    Touch {
        pressed: bool,
        x: u16,
        y: u16,
    },
    Battery(Battery),
    /// What the Mac's microphone heard: mono i16 LE at the receiver's rate.
    Mic(Vec<u8>),
    /// What the IMU and the magnetometer measure.
    Motion(Motion),
}

/// One running esp-emu: its process, its UART1, its control port and the
/// board's chips beside it.
pub struct Emu {
    pid: i32,
    control: String,
    /// Frames for kemu, written in order by the UART1 task; None once closed.
    uart: Mutex<Option<mpsc::UnboundedSender<Vec<u8>>>>,
    /// The board's inputs, applied in order on a thread of their own: the
    /// chips' socket is blocking, and a page must never wait on it.
    inputs: Mutex<Option<std::sync::mpsc::Sender<Input>>>,
    /// The chip resets asked for: whoever decodes its UART1 forgets the
    /// frame each one cut short.
    resets: AtomicU64,
    /// The board's chips, for the emulated time they last heard: set once
    /// they are attached.
    clock: OnceLock<chips::Handle>,
    /// esp-emu's exit code once it has gone and its console is drained.
    gone: watch::Receiver<Option<i32>>,
    /// esp-emu's console input: lines for a firmware without kemu's console.
    stdin: Mutex<Option<mpsc::UnboundedSender<Vec<u8>>>>,
    /// Has kemu said HELLO: is the console kemu's, over UART1?
    kemu: std::sync::atomic::AtomicBool,
    /// The C5's esp-emu, if it came up: stopped with the P4's.
    c5: Option<C5>,
}

/// The C5's esp-emu, and what it leaves behind.
struct C5 {
    pid: i32,
    exit: watch::Receiver<Option<i32>>,
    /// The socket and the run's flash.
    scratch: Vec<PathBuf>,
}

/// The ends of a launched esp-emu its owner wires up once it holds the [`Emu`].
pub struct Wiring {
    /// UART1, from kemu.
    pub uart: OwnedReadHalf,
    /// What the board does: the LED, a buzz, the board off, the speaker,
    /// the microphone's rate.
    pub chips: mpsc::UnboundedReceiver<chips::Out>,
}

/// esp-emu on `cfg`, with the board's chips attached. Every console line
/// (stdout and stderr, in the order esp-emu wrote them) goes to `on_line`
/// from the moment it starts, from a thread of its own.
pub async fn launch(cfg: &Launch, on_line: Arc<dyn Fn(String) + Send + Sync>) -> Result<(Arc<Emu>, Wiring), String> {
    let exe = ensure_esp_emu()?;
    let port = free_port().map_err(|e| e.to_string())?;
    let control = format!("127.0.0.1:{}", free_port().map_err(|e| e.to_string())?);
    let cosim = format!("127.0.0.1:{}", free_port().map_err(|e| e.to_string())?);
    // An esp-emu that is not there says so before the C5 is built for it.
    if !exe.is_file() {
        return Err(format!("could not start {}: there is no such file", exe.display()));
    }
    // The C5 first: the P4 connects to its socket as it boots.
    let c5 = if cfg.wifi { Some(start_c5(&exe, cfg).await?) } else { None };
    let sock = c5.as_ref().map(|c| c.scratch[0].clone());
    let (out_r, out_w) = io::pipe().map_err(|e| e.to_string())?;
    let out_w2 = out_w.try_clone().map_err(|e| e.to_string())?;
    let mut cmd = Command::new(&exe);
    cmd.args(["--chip", "esp32p4"]).arg("--firmware").arg(&cfg.firmware);
    if let Some(elf) = &cfg.elf {
        cmd.arg("--elf").arg(elf);
    }
    cmd.args(["--psram-size", PSRAM])
        .args(["--uart1-tcp", &format!("127.0.0.1:{port}")])
        .args(["--control-tcp", &control])
        .args(["--cosim-tcp", &cosim])
        // The Kode Dot's console is its USB-Serial/JTAG. A board image also
        // opens a CDC console on the USB-OTG port, which esp-emu would print
        // beside it, every line twice.
        .args(["--console", "usb-jtag"])
        // The P4's network is the C5's, over SDIO; its own stays shut. Left
        // out, esp-emu on Linux opens TAP tap0 when there is one (or makes
        // it, as root), and the P4 would be on the host's network.
        .args(["--net", &net_arg(&[], true)]);
    if let Some(sd) = &cfg.sd {
        cmd.arg("--sd").arg(sd_arg(sd));
    }
    if let Some(sock) = &sock {
        cmd.arg("--hosted").arg(format!("bridge:host:{}", sock.display()));
        cmd.args(["--hosted-reset-gpio", PIN_C5_RESET]);
    }
    if cfg.save_state {
        cmd.arg("--save-state");
    }
    // One pipe for both, merged: a crash's last
    // lines come out in the order they were written.
    cmd.stdin(Stdio::piped()).stdout(out_w).stderr(out_w2).kill_on_drop(true);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            if let Some(c5) = c5 {
                c5.stop().await;
            }
            return Err(format!("could not start {}: {e}", exe.display()));
        }
    };
    drop(cmd); // our copies of the pipe's write end, or its reader never sees EOF
    let pid = child.id().unwrap_or(0) as i32;
    LIVE.lock().unwrap_or_else(PoisonError::into_inner).push(pid);
    let stdin = child.stdin.take().map(|mut w| {
        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        tokio::spawn(async move {
            while let Some(b) = rx.recv().await {
                if w.write_all(&b).await.is_err() || w.flush().await.is_err() {
                    break;
                }
            }
            drop(w);
        });
        tx
    });

    let (drained_tx, mut drained_rx) = watch::channel(false);
    std::thread::spawn(move || {
        let mut r = BufReader::new(out_r);
        let mut raw = Vec::new();
        while matches!(r.read_until(b'\n', &mut raw), Ok(n) if n > 0) {
            let line = String::from_utf8_lossy(&raw).trim_end_matches(['\r', '\n']).to_string();
            on_line(line);
            raw.clear();
        }
        let _ = drained_tx.send(true);
    });
    let (exit_tx, mut exit_rx) = watch::channel(None::<i32>);
    tokio::spawn(async move {
        let rc = match child.wait().await {
            Ok(s) => s.code().unwrap_or_else(|| -s.signal().unwrap_or(0)),
            Err(_) => -1,
        };
        LIVE.lock().unwrap_or_else(PoisonError::into_inner).retain(|p| *p != pid);
        let _ = exit_tx.send(Some(rc));
    });
    // Until it is handed over, a launch dropped half-way (a Ctrl-C during
    // the boot) must not leave esp-emu behind.
    let mut unlaunched = Unlaunched { pid, exit: exit_rx.clone() };
    let (gone_tx, gone) = watch::channel(None::<i32>);
    {
        let mut exit_rx = exit_rx.clone();
        tokio::spawn(async move {
            let rc = exit_rx.wait_for(|r| r.is_some()).await.ok().and_then(|r| *r).unwrap_or(-1);
            let _ = drained_rx.wait_for(|d| *d).await;
            let _ = gone_tx.send(Some(rc));
        });
    }

    let emu_stub = Emu {
        pid,
        control: control.clone(),
        uart: Mutex::new(None),
        inputs: Mutex::new(None),
        resets: AtomicU64::new(0),
        clock: OnceLock::new(),
        gone,
        stdin: Mutex::new(stdin),
        kemu: std::sync::atomic::AtomicBool::new(false),
        c5,
    };
    let fail = |emu: Emu, e: String| async move {
        emu.stop().await;
        Err(e)
    };

    // UART1: esp-emu listens on it once it is up.
    let deadline = tokio::time::Instant::now() + UART_WAIT;
    let stream = loop {
        match TcpStream::connect(("127.0.0.1", port)).await {
            Ok(s) => break s,
            Err(e) => {
                let exited = *exit_rx.borrow_and_update();
                if let Some(rc) = exited {
                    return fail(emu_stub, format!("esp-emu exited with {rc} as it started")).await;
                }
                if tokio::time::Instant::now() > deadline {
                    return fail(emu_stub, format!("esp-emu's UART1 never opened: {e}")).await;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    };
    let _ = stream.set_nodelay(true);
    let (uart_rx, mut uart_wr) = stream.into_split();
    let (uart_tx, mut uart_q) = mpsc::unbounded_channel::<Vec<u8>>();
    tokio::spawn(async move {
        while let Some(frame) = uart_q.recv().await {
            if uart_wr.write_all(&frame).await.is_err() {
                break;
            }
        }
        let _ = uart_wr.shutdown().await;
    });
    *emu_stub.uart.lock().unwrap_or_else(PoisonError::into_inner) = Some(uart_tx);

    // The chips register, then reset the chip: esp-emu does not wait for
    // its devices, and the firmware has to find them from its first
    // instruction. The boot before that one is a board with nothing on its
    // bus, and its HELLO is followed by the real one.
    let (ev_tx, ev_rx) = mpsc::unbounded_channel();
    let sink: chips::Sink = Box::new(move |o| {
        let _ = ev_tx.send(o);
    });
    let attach = tokio::task::spawn_blocking(move || chips::Chips::attach(&cosim, sink));
    let attached = match tokio::time::timeout(CHIPS_WAIT, attach).await {
        Ok(Ok(Ok(c))) => c,
        Ok(Ok(Err(e))) => return fail(emu_stub, chips_failed(&e.to_string())).await,
        Ok(Err(e)) => return fail(emu_stub, chips_failed(&e.to_string())).await,
        Err(_) => return fail(emu_stub, chips_failed("no answer")).await,
    };
    let _ = emu_stub.clock.set(attached.board());
    let board = attached.board();
    std::thread::spawn(move || {
        let _ = attached.run(); // until esp-emu goes
    });
    if let Err(e) = emu_stub.reset().await {
        return fail(emu_stub, format!("esp-emu did not reset with the chips attached: {e}")).await;
    }
    let (in_tx, in_rx) = std::sync::mpsc::channel::<Input>();
    std::thread::spawn(move || {
        for i in in_rx {
            let _ = match i {
                Input::Buttons(m) => board.set_buttons(m),
                Input::Touch { pressed, x, y } => board.set_touch(pressed, x, y),
                Input::Battery(b) => board.set_battery(b),
                Input::Mic(pcm) => {
                    board.heard(&pcm);
                    Ok(())
                }
                Input::Motion(m) => board.set_motion(m),
            };
        }
    });
    *emu_stub.inputs.lock().unwrap_or_else(PoisonError::into_inner) = Some(in_tx);
    unlaunched.pid = 0;
    Ok((Arc::new(emu_stub), Wiring { uart: uart_rx, chips: ev_rx }))
}

/// An esp-emu started but not yet handed over: dropped, it is stopped.
struct Unlaunched {
    pid: i32,
    exit: watch::Receiver<Option<i32>>,
}

impl Drop for Unlaunched {
    fn drop(&mut self) {
        if self.pid > 0 && self.exit.borrow().is_none() {
            unsafe { libc::kill(self.pid, libc::SIGTERM) };
        }
    }
}

/// `--sd`: the image in the Kode Dot's slot, SDMMC slot 0.
fn sd_arg(image: &std::path::Path) -> String {
    format!("{},slot=0", image.display())
}

/// Where the C5's socket and run flash go: short, as a Unix socket's path
/// must be (104 bytes on the Mac), and this run's alone.
fn c5_scratch(name: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let file = format!("ksc5-{}-{n}-{name}", std::process::id());
    let p = std::env::temp_dir().join(&file);
    if p.as_os_str().len() < 100 {
        p
    } else {
        PathBuf::from("/tmp").join(file)
    }
}

/// The C5's esp-emu, listening on its SDIO socket: Kode's C5 firmware, the
/// network behind it (esp-emu's open soft AP, [`crate::c5::SSID`], NAT'd
/// through the Mac, with the forwards), its console into the cache's c5.log.
async fn start_c5(exe: &std::path::Path, cfg: &Launch) -> Result<C5, String> {
    let img = tokio::task::spawn_blocking(crate::c5::ensure).await.map_err(|e| e.to_string())?.map_err(|e| e.0)?;
    let sock = c5_scratch("h.sock");
    let _ = std::fs::remove_file(&sock);
    let run = c5_scratch("flash.bin");
    let kept = cfg.save_state.then(|| cache_dir().join("c5").join("flash.bin"));
    let flash = crate::c5::flash_for(&img, kept.as_deref(), &run).map_err(|e| format!("the C5's flash: {e}"))?;
    let log_path = cache_dir().join("c5.log");
    let _ = std::fs::create_dir_all(cache_dir());
    let log = std::fs::File::create(&log_path).map_err(|e| format!("{}: {e}", log_path.display()))?;
    let log2 = log.try_clone().map_err(|e| e.to_string())?;
    let mut cmd = Command::new(exe);
    cmd.args(["--chip", "esp32c5"]).arg("--firmware").arg(&flash).arg("--elf").arg(&img.elf);
    cmd.arg("--hosted").arg(format!("bridge:slave:{}", sock.display()));
    cmd.args(["--net", &net_arg(&cfg.forwards, cfg.restrict)]);
    cmd.args(["--wifi-ssid", crate::c5::SSID, "--wifi-password", ""]);
    if kept.is_some() {
        cmd.arg("--save-state");
    }
    cmd.stdin(Stdio::null()).stdout(log).stderr(log2).kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| format!("could not start the C5's {}: {e}", exe.display()))?;
    let pid = child.id().unwrap_or(0) as i32;
    LIVE.lock().unwrap_or_else(PoisonError::into_inner).push(pid);
    let (exit_tx, exit) = watch::channel(None::<i32>);
    tokio::spawn(async move {
        let rc = match child.wait().await {
            Ok(s) => s.code().unwrap_or_else(|| -s.signal().unwrap_or(0)),
            Err(_) => -1,
        };
        LIVE.lock().unwrap_or_else(PoisonError::into_inner).retain(|p| *p != pid);
        let _ = exit_tx.send(Some(rc));
    });
    let mut scratch = vec![sock.clone()];
    if kept.is_none() {
        scratch.push(run);
    }
    let c5 = C5 { pid, exit, scratch };
    let deadline = tokio::time::Instant::now() + C5_WAIT;
    while !sock.exists() {
        let exited = *c5.exit.borrow();
        if let Some(rc) = exited {
            c5.stop().await;
            return Err(format!("the C5's esp-emu exited with {rc} as it started: see {}", log_path.display()));
        }
        if tokio::time::Instant::now() > deadline {
            c5.stop().await;
            return Err(format!("the C5's esp-emu never opened its SDIO socket: see {}", log_path.display()));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(c5)
}

impl C5 {
    async fn stop(&self) {
        let mut exit = self.exit.clone();
        let running = exit.borrow().is_none();
        if running && self.pid > 0 {
            unsafe { libc::kill(self.pid, libc::SIGTERM) };
            if tokio::time::timeout(STOP_WAIT, exit.wait_for(|r| r.is_some())).await.is_err() {
                unsafe { libc::kill(self.pid, libc::SIGKILL) };
                let _ = exit.wait_for(|r| r.is_some()).await;
            }
        }
        for p in &self.scratch {
            let _ = std::fs::remove_file(p);
        }
    }
}

fn chips_failed(why: &str) -> String {
    format!("the board's chips did not attach to esp-emu (is it {ESP_EMU_VERSION} or newer?): {why}")
}

impl Emu {
    /// A frame to kemu, over UART1.
    pub fn send(&self, type_: u8, p: &[u8]) {
        self.to_kemu(proto::encode(type_, p));
    }

    /// A change to the board's own inputs, for its chips.
    pub fn input(&self, i: Input) {
        if let Some(tx) = &*self.inputs.lock().unwrap_or_else(PoisonError::into_inner) {
            let _ = tx.send(i);
        }
    }

    fn to_kemu(&self, frame: Vec<u8>) {
        if let Some(tx) = &*self.uart.lock().unwrap_or_else(PoisonError::into_inner) {
            let _ = tx.send(frame);
        }
    }

    pub fn console_in(&self, line: &str) {
        let mut data = line.as_bytes().to_vec();
        data.push(b'\n');
        self.console_bytes(&data);
    }

    /// kemu said HELLO: from now the console is its own, over UART1.
    pub fn kemu_seen(&self) {
        self.kemu.store(true, Ordering::SeqCst);
    }

    /// The chip reset: until the next HELLO the firmware may be one without
    /// kemu (an app a kodeOS built for the simulator booted into).
    pub fn kemu_gone(&self) {
        self.kemu.store(false, Ordering::SeqCst);
    }

    /// Console input, any bytes. Over UART1 when kemu is there rather than
    /// esp-emu's stdin, which hands a line over only once its newline
    /// arrives - no use for a binary payload. A firmware without kemu (a
    /// board image) reads esp-emu's stdin, its USB-Serial/JTAG.
    pub fn console_bytes(&self, data: &[u8]) {
        if !self.kemu.load(Ordering::SeqCst) {
            if let Some(tx) = &*self.stdin.lock().unwrap_or_else(PoisonError::into_inner) {
                let _ = tx.send(data.to_vec());
            }
            return;
        }
        for piece in data.chunks(CONSOLE_CHUNK) {
            self.to_kemu(proto::encode(proto::CONSOLE_IN, piece));
        }
    }

    /// A chip reset, as the board's EN line gives one: the firmware boots
    /// again from flash as it now is, apps installed since included.
    pub async fn reset(&self) -> io::Result<()> {
        self.resets.fetch_add(1, Ordering::SeqCst);
        self.control("reset", Duration::from_secs(5)).await.map(|_| ())
    }

    /// The emulated time, in ns since the chip's last reset, as the board's
    /// chips last heard it; 0 before they are attached.
    pub fn now_ns(&self) -> u64 {
        self.clock.get().map_or(0, |c| c.now_ns())
    }

    /// How many resets have been asked for: when it moves, the frame on
    /// UART1 was cut short.
    pub fn resets(&self) -> u64 {
        self.resets.load(Ordering::SeqCst)
    }

    /// One command on esp-emu's `--control-tcp` and its answer (`ok` or a
    /// value); an `err: ...` answer is an error. Its flash commands
    /// (`read-region`, `write-region`, `erase-region`) act on the flash
    /// the running firmware sees, which the image file is not.
    pub async fn control(&self, cmd: &str, wait: Duration) -> io::Result<String> {
        let s = TcpStream::connect(&self.control).await?;
        let (r, mut w) = s.into_split();
        let mut r = tokio::io::BufReader::new(r);
        w.write_all(format!("{cmd}\n").as_bytes()).await?;
        let mut answer = String::new();
        let n = tokio::time::timeout(wait, r.read_line(&mut answer))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, format!("esp-emu did not answer {cmd:?}")))??;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("esp-emu closed its control port at {cmd:?}"),
            ));
        }
        let answer = answer.trim().to_string();
        if answer.starts_with("err") {
            let word = cmd.split_whitespace().next().unwrap_or("");
            return Err(io::Error::other(format!("esp-emu refused {word}: {answer}")));
        }
        Ok(answer)
    }

    /// The exit code, once esp-emu has gone and its console is drained.
    pub async fn wait(&self) -> i32 {
        let mut gone = self.gone.clone();
        let rc = gone.wait_for(|r| r.is_some()).await.ok().and_then(|r| *r);
        rc.unwrap_or(-1)
    }

    fn has_gone(&self) -> bool {
        self.gone.borrow().is_some()
    }

    /// Stop it: UART1 closed, SIGTERM, and SIGKILL if it is still there
    /// after 5 s. Returns once it has gone and its console is drained.
    pub async fn stop(&self) {
        self.uart.lock().unwrap_or_else(PoisonError::into_inner).take();
        self.inputs.lock().unwrap_or_else(PoisonError::into_inner).take();
        if !self.has_gone() && self.pid > 0 {
            unsafe { libc::kill(self.pid, libc::SIGTERM) };
            if tokio::time::timeout(STOP_WAIT, self.wait()).await.is_err() {
                unsafe { libc::kill(self.pid, libc::SIGKILL) };
            }
        }
        self.wait().await;
        if let Some(c5) = &self.c5 {
            c5.stop().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An Emu whose control port is `control` and nothing else.
    fn on_control(control: String) -> Emu {
        let (_tx, gone) = watch::channel(None);
        Emu {
            pid: 0,
            control,
            uart: Mutex::new(None),
            inputs: Mutex::new(None),
            resets: AtomicU64::new(0),
            clock: OnceLock::new(),
            gone,
            stdin: Mutex::new(None),
            kemu: std::sync::atomic::AtomicBool::new(false),
            c5: None,
        }
    }

    /// A control port that answers every command with `answer`.
    async fn answering(answer: &'static str) -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            while let Ok((s, _)) = l.accept().await {
                let (r, mut w) = s.into_split();
                let mut r = tokio::io::BufReader::new(r);
                let mut line = String::new();
                while r.read_line(&mut line).await.unwrap_or(0) > 0 {
                    let _ = w.write_all(format!("{answer}\n").as_bytes()).await;
                    line.clear();
                }
            }
        });
        addr
    }

    #[test]
    fn the_pinned_esp_emu_stays_put_whatever_the_cache() {
        let home = std::path::Path::new("/h");
        assert_eq!(tools_dir_in(Some("/t".into()), home), PathBuf::from("/t"));
        if cfg!(target_os = "macos") {
            assert_eq!(tools_dir_in(None, home), PathBuf::from("/h/Library/Caches/kode-sim"));
            assert_eq!(tools_dir_in(Some("".into()), home), PathBuf::from("/h/Library/Caches/kode-sim"));
        }
    }

    #[test]
    fn an_emulator_without_its_chips_yet_is_at_time_zero() {
        assert_eq!(on_control(String::new()).now_ns(), 0);
    }

    #[tokio::test]
    async fn a_reset_esp_emu_refuses_is_an_error() {
        let emu = on_control(answering("err: not now").await);
        let e = emu.reset().await.unwrap_err();
        assert!(e.to_string().contains("not now"), "{e}");
        let emu = on_control(answering("ok").await);
        emu.reset().await.unwrap();
    }

    #[test]
    fn the_net_forwards_the_mac_s_ports_into_the_kode_dot() {
        assert_eq!(net_arg(&[], false), "user");
        assert_eq!(
            net_arg(&[(25565, 25565), (8080, 80)], false),
            "user,hostfwd=tcp:127.0.0.1:25565-:25565,hostfwd=tcp:127.0.0.1:8080-:80"
        );
        assert_eq!(net_arg(&[(1, 2)], true), "user,restrict=yes,hostfwd=tcp:127.0.0.1:1-:2");
    }
}
