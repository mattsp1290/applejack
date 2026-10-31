//! Spawning the loader: the environment rows the engine sets, log capture,
//! the `Session` methods, and the 32-bit refusal.

use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::launch::env::{self, AllowedRoots, Dropped};
use crate::{
    EnvPair, Error, GraphicsBackend, InstalledEngine, LaunchSpec, Prefix, Result, Session, SyncMode,
};

use super::cpuid::CpuidProbe;
use super::process::{self, Process, SESSION_VARIABLE};

/// `WINEDLLOVERRIDES` entries every Wine process of this engine carries:
/// the Wine Mono (`mscoree`) and Gecko (`mshtml`) installers disabled.
/// `wineboot --init` otherwise blocks on the install-Mono dialog, and
/// applejack never ships either package.
pub const MONO_GECKO_OVERRIDE: &str = "mscoree=;mshtml=";

/// System directories appended to the bundle's `wine/bin` on the
/// children's `PATH`. `/usr/local` and `/opt/homebrew` are not on it, so
/// `wineboot` does not find a host `update-mime-database` and the payload
/// cannot pick up host tools.
pub const HOST_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

/// The Rosetta variable the CPUID probe records.
pub const ADVERTISE_AVX: &str = "ROSETTA_ADVERTISE_AVX";

/// `MoltenVK_icd.json` relative to the bundle root, used for
/// `VK_ICD_FILENAMES` with the vkd3d-proton backend when present.
pub const VULKAN_ICD: &str = "vulkan/MoltenVK_icd.json";

/// How long `<server> -w` may take after `<server> -k` before the wait is
/// abandoned: `-k` escalates to `SIGKILL` on its own after about ten
/// seconds, so a `-w` still blocked after this is waiting on a server that
/// was started again in the meantime, not on the one that was killed.
pub const KILL_WAIT: Duration = Duration::from_secs(30);

/// How long [`Session::cancel`] waits for the loader to exit after the
/// server is gone before it kills the loader itself.
pub const LOADER_EXIT_WAIT: Duration = Duration::from_secs(5);

/// The `PATH` the children see: the loader's directory, then [`HOST_PATH`].
#[must_use]
pub fn child_path(installed: &InstalledEngine) -> OsString {
    let loader = installed.root.join(&installed.bundle.wine.loader);
    let mut path = loader
        .parent()
        .map_or_else(|| installed.root.as_os_str(), Path::as_os_str)
        .to_owned();
    path.push(":");
    path.push(HOST_PATH);
    path
}

/// `WINEDLLOVERRIDES` for `backend`: [`MONO_GECKO_OVERRIDE`] and then the
/// backend's DLLs as `a,b,c=n,b`.
#[must_use]
pub fn dll_overrides_value(bundle: &crate::BundleToml, backend: GraphicsBackend) -> String {
    let stems = super::prefix::override_stems(bundle, backend);
    if stems.is_empty() {
        MONO_GECKO_OVERRIDE.to_owned()
    } else {
        format!("{MONO_GECKO_OVERRIDE};{}=n,b", stems.join(","))
    }
}

/// The ICD file for `VK_ICD_FILENAMES`, when the bundle carries one.
#[must_use]
pub fn vulkan_icd(installed: &InstalledEngine) -> Option<PathBuf> {
    let icd = installed.root.join(VULKAN_ICD);
    icd.is_file().then_some(icd)
}

/// The engine tier of the environment: `PATH`, `WINEPREFIX`,
/// `APPLEJACK_SESSION`, `WINEDLLOVERRIDES`, the sync variable,
/// `VK_ICD_FILENAMES` for vkd3d-proton, and `ROSETTA_ADVERTISE_AVX=1` when
/// `advertise_avx`.
#[must_use]
pub fn engine_rows(
    installed: &InstalledEngine,
    prefix_dir: &Path,
    session_id: &str,
    backend: GraphicsBackend,
    sync_mode: SyncMode,
    advertise_avx: bool,
) -> Vec<EnvPair> {
    let pair = |name: &str, value: OsString| (OsString::from(name), value);
    let mut rows = vec![
        pair("PATH", child_path(installed)),
        pair("WINEPREFIX", prefix_dir.as_os_str().to_owned()),
        pair(SESSION_VARIABLE, OsString::from(session_id)),
        pair(
            "WINEDLLOVERRIDES",
            OsString::from(dll_overrides_value(&installed.bundle, backend)),
        ),
    ];
    match sync_mode {
        SyncMode::Msync => rows.push(pair("WINEMSYNC", OsString::from("1"))),
        SyncMode::Esync => rows.push(pair("WINEESYNC", OsString::from("1"))),
        SyncMode::None => {}
    }
    if backend == GraphicsBackend::Vkd3dProton
        && let Some(icd) = vulkan_icd(installed)
    {
        rows.push(pair("VK_ICD_FILENAMES", icd.into_os_string()));
    }
    if advertise_avx {
        rows.push(pair(ADVERTISE_AVX, OsString::from("1")));
    }
    rows
}

