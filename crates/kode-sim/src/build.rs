//! `kode-sim build`: an unchanged kode-sdk project, or kodeOS, built with
//! kemu linked in.
//!
//! The project is never touched: it is copied to a cache directory, and
//! only the copy's top CMakeLists.txt gets one line - an include of
//! firmware/kode_sim.cmake right before its project() call. With `board`
//! nothing is included: the image a Kode Dot runs.

use std::path::{Path, PathBuf};

/// Never copied, at the project's top only (a `main/build_info.c` is
/// source): build output, and a project's own sdkconfig, which would win
/// over every default - the emu overlay included. Nor deleted from the
/// copy, so the build's own sdkconfig, managed_components and
/// dependencies.lock are there for the next build, which then neither
/// configures nor solves its dependencies again.
pub const RSYNC_EXCLUDES: [&str; 7] =
    ["/build", "/build_*", "/managed_components", "/.git", "/sdkconfig", "/sdkconfig.old", "/dependencies.lock"];
/// The files kode-sim writes into the copy itself, from the project's -
/// kode-sim included, or pointed at a local kode-sdk - and only when their
/// text changes: a new mtime on any of them and the build configures again.
pub const REWRITTEN: [&str; 3] = ["CMakeLists.txt", "sdkconfig.defaults", "main/idf_component.yml"];
/// What the configuration was last made from (see [`config_stamp`]), next
/// to the copy.
const CONFIG_STAMP: &str = "sdkconfig.stamp";
/// The project's own dependencies.lock as it was last put in the copy.
const PROJECT_LOCK: &str = "dependencies.lock.project";
/// What the build's dependencies were last solved against (see
/// [`deps_stamp`]), next to `src/`.
const DEPS_STAMP: &str = "dependencies.stamp";
/// What the first build of a kode-sdk example says when it has fetched kbsp
/// and wants to be run again.
pub const BOOTSTRAP_AGAIN: &str = "kbsp has just been fetched";

#[derive(Debug, PartialEq, Eq)]
pub struct BuildError(pub String);

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a build leaves.
#[derive(Clone, Debug)]
pub struct BuildResult {
    /// The image the simulator boots.
    pub merged_bin: PathBuf,
    pub elf: PathBuf,
    /// The app image alone: what an install takes.
    pub app_bin: Option<PathBuf>,
}

/// firmware/kode_sim.cmake in the checkout this binary was built from (as
/// web/ is), or `$KODE_SIM_FIRMWARE`/kode_sim.cmake.
pub fn kode_sim_cmake() -> PathBuf {
    let dir = match std::env::var_os("KODE_SIM_FIRMWARE") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../firmware")),
    };
    resolve(&dir).join("kode_sim.cmake")
}

/// ESP-IDF's environment, sourced before each idf.py: what Espressif's
/// installer leaves (`~/.espressif/tools/activate_idf_v6.1.sh`), or
/// `$IDF_PATH/export.sh` for an ESP-IDF set up another way.
pub fn idf_env() -> Result<PathBuf, BuildError> {
    idf_env_in(&crate::emu::home(), std::env::var_os("IDF_PATH").as_deref())
}

pub fn idf_env_in(home: &Path, idf_path: Option<&std::ffi::OsStr>) -> Result<PathBuf, BuildError> {
    let activate = home.join(".espressif").join("tools").join("activate_idf_v6.1.sh");
    if activate.is_file() {
        return Ok(activate);
    }
    let export = idf_path.filter(|p| !p.is_empty()).map(|p| Path::new(p).join("export.sh"));
    if let Some(export) = &export {
        if export.is_file() {
            return Ok(export.clone());
        }
    }
    let also = match &export {
        Some(e) => format!(", nor {}", e.display()),
        None => " and IDF_PATH is not set".into(),
    };
    Err(BuildError(format!(
        "no ESP-IDF to build with: there is no {}{also}. Install ESP-IDF v6.1 with Espressif's installer \
         (https://docs.espressif.com/projects/esp-idf/en/v6.1/esp32p4/get-started/), or set IDF_PATH to an \
         ESP-IDF v6.1 checkout with its tools installed (./install.sh esp32p4)",
        activate.display()
    )))
}

/// Write `text` to `p` unless it says that already: its mtime is what the
/// build watches.
fn write_if_changed(p: &Path, text: &[u8]) -> Result<(), BuildError> {
    if std::fs::read(p).is_ok_and(|now| now == text) {
        return Ok(());
    }
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).map_err(|e| BuildError(format!("could not make {}: {e}", dir.display())))?;
    }
    std::fs::write(p, text).map_err(|e| BuildError(format!("{}: {e}", p.display())))
}