/// Whether a launch sets `ROSETTA_ADVERTISE_AVX=1`: the spec asks and the
/// recorded probe says the variable changes the bits.
#[must_use]
pub fn advertise_avx(spec: &LaunchSpec) -> bool {
    spec.advertise_avx
        && spec
            .cpuid_probe
            .as_ref()
            .is_some_and(CpuidProbe::changes_bits)
}

/// Runs the host environment, `extras`, and `engine_rows` through the
/// allowlist with the roots of `installed` and `prefix_dir`.
pub(crate) fn assemble_env(
    installed: &InstalledEngine,
    prefix_dir: &Path,
    extras: &[EnvPair],
    engine_rows: &[EnvPair],
) -> Result<(Vec<EnvPair>, Vec<Dropped>)> {
    let roots = AllowedRoots::new(&installed.root, &installed.overlay_root, prefix_dir)?;
    let base: Vec<EnvPair> = std::env::vars_os().collect();
    env::assemble(&base, extras, engine_rows, &roots)
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// `<kind>-<unix-nanos hex>-<pid hex>-<counter>`: unique on the host, so
/// process discovery never matches a stale environment.
pub(crate) fn new_session_id(kind: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    format!(
        "{kind}-{nanos:x}-{:x}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

fn open_log(log: &Path) -> Result<File> {
    if let Some(parent) = log.parent() {
        fs::create_dir_all(parent).map_err(|error| Error::io(parent, error))?;
    }
    File::options()
        .create(true)
        .append(true)
        .open(log)
        .map_err(|error| Error::io(log, error))
}

fn render(program: &Path, args: &[&OsStr]) -> String {
    let mut text = program.display().to_string();
    for arg in args {
        text.push(' ');
        text.push_str(&arg.to_string_lossy());
    }
    text
}

fn spawn_logged(
    program: &Path,
    args: &[&OsStr],
    env: &[EnvPair],
    cwd: &Path,
    log: &Path,
    header: &[String],
) -> Result<std::process::Child> {
    let mut file = open_log(log)?;
    let mut lines = vec![format!("[applejack] run: {}", render(program, args))];
    lines.extend(header.iter().cloned());
    for line in lines {
        writeln!(file, "{line}").map_err(|error| Error::io(log, error))?;
    }
    let stderr = file.try_clone().map_err(|error| Error::io(log, error))?;
    Command::new(program)
        .args(args)
        .env_clear()
        .envs(env.iter().map(|(name, value)| (name, value)))
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(file))
        .stderr(Stdio::from(stderr))
        .spawn()
        .map_err(|error| Error::Spawn {
            program: program.to_path_buf(),
            source: error,
        })
}

/// Waits for `child` up to `timeout`. `Ok(None)` means the deadline passed
/// and the child was killed and reaped.
pub(crate) fn wait_bounded(
    child: &mut std::process::Child,
    program: &Path,
    timeout: Duration,
) -> Result<Option<ExitStatus>> {
    let started = Instant::now();
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| Error::io(program, error))?
        {
            return Ok(Some(status));
        }
        if started.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The error for a process [`wait_bounded`] killed; the pid tells a user
/// what to look for in `ps` when the kill was not enough.
fn timed_out(command: String, timeout: Duration, log: &Path, pid: u32) -> Error {
    Error::WineCommandFailed {
        command,
        status: format!("timed out after {}s (pid {pid} killed)", timeout.as_secs()),
        log: log.to_path_buf(),
    }
}

/// Runs `<loader> args` in `cwd` to completion with `env`, stdout and
/// stderr appended to `log`, bounded by `timeout`. On expiry the loader
/// process is killed and reaped and the step is reported as failed with the
/// log path, so a Wine dialog cannot block the caller for longer than
/// `timeout`; stopping the prefix's server is the caller's step, so that a
/// failed prefix operation kills it once.
pub(crate) fn run_loader(
    installed: &InstalledEngine,
    args: &[&str],
    env: &[EnvPair],
    cwd: &Path,
    log: &Path,
    timeout: Duration,
) -> Result<()> {
    let loader = installed.root.join(&installed.bundle.wine.loader);
    let args: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
    let mut child = spawn_logged(&loader, &args, env, cwd, log, &[])?;
    let pid = child.id();
    let Some(status) = wait_bounded(&mut child, &loader, timeout)? else {
        return Err(timed_out(render(&loader, &args), timeout, log, pid));
    };
    if status.success() {
        Ok(())
    } else {
        Err(Error::WineCommandFailed {
            command: render(&loader, &args),
            status: status.to_string(),
            log: log.to_path_buf(),
        })
    }
}

/// `<server> -w` for `prefix_dir`, bounded by `timeout`: on expiry the wait
/// is reported as failed; the caller stops the server.
pub(crate) fn wait_for_server(
    installed: &InstalledEngine,
    prefix_dir: &Path,
    log: &Path,
    timeout: Duration,
) -> Result<()> {
    let server = installed.root.join(&installed.bundle.wine.server);
    let path = child_path(installed);
    let mut child = server_command(&server, &path, prefix_dir, "-w", log)?;
    let pid = child.id();
    let Some(status) = wait_bounded(&mut child, &server, timeout)? else {
        return Err(timed_out(
            format!("{} -w", server.display()),
            timeout,
            log,
            pid,
        ));
    };
    if status.success() {
        Ok(())
    } else {
        Err(Error::WineCommandFailed {
            command: format!("{} -w", server.display()),
            status: status.to_string(),
            log: log.to_path_buf(),
        })
    }
}

fn server_command(
    server: &Path,
    path: &OsStr,
    prefix_dir: &Path,
    flag: &str,
    log: &Path,
) -> Result<std::process::Child> {
    let env = [
        (OsString::from("PATH"), path.to_owned()),
        (
            OsString::from("WINEPREFIX"),
            prefix_dir.as_os_str().to_owned(),
        ),
    ];
    spawn_logged(server, &[OsStr::new(flag)], &env, prefix_dir, log, &[])
}

/// `<server> -k` then `<server> -w` bounded by [`KILL_WAIT`], so the caller
/// returns once the server and every process of the prefix are gone, or
/// with an error naming the wait when a server is still there after the
/// bound.
fn kill_server(server: &Path, path: &OsStr, prefix_dir: &Path, log: &Path) -> Result<()> {
    // `-k` exits non-zero when no server runs, which is the state wanted.
    let _ = server_command(server, path, prefix_dir, "-k", log)?.wait();
    let mut child = server_command(server, path, prefix_dir, "-w", log)?;
    let pid = child.id();
    match wait_bounded(&mut child, server, KILL_WAIT)? {
        Some(_) => Ok(()),
        None => Err(timed_out(
            format!("{} -w", server.display()),
            KILL_WAIT,
            log,
            pid,
        )),
    }
}

/// [`kill_server`] for the bundle's server and the children's `PATH`.
pub(crate) fn kill_prefix_server(
    installed: &InstalledEngine,
    prefix_dir: &Path,
    log: &Path,
) -> Result<()> {
    let server = installed.root.join(&installed.bundle.wine.server);
    kill_server(&server, &child_path(installed), prefix_dir, log)
}

/// Spawns the loader for `spec` inside `prefix`.
pub(crate) fn spawn(
    installed: &InstalledEngine,
    prefix: &Prefix,
    spec: &LaunchSpec,
) -> Result<Session> {
    if prefix.engine != installed.id {
        return Err(Error::InvalidPrefix {
            path: prefix.dir.clone(),
            reason: format!(
                "it belongs to engine {}, not {}",
                prefix.engine, installed.id
            ),
        });
    }
    check_32bit(installed, &prefix.dir, &spec.cwd, &spec.exe)?;
    check_dxmt_copies(&prefix.dir, prefix.backend)?;
    let id = new_session_id("launch");
    let log_path = spec.log_dir.join(&id).join("wine.log");
    let rows = engine_rows(
        installed,
        &prefix.dir,
        &id,
        prefix.backend,
        prefix.sync_mode,
        advertise_avx(spec),
    );
    let (env, dropped) = assemble_env(installed, &prefix.dir, &spec.env_extras, &rows)?;
    // Host variables outside the allowlist are the normal case and would
    // put every name in the user's shell into the log; game extras and
    // rejected values are worth a line.
    let header: Vec<String> = dropped
        .iter()
        .filter(|drop| {
            drop.from != env::Owner::Host || drop.reason != env::DropReason::NotAllowlisted
        })
        .map(|drop| {
            format!(
                "[applejack] dropped {} from {:?} tier: {}",
                drop.name.to_string_lossy(),
                drop.from,
                drop.reason
            )
        })
        .collect();
    let loader = installed.root.join(&installed.bundle.wine.loader);
    let mut args: Vec<&OsStr> = vec![spec.exe.as_os_str()];
    args.extend(spec.args.iter().map(OsString::as_os_str));
    let child = spawn_logged(&loader, &args, &env, &spec.cwd, &log_path, &header)?;
    let path = env
        .iter()
        .find(|(name, _)| name == "PATH")
        .map_or_else(|| child_path(installed), |(_, value)| value.clone());
    Ok(Session {
        id,
        pid: child.id(),
        server: installed.root.join(&installed.bundle.wine.server),
        prefix_dir: prefix.dir.clone(),
        log_path,
        path,
        child,
    })
}

impl Drop for Session {
    /// Reaps the loader when it has already exited. A session dropped while
    /// its loader still runs is not waited for; the loader is reaped by the
    /// kernel when the launcher exits, so callers keep a `Session` until
    /// [`Session::wait`] or [`Session::cancel`] has returned.
    fn drop(&mut self) {
        let _ = self.child.try_wait();
    }
}

impl Session {
    /// Waits for the loader process to exit.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] from `waitpid`.
    pub fn wait(&mut self) -> Result<ExitStatus> {
        self.child
            .wait()
            .map_err(|error| Error::io(&self.log_path, error))
    }

    /// The loader's exit status if it has exited.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] from `waitpid`.
    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
        self.child
            .try_wait()
            .map_err(|error| Error::io(&self.log_path, error))
    }

    /// Kills everything in the prefix with `<server> -k`, waits for the
    /// server to be gone (bounded by [`KILL_WAIT`]), and reaps the loader:
    /// it normally exits on its own once the server is gone, and is killed
    /// after [`LOADER_EXIT_WAIT`] otherwise. The loader is reaped even when
    /// the server wait fails, so no process of the call outlives it.
    ///
    /// # Errors
    ///
    /// Returns the spawn or wait failure, or [`Error::WineCommandFailed`]
    /// when a server is still running after the bound.
    pub fn cancel(&mut self) -> Result<()> {
        let server = kill_server(&self.server, &self.path, &self.prefix_dir, &self.log_path);
        wait_bounded(&mut self.child, &self.log_path, LOADER_EXIT_WAIT)?;
        server
    }

    /// Same-uid processes carrying this session's id, by pid.
    #[must_use]
    pub fn processes(&self) -> Vec<Process> {
        process::find_session(&self.id)
    }

    /// Whether a `wineserver` is listening for this session's prefix.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when the prefix directory cannot be stat'ed.
    pub fn wineserver_alive(&self) -> Result<bool> {
        process::wineserver_alive(&self.prefix_dir)
    }
}

/// The machine type in a PE header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeMachine {
    /// `IMAGE_FILE_MACHINE_I386`.
    I386,
    /// `IMAGE_FILE_MACHINE_AMD64`.
    Amd64,
    /// Anything else, verbatim.
    Other(u16),
}