/// What a configuration is made from, hashed: the project's
/// sdkconfig.defaults* (as the copy `src` has them), the emu overlay unless
/// for the board, the kode-sdk it was pointed at. When it changes, the
/// sdkconfig made from it before goes.
pub fn config_stamp(src: &Path, sdk: Option<&Path>, board: bool) -> String {
    use sha1::{Digest, Sha1};
    let mut h = Sha1::new();
    h.update(format!("board={board}\nsdk={}\n", sdk.map(|s| s.display().to_string()).unwrap_or_default()));
    let mut defaults: Vec<PathBuf> = std::fs::read_dir(src)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("sdkconfig.defaults")))
        .collect();
    defaults.sort();
    let overlay = (!board).then(|| kode_sim_cmake().with_file_name("sdkconfig.emu"));
    for f in defaults.iter().chain(overlay.iter()) {
        let body = std::fs::read(f).unwrap_or_default();
        h.update(format!("{}\n{}\n", f.file_name().unwrap_or_default().to_string_lossy(), body.len()));
        h.update(&body);
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// The kode-sdk's components as the component manager solves them: each
/// one's name and idf_component.yml, hashed. A kode-sdk pulled in the same
/// place changes it (a component gone, a version on), and a dependencies.lock
/// solved against the old one points where nothing is.
pub fn deps_stamp(sdk: Option<&Path>) -> String {
    use sha1::{Digest, Sha1};
    let Some(sdk) = sdk else { return String::new() };
    let mut h = Sha1::new();
    h.update(format!("sdk={}\n", sdk.display()));
    let mut manifests: Vec<PathBuf> = std::fs::read_dir(sdk.join("components"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path().join("idf_component.yml"))
        .filter(|p| p.is_file())
        .collect();
    manifests.sort();
    for m in manifests {
        let body = std::fs::read(&m).unwrap_or_default();
        h.update(format!("{}\n{}\n", m.display(), body.len()));
        h.update(&body);
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// A path made absolute, with what exists of it resolved, as Python's
/// `Path.resolve()` has it.
pub fn resolve(p: &Path) -> PathBuf {
    if let Ok(r) = std::fs::canonicalize(p) {
        return r;
    }
    let abs = if p.is_absolute() { p.to_path_buf() } else { std::env::current_dir().unwrap_or_default().join(p) };
    match (abs.parent(), abs.file_name()) {
        (Some(dir), Some(name)) if dir != abs => resolve(dir).join(name),
        _ => abs,
    }
}

fn read(p: &Path) -> Result<String, BuildError> {
    std::fs::read_to_string(p).map_err(|e| BuildError(format!("{}: {e}", p.display())))
}

fn write(p: &Path, text: &str) -> Result<(), BuildError> {
    std::fs::write(p, text).map_err(|e| BuildError(format!("{}: {e}", p.display())))
}

/// The text of `cmakelists` with `cmake` included before its project():
/// the first line that starts with it, not one indented or commented out.
pub fn inject_with(cmakelists: &Path, cmake: &Path) -> Result<String, BuildError> {
    let text = read(cmakelists)?;
    let lines: Vec<&str> = text.lines().collect();
    let Some(i) = lines.iter().position(|l| l.starts_with("project(")) else {
        return Err(BuildError(format!(
            "{}: no top-level project() call to put kode-sim in front of",
            cmakelists.display()
        )));
    };
    let include = format!("include(\"{}\")", cmake.display());
    let mut out: Vec<&str> = lines[..i].to_vec();
    out.push(&include);
    out.extend_from_slice(&lines[i..]);
    Ok(out.join("\n") + "\n")
}

pub fn inject(cmakelists: &Path) -> Result<String, BuildError> {
    inject_with(cmakelists, &kode_sim_cmake())
}

/// The project's place in `cache`: its name and the first 8 hex digits of
/// the SHA-1 of its resolved path, as kode-sim has always named it, so
/// the builds made before are found.
pub fn work_dir(cache: &Path, project: &Path, board: bool) -> PathBuf {
    use sha1::{Digest, Sha1};
    let project = resolve(project);
    let digest = Sha1::digest(project.to_string_lossy().as_bytes());
    let tag: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
    let name = project.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    cache.join(format!("{name}-{tag}{}", if board { "-board" } else { "" }))
}

/// The work dir's lock, if no other build holds it: one build at a time
/// in a work dir, as two (two terminals, two checkouts of kode-sim on one
/// project) would each sync, configure and link over the other's.
fn try_lock_work(work: &Path) -> Result<Option<std::fs::File>, BuildError> {
    use std::os::fd::AsRawFd;
    std::fs::create_dir_all(work).map_err(|e| BuildError(format!("could not make {}: {e}", work.display())))?;
    let path = work.join(".build.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| BuildError(format!("{}: {e}", path.display())))?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(Some(file));
    }
    Ok(None)
}

/// The work dir's lock, waiting for another build to let it go.
fn lock_work(work: &Path, project: &Path) -> Result<std::fs::File, BuildError> {
    use std::os::fd::AsRawFd;
    if let Some(f) = try_lock_work(work)? {
        return Ok(f);
    }
    eprintln!("kode-sim: waiting for another build of {} to finish", project.display());
    let path = work.join(".build.lock");
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .map_err(|e| BuildError(format!("{}: {e}", path.display())))?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(BuildError(format!("{}: {}", path.display(), std::io::Error::last_os_error())));
    }
    Ok(file)
}

/// The project's copy, ready to build: kode-sim included, and the
/// sdkconfig of the last build kept only while what it was made from - the
/// project's defaults, the emu overlay, the kode-sdk - is unchanged. What
/// did not change keeps its mtime, so a build of an unchanged project
/// neither configures nor solves its dependencies again. With `board`, as
/// for the board: kemu left out, the image a Kode Dot runs.
pub fn prepare(cache: &Path, project: &Path, sdk: Option<&Path>, board: bool) -> Result<PathBuf, BuildError> {
    let project = resolve(project);
    let top = project.join("CMakeLists.txt");
    if !top.exists() {
        return Err(BuildError(format!("{}: not found - is {} an ESP-IDF project?", top.display(), project.display())));
    }
    let work = work_dir(cache, &project, board);
    let src = work.join("src");
    std::fs::create_dir_all(&src).map_err(|e| BuildError(format!("could not make {}: {e}", src.display())))?;
    let mut rsync = std::process::Command::new("rsync");
    rsync.args(["-a", "--delete"]);
    rsync.args(RSYNC_EXCLUDES.iter().chain(&REWRITTEN).map(|e| format!("--exclude={}", anchored(e))));
    rsync.arg(format!("{}/", project.display())).arg(format!("{}/", src.display()));
    match rsync.status() {
        Ok(s) if s.success() => {}
        Ok(s) => return Err(BuildError(format!("rsync of {} into {} failed ({s})", project.display(), src.display()))),
        Err(e) => return Err(BuildError(format!("could not run rsync: {e}"))),
    }
    let sdk = sdk.map(resolve);
    for name in REWRITTEN {
        let (from, to) = (project.join(name), src.join(name));
        if !from.is_file() {
            let _ = std::fs::remove_file(&to);
            continue;
        }
        let mut text = std::fs::read(&from).map_err(|e| BuildError(format!("{}: {e}", from.display())))?;
        if name == "CMakeLists.txt" && !board {
            text = inject(&from)?.into_bytes();
        }
        if let Some(sdk) = &sdk {
            text = local_sdk(name, &String::from_utf8_lossy(&text), sdk).into_bytes();
        }
        write_if_changed(&to, &text)?;
    }
    // The project's own dependencies.lock, when it has one, and only when
    // it changed: in between, the one the build solved.
    let lock = project.join("dependencies.lock");
    if let Ok(theirs) = std::fs::read(&lock) {
        let seen = work.join(PROJECT_LOCK);
        if std::fs::read(&seen).ok().as_deref() != Some(&theirs[..]) {
            write_if_changed(&src.join("dependencies.lock"), &theirs)?;
            write_if_changed(&seen, &theirs)?;
        }
    }
    // Solved against another kode-sdk than this one, or another
    // configuration (a dependency's rules may ask a Kconfig option, as
    // kdrv's esp_cam_sensor asks KODE_TEST_DOT): solved again, from the
    // project's own lock when it has one.
    let resolve = || -> Result<(), BuildError> {
        match std::fs::read(&lock) {
            Ok(theirs) => write_if_changed(&src.join("dependencies.lock"), &theirs)?,
            Err(_) => {
                let _ = std::fs::remove_file(src.join("dependencies.lock"));
            }
        }
        Ok(())
    };
    let deps = deps_stamp(sdk.as_deref());
    let deps_file = work.join(DEPS_STAMP);
    if std::fs::read_to_string(&deps_file).ok().as_deref() != Some(deps.as_str()) {
        resolve()?;
        write(&deps_file, &deps)?;
    }
    let stamp = config_stamp(&src, sdk.as_deref(), board);
    let stamp_file = work.join(CONFIG_STAMP);
    if std::fs::read_to_string(&stamp_file).ok().as_deref() != Some(stamp.as_str()) {
        for stale in ["sdkconfig", "sdkconfig.old"] {
            let _ = std::fs::remove_file(src.join(stale));
        }
        // The component manager weighs those rules as CMake configures:
        // configured again, not only rebuilt.
        resolve()?;
        let _ = std::fs::remove_file(work.join("out").join("CMakeCache.txt"));
        write(&stamp_file, &stamp)?;
    }
    Ok(src)
}

/// An rsync pattern for a path from the project's top: there only.
fn anchored(p: &str) -> String {
    if p.starts_with('/') {
        p.to_string()
    } else {
        format!("/{p}")
    }
}

/// `idf.py ARGS` in the ESP-IDF environment `script` sets up, its output
/// into `log` - the script's too, when it fails.
pub(crate) fn idf(script: &Path, args: &[String], log: &Path, append: bool) -> Result<bool, BuildError> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(append)
        .truncate(!append)
        .open(log)
        .map_err(|e| BuildError(format!("{}: {e}", log.display())))?;
    let err = file.try_clone().map_err(|e| BuildError(e.to_string()))?;
    // Paths as arguments, never spliced into the script; and none of them
    // in sight of the script sourced, which may read its own.
    const SH: &str = r#"s=$1; a=$2; shift 2; args=("$@"); set --
. "$s" >"$a" 2>&1 || { st=$?; echo "kode-sim: ESP-IDF's environment did not activate ($s exited $st):"; cat "$a"; exit 1; }
command -v idf.py >/dev/null || { echo "kode-sim: ESP-IDF's environment did not activate ($s left no idf.py on PATH):"; cat "$a"; exit 1; }
idf.py "${args[@]}""#;
    let activate_log = log.with_extension("activate.log");
    let status = std::process::Command::new("bash")
        .args(["-c", SH, "bash"])
        .arg(script)
        .arg(&activate_log)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(file)
        .stderr(err)
        .status()
        .map_err(|e| BuildError(format!("could not run bash: {e}")))?;
    Ok(status.success())
}

/// Build `project` in kode-sim's cache: `idf.py build merge-bin`, once
/// more when a kode-sdk example has just fetched its kbsp.
pub fn build(project: &Path, sdk: Option<&Path>, board: bool) -> Result<BuildResult, BuildError> {
    let project = resolve(project);
    let env = idf_env()?;
    let cache = crate::emu::cache_dir();
    let _lock = lock_work(&work_dir(&cache, &project, board), &project)?;
    let src = prepare(&cache, &project, sdk, board)?;
    let work = src.parent().unwrap().to_path_buf();
    let out = work.join("out");
    let log = work.join("build.log");
    let args: Vec<String> = ["-C".into(), src.display().to_string(), "-B".into(), out.display().to_string()]
        .into_iter()
        .chain(["build".into(), "merge-bin".into()])
        .collect();
    let mut ok = idf(&env, &args, &log, false)?;
    if !ok && String::from_utf8_lossy(&std::fs::read(&log).unwrap_or_default()).contains(BOOTSTRAP_AGAIN) {
        ok = idf(&env, &args, &log, true)?;
    }
    if !ok {
        let text = String::from_utf8_lossy(&std::fs::read(&log).unwrap_or_default()).into_owned();
        let lines: Vec<&str> = text.lines().collect();
        let tail: Vec<String> =
            lines[lines.len().saturating_sub(30)..].iter().map(|l| l.chars().take(300).collect()).collect();
        return Err(BuildError(format!(
            "build of {} failed (full log: {}):\n{}",
            project.display(),
            log.display(),
            tail.join("\n")
        )));
    }
    let desc = json_file(&out.join("project_description.json"))?;
    let field = |k: &str| {
        desc.get(k)
            .and_then(|v| v.as_str())
            .map(|v| out.join(v))
            .ok_or_else(|| BuildError(format!("{}: no {k}", out.join("project_description.json").display())))
    };
    let app_bin = field("app_bin")?;
    Ok(BuildResult { merged_bin: bootable(&out, &app_bin)?, elf: field("app_elf")?, app_bin: Some(app_bin) })
}

/// A build as the commands start one: said on the terminal, and its
/// failure said there too, with the exit code it means.
pub fn built(project: &Path, sdk: Option<&Path>, board: bool) -> Result<BuildResult, i32> {
    eprintln!("kode-sim: building {} ...", project.display());
    match build(project, sdk, board) {
        Ok(fw) => {
            eprintln!("kode-sim: built {}", fw.merged_bin.display());
            Ok(fw)
        }
        Err(e) => {
            eprintln!("kode-sim: {e}");
            Err(1)
        }
    }
}

/// `kode-sim build`; with `json`, what it built on stdout too (see [`paths_json`]).
pub fn command(project: &Path, sdk: Option<&Path>, board: bool, json: bool) -> i32 {
    match built(project, sdk, board) {
        Ok(fw) => {
            if let Some(app) = &fw.app_bin {
                eprintln!(
                    "kode-sim: to install it in a running kodeOS: kcli.py -p <its serial port> app-install {}",
                    app.display()
                );
            }
            if json {
                println!("{}", paths_json(&fw));
            }
            0
        }
        Err(code) => code,
    }
}

/// What a build left, for a script: one JSON object on one line.
pub fn paths_json(fw: &BuildResult) -> String {
    let path = |p: &Path| serde_json::Value::String(p.to_string_lossy().into_owned());
    serde_json::json!({
        "merged_bin": path(&fw.merged_bin),
        "elf": path(&fw.elf),
        "app_bin": fw.app_bin.as_deref().map(path),
    })
    .to_string()
}

fn json_file(p: &Path) -> Result<serde_json::Value, BuildError> {
    serde_json::from_str(&read(p)?).map_err(|e| BuildError(format!("{}: {e}", p.display())))
}

/// One entry of an ESP-IDF partition table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Partition {
    pub ptype: u8,
    pub subtype: u8,
    pub offset: u32,
    pub size: u32,
    pub name: String,
}

/// The entries of a partition table, up to the first that is not one (the
/// MD5 entry, or the erased flash after the last).
pub fn partitions(table: &[u8]) -> Vec<Partition> {
    let mut out = vec![];
    for e in table.as_chunks::<32>().0 {
        if e[..2] != [0xaa, 0x50] {
            break;
        }
        let u32le = |i: usize| u32::from_le_bytes([e[i], e[i + 1], e[i + 2], e[i + 3]]);
        let name = &e[12..28];
        let end = name.iter().position(|&b| b == 0).unwrap_or(name.len());
        out.push(Partition {
            ptype: e[2],
            subtype: e[3],
            offset: u32le(4),
            size: u32le(8),
            name: String::from_utf8_lossy(&name[..end]).into_owned(),
        });
    }
    out
}

/// The image the simulator boots: out/merged-binary.bin, laid out again
/// when the build meant it for the mailbox. kode-sdk builds an app for
/// app_pending, where kodeOS adopts it, so its merged image is the app
/// alone at the mailbox and has no bootloader; here the app boots on its
/// own, from the factory slot, as on a board with nothing else on it.
pub fn bootable(out: &Path, app_bin: &Path) -> Result<PathBuf, BuildError> {
    let merged = out.join("merged-binary.bin");
    let flasher = json_file(&out.join("flasher_args.json"))?;
    let has_bootloader = flasher
        .get("flash_files")
        .and_then(|f| f.as_object())
        .is_some_and(|f| f.values().any(|v| v.as_str().is_some_and(|s| s.starts_with("bootloader"))));
    if has_bootloader {
        return Ok(merged);
    }
    let cfg = json_file(&out.join("config").join("sdkconfig.json"))?;
    let offset = |k: &str| {
        cfg.get(k)
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .ok_or_else(|| BuildError(format!("{}: no {k} in config/sdkconfig.json", out.display())))
    };
    let bytes = |p: PathBuf| std::fs::read(&p).map_err(|e| BuildError(format!("{}: {e}", p.display())));
    let table = bytes(out.join("partition_table").join("partition-table.bin"))?;
    let mut found: Option<Partition> = None;
    for p in partitions(&table) {
        if p.ptype == 0 && (found.is_none() || p.subtype == 0x00) {
            found = Some(p); // an app slot; factory first
        }
    }
    let name = app_bin.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let Some(part) = found else {
        return Err(BuildError(format!("{}: the partition table has no app slot to boot {name} from", out.display())));
    };
    let slot = part.offset as usize;
    let app = bytes(app_bin.to_path_buf())?;
    if app.len() > part.size as usize {
        return Err(BuildError(format!(
            "{name} is {} bytes and the {} partition it would boot from holds {} bytes: too big to boot on its own. \
             Run kodeOS in the simulator (kode-sim run <KodeOS>) and `kode-sim install` the app into it, \
             as on the board",
            app.len(),
            part.name,
            part.size
        )));
    }
    let boot = bytes(out.join("bootloader").join("bootloader.bin"))?;
    let mut image = vec![0xffu8; slot + app.len()];
    for (at, data) in
        [(offset("BOOTLOADER_OFFSET_IN_FLASH")?, &boot), (offset("PARTITION_TABLE_OFFSET")?, &table), (slot, &app)]
    {
        if at + data.len() > image.len() {
            image.resize(at + data.len(), 0xff);
        }
        image[at..at + data.len()].copy_from_slice(data);
    }
    std::fs::write(&merged, image).map_err(|e| BuildError(format!("{}: {e}", merged.display())))?;
    Ok(merged)
}

/// `  kodediy/<name>:` alone on its line: the name.
fn kodediy_dep(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("  kodediy/")?;
    let (name, tail) = rest.split_once(':')?;
    let word = |c: char| c.is_alphanumeric() || c == '_' || c == '-';
    (!name.is_empty() && name.chars().all(word) && tail.trim().is_empty()).then_some(name)
}

/// Every `kodediy/<name>` dependency of a component manifest as an
/// override_path to that component of `sdk`; the rest as it is.
pub fn override_kodediy(text: &str, sdk: &Path) -> String {
    let mut out: Vec<String> = vec![];
    let mut skipping = false;
    for line in text.lines() {
        if let Some(name) = kodediy_dep(line) {
            out.push(format!("  kodediy/{name}:"));
            out.push(format!("    override_path: {}", sdk.join("components").join(name).display()));
            skipping = true;
            continue;
        }
        if skipping && (line.starts_with("    ") || line.trim().is_empty()) {
            continue; // the git/path/version lines it replaces
        }
        skipping = false;
        out.push(line.to_string());
    }
    out.join("\n") + "\n"
}

/// For an example pinned by git, the text of `name` (one of [`REWRITTEN`])
/// using a local kode-sdk checkout instead: every kodediy/<name>
/// dependency of main/idf_component.yml becomes an override_path to that
/// component of `sdk`, and the paths into the managed kbsp point at its kbsp.
fn local_sdk(name: &str, text: &str, sdk: &Path) -> String {
    if name == "main/idf_component.yml" {
        return override_kodediy(text, sdk);
    }
    let kbsp = sdk.join("components").join("kbsp").display().to_string();
    text.replace("${CMAKE_CURRENT_LIST_DIR}/managed_components/kodediy__kbsp", &kbsp)
        .replace("managed_components/kodediy__kbsp", &kbsp)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::fs;

    const BUTTONS_LED: &str = "cmake_minimum_required(VERSION 3.22)
set(IDF_TARGET esp32p4)
include($ENV{IDF_PATH}/tools/cmake/project.cmake)
project(buttons_led)

if(NOT KBSP_BOOTSTRAPPED)
    message(FATAL_ERROR \"again\")
endif()
";

    /// A folder of the test's own, gone when it ends.
    pub(crate) struct Tmp(pub PathBuf);

    impl Tmp {
        pub fn new(tag: &str) -> Tmp {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static N: AtomicUsize = AtomicUsize::new(0);
            let p = std::env::temp_dir().join(format!(
                "kode-sim-test-{tag}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::SeqCst)
            ));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).unwrap();
            Tmp(fs::canonicalize(&p).unwrap())
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn one_build_at_a_time_in_a_work_dir() {
        let t = Tmp::new("lock");
        let work = t.0.join("app-12345678");
        let first = try_lock_work(&work).unwrap().expect("the first build takes the work dir");
        assert!(try_lock_work(&work).unwrap().is_none(), "a second build waits while the first holds it");
        drop(first);
        // (A child another test forks holds the descriptor until its exec.)
        let free = (0..100).any(|_| {
            try_lock_work(&work).unwrap().is_some() || {
                std::thread::sleep(std::time::Duration::from_millis(10));
                false
            }
        });
        assert!(free, "and goes once the first is done");
    }

    fn cmake() -> PathBuf {
        PathBuf::from("/kode-sim/firmware/kode_sim.cmake")
    }

    fn include() -> String {
        format!("include(\"{}\")", cmake().display())
    }

    #[test]
    fn what_was_built_is_one_line_of_json() {
        let fw = BuildResult {
            merged_bin: "/w/out/merged-binary.bin".into(),
            elf: "/w/out/app.elf".into(),
            app_bin: Some("/w/out/app.bin".into()),
        };
        let j: serde_json::Value = serde_json::from_str(&paths_json(&fw)).unwrap();
        assert_eq!(j["merged_bin"], "/w/out/merged-binary.bin");
        assert_eq!(j["elf"], "/w/out/app.elf");
        assert_eq!(j["app_bin"], "/w/out/app.bin");
        assert!(!paths_json(&fw).contains('\n'));
        let j: serde_json::Value = serde_json::from_str(&paths_json(&BuildResult { app_bin: None, ..fw })).unwrap();
        assert!(j["app_bin"].is_null());
    }

    #[test]
    fn inject_before_project() {
        let t = Tmp::new("inject");
        let f = t.0.join("CMakeLists.txt");
        fs::write(&f, BUTTONS_LED).unwrap();
        let text = inject_with(&f, &cmake()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        let i = lines.iter().position(|l| *l == "project(buttons_led)").unwrap();
        assert_eq!(lines[i - 1], include());
        assert_eq!(lines[i - 2], "include($ENV{IDF_PATH}/tools/cmake/project.cmake)");
        assert!(text.ends_with("endif()\n"));
    }

    #[test]
    fn inject_ignores_indented_and_commented_project() {
        let t = Tmp::new("inject");
        let f = t.0.join("CMakeLists.txt");
        fs::write(&f, format!("# project(nope)\nif(X)\n  project(inner)\nendif()\n{BUTTONS_LED}")).unwrap();
        let text = inject_with(&f, &cmake()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        let i = lines.iter().position(|l| *l == "project(buttons_led)").unwrap();
        assert!(lines[i - 1].starts_with("include("));
        assert_eq!(lines.iter().filter(|l| **l == include()).count(), 1);
    }

    #[test]
    fn inject_no_project_says_which_file() {
        let t = Tmp::new("inject");
        let f = t.0.join("CMakeLists.txt");
        fs::write(&f, "cmake_minimum_required(VERSION 3.22)\n").unwrap();
        let e = inject_with(&f, &cmake()).unwrap_err();
        assert!(e.0.contains(&f.display().to_string()), "{e}");
        assert!(e.0.contains("project()"), "{e}");
    }

    #[test]
    fn the_injected_file_is_the_checkout_s() {
        let c = kode_sim_cmake();
        assert!(c.is_absolute() && c.ends_with("firmware/kode_sim.cmake") && c.exists(), "{}", c.display());
        assert!(!c.to_string_lossy().contains(".."), "{}", c.display());
    }

    #[test]
    fn override_keeps_every_kodediy_dep() {
        let text = "dependencies:
  kodediy/kode:
    git: git@github.com:kodediy/kode-sdk.git
    path: components/kode
    version: v00.00.27
  kodediy/kscreen:
    git: git@github.com:kodediy/kode-sdk.git
    path: components/kscreen
    version: v00.00.27
  espressif/esp_lvgl_port: \"^2.4\"
";
        let out = override_kodediy(text, Path::new("/sdk"));
        assert!(out.contains("  kodediy/kode:\n    override_path: /sdk/components/kode\n"), "{out}");
        assert!(out.contains("override_path: /sdk/components/kscreen"), "{out}");
        assert!(out.contains("  espressif/esp_lvgl_port: \"^2.4\"\n"), "{out}");
        assert!(!out.contains("git:") && !out.contains("version:"), "{out}");
        assert!(out.starts_with("dependencies:\n"));
    }

    #[test]
    fn the_work_dir_is_named_as_it_always_was() {
        // hashlib.sha1(b"/projects/kode-sdk/examples/buttons_led")
        let p = Path::new("/projects/kode-sdk/examples/buttons_led");
        let cache = Path::new("/c");
        assert_eq!(work_dir(cache, p, false), Path::new("/c/buttons_led-4eb7d4a0"));
        assert_eq!(work_dir(cache, p, true), Path::new("/c/buttons_led-4eb7d4a0-board"));
    }

    fn rsync_ok() -> bool {
        crate::emu::which("rsync")
    }

    #[test]
    fn prepare_drops_a_stale_sdkconfig() {
        if !rsync_ok() {
            return;
        }
        let t = Tmp::new("prepare");
        let cache = t.0.join("cache");
        let proj = t.0.join("app");
        fs::create_dir_all(proj.join("build")).unwrap();
        fs::write(proj.join("CMakeLists.txt"), BUTTONS_LED).unwrap();
        fs::write(proj.join("sdkconfig"), "CONFIG_FROM_THE_PROJECT=y\n").unwrap();
        fs::write(proj.join("build").join("x.o"), "").unwrap();
        let src = prepare(&cache, &proj, None, false).unwrap();
        assert_eq!(src, work_dir(&cache, &proj, false).join("src"));
        assert!(!src.join("sdkconfig").exists());
        assert!(!src.join("build").exists(), "build output is not copied");
        fs::write(src.join("sdkconfig"), "CONFIG_FROM_THE_LAST_BUILD=y\n").unwrap();
        fs::write(src.join("sdkconfig.old"), "x\n").unwrap();
        fs::write(proj.join("sdkconfig.defaults"), "CONFIG_A=y\n").unwrap();
        let src = prepare(&cache, &proj, None, false).unwrap();
        assert!(!src.join("sdkconfig").exists() && !src.join("sdkconfig.old").exists());
        assert!(fs::read_to_string(src.join("CMakeLists.txt")).unwrap().contains("kode_sim.cmake"));
        assert_eq!(fs::read_to_string(proj.join("CMakeLists.txt")).unwrap(), BUTTONS_LED, "the project is untouched");
    }

    #[test]
    fn only_the_project_s_own_build_output_and_config_are_left_behind() {
        if !rsync_ok() {
            return;
        }
        let t = Tmp::new("prepare");
        let proj = t.0.join("app");
        for d in [
            "build",
            "build_p4",
            "managed_components/x",
            "main",
            "components/x/build",
            "components/y/managed_components",
        ] {
            fs::create_dir_all(proj.join(d)).unwrap();
        }
        fs::write(proj.join("CMakeLists.txt"), BUTTONS_LED).unwrap();
        for f in [
            "build/x.o",
            "build_p4/x.o",
            "managed_components/x/y.c",
            "sdkconfig",
            "sdkconfig.old",
            "main/build_info.c",
            "main/sdkconfig",
            "main/build_x.h",
            "components/x/build/y.c",
            "components/x/sdkconfig.old",
            "components/y/managed_components/z.c",
        ] {
            fs::write(proj.join(f), "x").unwrap();
        }
        let src = prepare(&t.0.join("cache"), &proj, None, false).unwrap();
        for gone in ["build", "build_p4", "managed_components", "sdkconfig", "sdkconfig.old"] {
            assert!(!src.join(gone).exists(), "{gone} is copied");
        }
        for kept in [
            "main/build_info.c",
            "main/sdkconfig",
            "main/build_x.h",
            "components/x/build/y.c",
            "components/x/sdkconfig.old",
            "components/y/managed_components/z.c",
        ] {
            assert!(src.join(kept).exists(), "{kept} is not copied");
        }
    }

    fn mtime(p: &Path) -> std::time::SystemTime {
        fs::metadata(p).unwrap().modified().unwrap()
    }

    fn age(p: &Path) {
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
        fs::File::options().write(true).open(p).unwrap().set_modified(t).unwrap();
    }

    #[test]
    fn a_second_prepare_of_the_same_project_changes_nothing_the_build_watches() {
        if !rsync_ok() {
            return;
        }
        let t = Tmp::new("prepare");
        let cache = t.0.join("cache");
        let proj = t.0.join("app");
        fs::create_dir_all(proj.join("main")).unwrap();
        fs::write(proj.join("CMakeLists.txt"), BUTTONS_LED).unwrap();
        fs::write(proj.join("sdkconfig.defaults"), "CONFIG_A=managed_components/kodediy__kbsp/x\n").unwrap();
        fs::write(proj.join("main").join("idf_component.yml"), "dependencies:\n  kodediy/kbsp:\n    git: x\n").unwrap();
        let sdk = Some(Path::new("/sdk"));
        let src = prepare(&cache, &proj, sdk, false).unwrap();
        // What the build made there: its configuration and its solved dependencies.
        fs::write(src.join("sdkconfig"), "CONFIG_FROM_THE_LAST_BUILD=y\n").unwrap();
        fs::write(src.join("dependencies.lock"), "solved\n").unwrap();
        let watched = ["CMakeLists.txt", "sdkconfig.defaults", "main/idf_component.yml"];
        for f in watched {
            age(&src.join(f));
        }
        let before: Vec<_> = watched.iter().map(|f| mtime(&src.join(f))).collect();
        let src = prepare(&cache, &proj, sdk, false).unwrap();
        let after: Vec<_> = watched.iter().map(|f| mtime(&src.join(f))).collect();
        assert_eq!(before, after, "rewritten although nothing changed");
        assert_eq!(fs::read_to_string(src.join("sdkconfig")).unwrap(), "CONFIG_FROM_THE_LAST_BUILD=y\n");
        assert_eq!(fs::read_to_string(src.join("dependencies.lock")).unwrap(), "solved\n");
        assert!(fs::read_to_string(src.join("CMakeLists.txt")).unwrap().contains("kode_sim.cmake"));
        assert!(fs::read_to_string(src.join("main/idf_component.yml")).unwrap().contains("override_path: /sdk"));
    }

    #[test]
    fn a_new_configuration_is_solved_and_configured_again() {
        if !rsync_ok() {
            return;
        }
        // A component's rules may ask a Kconfig option: once the sdkconfig
        // is made again, so are the solved dependencies and CMake's cache.
        let t = Tmp::new("prepare");
        let cache = t.0.join("cache");
        let proj = t.0.join("app");
        fs::create_dir_all(proj.join("main")).unwrap();
        fs::write(proj.join("CMakeLists.txt"), BUTTONS_LED).unwrap();
        fs::write(proj.join("sdkconfig.defaults"), "CONFIG_KODE_TEST_DOT=n\n").unwrap();
        let src = prepare(&cache, &proj, None, false).unwrap();
        let out = work_dir(&cache, &proj, false).join("out");
        fs::create_dir_all(&out).unwrap();
        fs::write(src.join("dependencies.lock"), "solved without the camera\n").unwrap();
        fs::write(out.join("CMakeCache.txt"), "configured\n").unwrap();
        prepare(&cache, &proj, None, false).unwrap();
        assert!(src.join("dependencies.lock").exists() && out.join("CMakeCache.txt").exists(), "nothing changed");
        fs::write(proj.join("sdkconfig.defaults"), "CONFIG_KODE_TEST_DOT=y\n").unwrap();
        prepare(&cache, &proj, None, false).unwrap();
        assert!(!src.join("dependencies.lock").exists(), "solved for the old configuration");
        assert!(!out.join("CMakeCache.txt").exists(), "configured for the old configuration");
    }

    #[test]
    fn the_dependencies_are_solved_again_when_the_kode_sdk_s_components_change() {
        if !rsync_ok() {
            return;
        }
        let t = Tmp::new("prepare");
        let cache = t.0.join("cache");
        let proj = t.0.join("app");
        let sdk = t.0.join("kode-sdk");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("CMakeLists.txt"), BUTTONS_LED).unwrap();
        for c in ["kdrv", "rfal"] {
            fs::create_dir_all(sdk.join("components").join(c)).unwrap();
            fs::write(sdk.join("components").join(c).join("idf_component.yml"), "version: 0.1.0\n").unwrap();
        }
        let src = prepare(&cache, &proj, Some(&sdk), false).unwrap();
        fs::write(src.join("dependencies.lock"), "solved against rfal\n").unwrap();
        prepare(&cache, &proj, Some(&sdk), false).unwrap();
        assert!(src.join("dependencies.lock").exists(), "the same kode-sdk: kept");
        // A kode-sdk pulled in the same place: rfal gone, kdrv a version on.
        fs::remove_dir_all(sdk.join("components").join("rfal")).unwrap();
        fs::write(sdk.join("components").join("kdrv").join("idf_component.yml"), "version: 0.1.1\n").unwrap();
        prepare(&cache, &proj, Some(&sdk), false).unwrap();
        assert!(!src.join("dependencies.lock").exists(), "solved against a kode-sdk that is not there any more");
    }

    #[test]
    fn the_configuration_is_made_again_when_what_it_is_made_from_changes() {
        if !rsync_ok() {
            return;
        }
        let t = Tmp::new("prepare");
        let cache = t.0.join("cache");
        let proj = t.0.join("app");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("CMakeLists.txt"), BUTTONS_LED).unwrap();
        fs::write(proj.join("sdkconfig.defaults"), "CONFIG_A=y\n").unwrap();
        let again = |sdk: Option<&Path>| {
            let src = prepare(&cache, &proj, sdk, false).unwrap();
            let had = src.join("sdkconfig").exists();
            fs::write(src.join("sdkconfig"), "CONFIG_FROM_THE_LAST_BUILD=y\n").unwrap();
            had
        };
        again(None);
        assert!(again(None), "kept: nothing changed");
        fs::write(proj.join("sdkconfig.defaults"), "CONFIG_A=n\n").unwrap();
        assert!(!again(None), "sdkconfig.defaults changed");
        fs::write(proj.join("sdkconfig.defaults.esp32p4"), "CONFIG_B=y\n").unwrap();
        assert!(!again(None), "a new sdkconfig.defaults.*");
        assert!(again(None));
        assert!(!again(Some(Path::new("/sdk"))), "another kode-sdk");
        assert!(again(Some(Path::new("/sdk"))));
        fs::remove_file(proj.join("sdkconfig.defaults.esp32p4")).unwrap();
        assert!(!again(Some(Path::new("/sdk"))), "one gone");
        assert!(
            !work_dir(&cache, &proj, false).join("src/sdkconfig.defaults.esp32p4").exists(),
            "gone from the copy too"
        );
    }

    #[test]
    fn the_stamp_tells_the_board_s_and_the_simulator_s_configurations_apart() {
        let t = Tmp::new("stamp");
        fs::write(t.0.join("sdkconfig.defaults"), "CONFIG_A=y\n").unwrap();
        let emu = config_stamp(&t.0, None, false);
        assert_ne!(emu, config_stamp(&t.0, None, true));
        assert_ne!(emu, config_stamp(&t.0, Some(Path::new("/sdk")), false));
        assert_eq!(emu, config_stamp(&t.0, None, false));
    }

    #[test]
    fn a_project_s_own_dependencies_lock_is_used_and_the_build_s_kept() {
        if !rsync_ok() {
            return;
        }
        let t = Tmp::new("prepare");
        let cache = t.0.join("cache");
        let proj = t.0.join("app");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("CMakeLists.txt"), BUTTONS_LED).unwrap();
        let src = prepare(&cache, &proj, None, false).unwrap();
        assert!(!src.join("dependencies.lock").exists());
        fs::write(src.join("dependencies.lock"), "the build's\n").unwrap();
        prepare(&cache, &proj, None, false).unwrap();
        assert_eq!(fs::read_to_string(src.join("dependencies.lock")).unwrap(), "the build's\n");
        fs::write(proj.join("dependencies.lock"), "the project's\n").unwrap();
        prepare(&cache, &proj, None, false).unwrap();
        assert_eq!(fs::read_to_string(src.join("dependencies.lock")).unwrap(), "the project's\n");
        fs::write(src.join("dependencies.lock"), "solved again by the build\n").unwrap();
        prepare(&cache, &proj, None, false).unwrap();
        assert_eq!(
            fs::read_to_string(src.join("dependencies.lock")).unwrap(),
            "solved again by the build\n",
            "the project's goes in when it changes, not every time"
        );
    }

    #[test]
    fn prepare_for_the_board_includes_nothing() {
        if !rsync_ok() {
            return;
        }
        let t = Tmp::new("prepare");
        let proj = t.0.join("app");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("CMakeLists.txt"), BUTTONS_LED).unwrap();
        let src = prepare(&t.0.join("cache"), &proj, None, true).unwrap();
        assert!(src.starts_with(work_dir(&t.0.join("cache"), &proj, true)));
        assert_eq!(fs::read_to_string(src.join("CMakeLists.txt")).unwrap(), BUTTONS_LED);
    }

    #[test]
    fn prepare_of_what_is_not_a_project_says_so() {
        let t = Tmp::new("prepare");
        let e = prepare(&t.0.join("cache"), &t.0, None, false).unwrap_err();
        assert!(e.0.contains("not found - is") && e.0.contains("an ESP-IDF project?"), "{e}");
    }

    #[test]
    fn prepare_points_a_pinned_example_at_a_local_sdk() {
        if !rsync_ok() {
            return;
        }
        let t = Tmp::new("prepare");
        let proj = t.0.join("app");
        fs::create_dir_all(proj.join("main")).unwrap();
        fs::write(
            proj.join("CMakeLists.txt"),
            format!("set(X ${{CMAKE_CURRENT_LIST_DIR}}/managed_components/kodediy__kbsp/kbsp.cmake)\n{BUTTONS_LED}"),
        )
        .unwrap();
        fs::write(proj.join("sdkconfig.defaults"), "CONFIG_A=managed_components/kodediy__kbsp/x\n").unwrap();
        fs::write(proj.join("main").join("idf_component.yml"), "dependencies:\n  kodediy/kbsp:\n    git: x\n").unwrap();
        let src = prepare(&t.0.join("cache"), &proj, Some(Path::new("/sdk")), false).unwrap();
        let top = fs::read_to_string(src.join("CMakeLists.txt")).unwrap();
        assert!(top.starts_with("set(X /sdk/components/kbsp/kbsp.cmake)\n"), "{top}");
        assert!(top.contains("kode_sim.cmake"));
        let defaults = fs::read_to_string(src.join("sdkconfig.defaults")).unwrap();
        assert_eq!(defaults, "CONFIG_A=/sdk/components/kbsp/x\n");
        let manifest = fs::read_to_string(src.join("main").join("idf_component.yml")).unwrap();
        assert_eq!(manifest, "dependencies:\n  kodediy/kbsp:\n    override_path: /sdk/components/kbsp\n");
    }

    // ---- bootable -----------------------------------------------------------

    /// (name, type, subtype, offset, size) as ESP-IDF writes them.
    pub(crate) fn partition_table(entries: &[(&str, u8, u8, u32, u32)]) -> Vec<u8> {
        let mut out = vec![];
        for (name, t, st, off, size) in entries {
            out.extend_from_slice(&[0xaa, 0x50, *t, *st]);
            out.extend_from_slice(&off.to_le_bytes());
            out.extend_from_slice(&size.to_le_bytes());
            let mut n = [0u8; 16];
            n[..name.len()].copy_from_slice(name.as_bytes());
            out.extend_from_slice(&n);
            out.extend_from_slice(&0u32.to_le_bytes());
        }
        out.resize(3072, 0xff);
        out
    }

    pub(crate) fn kode_sdk_table() -> Vec<u8> {
        partition_table(&[
            ("nvs", 1, 0x02, 0xB000, 0x10000),
            ("factory", 0, 0x00, 0x20000, 0x400000),
            ("app_pending", 1, 0x41, 0x1A00000, 0x600000),
        ])
    }

    fn out_dir(t: &Tmp, flash_files: &str) -> PathBuf {
        let out = t.0.join("out");
        for d in ["config", "bootloader", "partition_table"] {
            fs::create_dir_all(out.join(d)).unwrap();
        }
        fs::write(
            out.join("config").join("sdkconfig.json"),
            r#"{"BOOTLOADER_OFFSET_IN_FLASH": 8192, "PARTITION_TABLE_OFFSET": 32768}"#,
        )
        .unwrap();
        let mut boot = b"\xe9BOOT".to_vec();
        boot.resize(105, 0);
        fs::write(out.join("bootloader").join("bootloader.bin"), boot).unwrap();
        fs::write(out.join("partition_table").join("partition-table.bin"), kode_sdk_table()).unwrap();
        let mut app = b"\xe9APP!".to_vec();
        app.resize(1005, 0);
        fs::write(out.join("app.bin"), &app).unwrap();
        fs::write(out.join("flasher_args.json"), format!("{{\"flash_files\": {flash_files}}}")).unwrap();
        let mut merged = vec![0xff; 0x1A00000];
        merged.extend_from_slice(&app);
        fs::write(out.join("merged-binary.bin"), merged).unwrap();
        out
    }

    #[test]
    fn the_partition_table_is_read_to_its_end() {
        let p = partitions(&kode_sdk_table());
        assert_eq!(p.len(), 3);
        assert_eq!(
            p[2],
            Partition { ptype: 1, subtype: 0x41, offset: 0x1A00000, size: 0x600000, name: "app_pending".into() }
        );
        assert!(partitions(&[0xff; 64]).is_empty());
    }

    #[test]
    fn an_app_built_for_the_mailbox_boots_from_the_factory_slot() {
        let t = Tmp::new("bootable");
        let out = out_dir(&t, r#"{"0x1a00000": "app.bin"}"#);
        let image = fs::read(bootable(&out, &out.join("app.bin")).unwrap()).unwrap();
        assert_eq!(&image[0x2000..0x2005], b"\xe9BOOT");
        assert_eq!(&image[0x8000..0x8002], b"\xaa\x50");
        assert_eq!(&image[0x20000..0x20005], b"\xe9APP!");
        assert!(image.len() < 0x1A00000, "not left in the mailbox too");
        assert!(image[0x2000 + 105..0x8000].iter().all(|&b| b == 0xff));
    }

    #[test]
    fn a_project_with_its_own_bootloader_keeps_its_image() {
        let t = Tmp::new("bootable");
        let out = out_dir(
            &t,
            r#"{"0x2000": "bootloader/bootloader.bin", "0x8000": "partition_table/partition-table.bin", "0x20000": "app.bin"}"#,
        );
        let before = fs::read(out.join("merged-binary.bin")).unwrap();
        assert_eq!(bootable(&out, &out.join("app.bin")).unwrap(), out.join("merged-binary.bin"));
        assert_eq!(fs::read(out.join("merged-binary.bin")).unwrap(), before);
    }

    #[test]
    fn an_app_bigger_than_the_factory_slot_says_so() {
        let t = Tmp::new("bootable");
        let out = out_dir(&t, r#"{"0x1a00000": "app.bin"}"#);
        fs::write(
            out.join("partition_table").join("partition-table.bin"),
            partition_table(&[("factory", 0, 0x00, 0x20000, 0x200), ("app_pending", 1, 0x41, 0x1A00000, 0x600000)]),
        )
        .unwrap();
        let e = bootable(&out, &out.join("app.bin")).unwrap_err();
        assert!(e.0.contains("app.bin is 1005 bytes") && e.0.contains("factory") && e.0.contains("512 bytes"), "{e}");
        assert!(e.0.contains("kodeOS") && e.0.contains("kode-sim install"), "{e}");
    }

    #[test]
    fn esp_idf_s_environment_is_its_activate_script_or_idf_path_s_export_sh() {
        let t = Tmp::new("idf");
        let home = t.0.join("home");
        let idf = t.0.join("esp-idf");
        fs::create_dir_all(&idf).unwrap();
        let e = idf_env_in(&home, None).unwrap_err();
        assert!(e.0.contains("activate_idf_v6.1.sh") && e.0.contains("IDF_PATH"), "{e}");
        assert!(e.0.contains("ESP-IDF v6.1"), "{e}");
        let e = idf_env_in(&home, Some(idf.as_os_str())).unwrap_err();
        assert!(e.0.contains(&idf.join("export.sh").display().to_string()), "{e}");
        fs::write(idf.join("export.sh"), "").unwrap();
        assert_eq!(idf_env_in(&home, Some(idf.as_os_str())).unwrap(), idf.join("export.sh"));
        let activate = home.join(".espressif/tools/activate_idf_v6.1.sh");
        fs::create_dir_all(activate.parent().unwrap()).unwrap();
        fs::write(&activate, "").unwrap();
        assert_eq!(idf_env_in(&home, Some(idf.as_os_str())).unwrap(), activate, "the activate script first");
    }

    #[test]
    fn an_environment_that_does_not_activate_says_why_in_the_log() {
        let t = Tmp::new("idf");
        let script = t.0.join("activate.sh");
        fs::write(&script, "echo \"python3 not found\" >&2\nreturn 1\n").unwrap();
        let log = t.0.join("build.log");
        assert!(!idf(&script, &["--version".into()], &log, false).unwrap());
        let text = fs::read_to_string(&log).unwrap();
        assert!(text.contains("python3 not found"), "{text}");
        assert!(text.contains(&script.display().to_string()), "{text}");
    }

    #[test]
    fn an_environment_that_exits_0_but_brings_no_idf_py_says_why_in_the_log() {
        // ESP-IDF's export.sh can fail and still return 0 (a missing
        // constraints file, say): what it printed must reach the log, not
        // only "idf.py: command not found".
        let t = Tmp::new("idf");
        let script = t.0.join("export.sh");
        fs::write(&script, "echo \"constraints file doesn't exist\" >&2\nPATH=/usr/bin:/bin\n").unwrap();
        let log = t.0.join("build.log");
        assert!(!idf(&script, &["--version".into()], &log, false).unwrap());
        let text = fs::read_to_string(&log).unwrap();
        assert!(text.contains("constraints file doesn't exist"), "{text}");
        assert!(text.contains("no idf.py on PATH"), "{text}");
    }

    #[test]
    fn a_table_with_no_app_slot_cannot_boot() {
        let t = Tmp::new("bootable");
        let out = out_dir(&t, r#"{"0x1a00000": "app.bin"}"#);
        fs::write(
            out.join("partition_table").join("partition-table.bin"),
            partition_table(&[("nvs", 1, 2, 0xB000, 0x10000)]),
        )
        .unwrap();
        let e = bootable(&out, &out.join("app.bin")).unwrap_err();
        assert!(e.0.contains("no app slot to boot app.bin from"), "{e}");
    }
}