/// Reads the PE machine type of `path`; `None` when the file is not a PE
/// image.
///
/// # Errors
///
/// Returns the read failure.
pub fn pe_machine(path: &Path) -> io::Result<Option<PeMachine>> {
    let mut file = File::open(path)?;
    let mut dos = [0u8; 64];
    // `read_exact`: a short read on a slow volume must not turn an i386 PE
    // into "not a PE" and skip the refusal.
    if file.read_exact(&mut dos).is_err() || &dos[..2] != b"MZ" {
        return Ok(None);
    }
    let offset = u32::from_le_bytes([dos[60], dos[61], dos[62], dos[63]]);
    file.seek(SeekFrom::Start(u64::from(offset)))?;
    let mut header = [0u8; 6];
    if file.read_exact(&mut header).is_err() || &header[..4] != b"PE\0\0" {
        return Ok(None);
    }
    Ok(Some(match u16::from_le_bytes([header[4], header[5]]) {
        0x014c => PeMachine::I386,
        0x8664 => PeMachine::Amd64,
        other => PeMachine::Other(other),
    }))
}

/// Where `exe` is on disk: an absolute Unix path as is, `X:\...` or
/// `X:/...` through `<prefix>/dosdevices/x:`, anything else relative to
/// `cwd`.
#[must_use]
pub fn resolve_exe(prefix_dir: &Path, cwd: &Path, exe: &Path) -> PathBuf {
    if exe.is_absolute() {
        return exe.to_path_buf();
    }
    let text = exe.to_string_lossy();
    let bytes = text.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
    {
        let drive = format!("{}:", bytes[0].to_ascii_lowercase() as char);
        let rest = text[3..].replace('\\', "/");
        return prefix_dir.join("dosdevices").join(drive).join(rest);
    }
    cwd.join(text.replace('\\', "/"))
}

/// `builtin_signature` in Wine's `dlls/ntdll/loader.c`, right after the 64-byte
/// DOS header.
const BUILTIN_MARKER: &[u8] = b"Wine builtin DLL\0";
const BUILTIN_MARKER_OFFSET: usize = 0x40;

/// Refuses a `dxmt` prefix made with an engine before 11.0-2, whose `d3d11.dll`
/// still carries Wine's builtin marker. Wine takes such a copy for a
/// placeholder and quietly loads its own, so the title would otherwise fail in
/// `D3D11CreateDeviceAndSwapChain` with nothing naming the cause. A file that
/// cannot be read passes: the launch reports its own error.
///
/// # Errors
///
/// Returns [`Error::PrefixUnusable`].
pub fn check_dxmt_copies(prefix_dir: &Path, backend: GraphicsBackend) -> Result<()> {
    if backend != GraphicsBackend::Dxmt {
        return Ok(());
    }
    let d3d11 = prefix_dir.join("drive_c/windows/system32/d3d11.dll");
    let mut head = [0; BUILTIN_MARKER_OFFSET + BUILTIN_MARKER.len()];
    let marked = File::open(d3d11)
        .and_then(|mut file| file.read_exact(&mut head))
        .is_ok()
        && head.ends_with(BUILTIN_MARKER);
    if marked {
        return Err(Error::PrefixUnusable {
            path: prefix_dir.to_path_buf(),
            reason: "its DXMT DLLs came from an engine before 11.0-2 and Wine would ignore \
                     them; activate a newer engine and run `applejack prefix reset`",
        });
    }
    Ok(())
}

/// Refuses a 32-bit executable when the bundle runs 64-bit code only. A
/// file that cannot be read or is not a PE image passes: Wine reports its
/// own error.
///
/// # Errors
///
/// Returns [`Error::ThirtyTwoBitUnsupported`].
pub fn check_32bit(
    installed: &InstalledEngine,
    prefix_dir: &Path,
    cwd: &Path,
    exe: &Path,
) -> Result<()> {
    if installed.bundle.wine.wow64.supports_32bit() {
        return Ok(());
    }
    let path = resolve_exe(prefix_dir, cwd, exe);
    if pe_machine(&path).ok().flatten() == Some(PeMachine::I386) {
        return Err(Error::ThirtyTwoBitUnsupported {
            exe: exe.to_path_buf(),
            version: installed.version.clone(),
        });
    }
    Ok(())
}
