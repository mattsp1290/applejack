//! `cargo xtask engine-payload`: fetches the pinned inputs of the Rosetta
//! engine payload, checks an assembled payload tree, closes its dylib
//! references (`closure.rs`), and writes `BUNDLE.toml`. The Wine build itself
//! runs through the scripts under `build/`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use sha2::{Digest, Sha256};

use crate::bundle_toml::{BundleSpec, SHIPPED_ELSEWHERE, elsewhere_matches};
use crate::closure;
use crate::denylist::{Denylist, Hit};
use crate::pins::{Pin, Pins};
use crate::self_contained::{
    Search, Violation, binary_dir, check_otool_listing, check_references_resolve, check_rpaths,
    has_system_rpath, id_entry, is_executable, normalize_relative, references_left_to_dyld,
    relocated_rpaths,
};

pub const USAGE: &str = "\
usage: cargo xtask engine-payload <mode>
  --fetch [id ...]                    download every pin (or the named ones) into build/work/, verify SHA-256, and denylist-scan the listing
  --check <payload-dir>               denylist scan plus self-containment of every Mach-O and symlink; references resolve along the load chains of the executables <payload-dir>/BUNDLE.toml declares and of every executable under wine/
  --close <payload-dir>               the dylib closure: copy the x86-64 Homebrew libraries Wine links or dlopens into wine/lib/, rewrite install names and references to @rpath, add the run paths, re-sign, repeat until stable, then --check
  --check-listing <file|->            denylist scan of an archive listing, one path per line (an empty listing fails)
  --write-bundle-toml <spec> <out>    render BUNDLE.toml from a typed spec file, plus THIRD_PARTY_NOTICES.md beside it
  --print-pin <id> [field]            print a pin (all fields as key=value, or one field)
  --build                             run the build scripts under build/ in order (assemble.sh ends with --close), then --check the payload";

const PINS_FILE: &str = "build/pins.toml";
const DENYLIST_FILE: &str = "build/denylist.toml";
const WORK_DIR: &str = "build/work";

// The component fetches precede the Wine build, which needs the GStreamer
// devel tree and stage from `build/gstreamer/fetch.sh`.
const BUILD_SCRIPTS: &[&str] = &[
    "build/wine/fetch.sh",
    "build/dxmt/fetch.sh",
    "build/vkd3d/fetch.sh",
    "build/vulkan/fetch.sh",
    "build/gstreamer/fetch.sh",
    "build/wine/build.sh",
    "build/osslsigncode/build.sh",
    "crates/applejack-engine/probes/hello32/build.sh",
    "crates/applejack-engine/probes/dx11-sample/build.sh",
    "crates/applejack-engine/probes/xinput-check/build.sh",
    "crates/applejack-engine/probes/cpuid/build.sh",
    "build/assemble.sh",
];

pub fn run(root: &Path, args: &[String]) -> ExitCode {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args.as_slice() {
        ["--fetch", ids @ ..] => fetch(root, ids),
        ["--check", payload] => check(root, Path::new(payload)),
        ["--close", payload] => closure::close(root, Path::new(payload)),
        ["--check-listing", listing] => check_listing(root, listing),
        ["--write-bundle-toml", spec, out] => write_bundle_toml(Path::new(spec), Path::new(out)),
        ["--print-pin", id] => print_pin(root, id, None),
        ["--print-pin", id, field] => print_pin(root, id, Some(field)),
        ["--build"] => build(root),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("[xtask] engine-payload: {message}");
            ExitCode::FAILURE
        }
    }
}

/// The offline `pins-verify` check step: both data files parse and validate.
pub fn pins_verify(root: &Path) -> ExitCode {
    let outcome = load_pins(root).and_then(|pins| load_denylist(root).map(|deny| (pins, deny)));
    match outcome {
        Ok((_, denylist)) if denylist.is_empty() => {
            eprintln!("[xtask] pins-verify: {DENYLIST_FILE} has no patterns");
            ExitCode::FAILURE
        }
        Ok((pins, denylist)) => {
            println!(
                "[xtask] pins-verify: {PINS_FILE} ok ({} pins), {DENYLIST_FILE} ok ({} patterns)",
                pins.pin.len(),
                denylist.len()
            );
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("[xtask] pins-verify: {message}");
            ExitCode::FAILURE
        }
    }
}

fn load_pins(root: &Path) -> Result<Pins, String> {
    let path = root.join(PINS_FILE);
    let text = fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    Pins::parse(&text).map_err(|error| format!("{}: {error}", path.display()))
}

pub(crate) fn load_denylist(root: &Path) -> Result<Denylist, String> {
    let path = root.join(DENYLIST_FILE);
    let text = fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    Denylist::parse(&text).map_err(|error| format!("{}: {error}", path.display()))
}

fn fetch(root: &Path, ids: &[&str]) -> Result<(), String> {
    let pins = load_pins(root)?;
    let denylist = load_denylist(root)?;
    for id in ids {
        if pins.get(id).is_none() {
            return Err(format!("no pin with id `{id}` in {PINS_FILE}"));
        }
    }
    let work = root.join(WORK_DIR);
    let selected = pins
        .pin
        .iter()
        .filter(|pin| ids.is_empty() || ids.contains(&pin.id.as_str()));
    for pin in selected {
        fetch_pin(&work, pin, &denylist)?;
    }
    Ok(())
}

/// Downloads and verifies one pin, then scans its listing against the
/// denylist before the archive lands at `dest`; a present archive is scanned
/// too, so every `--fetch` rejects a denylisted archive before any script
/// extracts it.
fn fetch_pin(work: &Path, pin: &Pin, denylist: &Denylist) -> Result<(), String> {
    let dest = work.join(&pin.dest);
    let kind = archive_kind(&pin.dest)?;
    if dest.is_file() {
        let actual = sha256_file(&dest)?;
        if actual == pin.sha256 {
            println!(
                "[xtask] fetch {}: present, hash ok ({})",
                pin.id,
                dest.display()
            );
            return scan_archive(denylist, pin, kind, &dest);
        }
        println!(
            "[xtask] fetch {}: present but hash differs, refetching ({})",
            pin.id,
            dest.display()
        );
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
    }
    let partial = dest.with_extension(format!(
        "{}.partial",
        dest.extension()
            .map(|extension| extension.to_string_lossy().into_owned())
            .unwrap_or_default()
    ));
    println!(
        "[xtask] fetch {}: {} -> {}",
        pin.id,
        pin.url,
        dest.display()
    );
    // The hash protects integrity; the protocol limits keep a redirect from
    // downgrading the pinned HTTPS URL to plain HTTP.
    let status = Command::new("curl")
        .args([
            "-fsSL",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--retry",
            "3",
            "-o",
        ])
        .arg(&partial)
        .arg(&pin.url)
        .status()
        .map_err(|error| format!("curl failed to start: {error}"))?;
    if !status.success() {
        let _ = fs::remove_file(&partial);
        return Err(format!("curl {} failed ({status})", pin.url));
    }
    let actual = sha256_file(&partial)?;
    if actual != pin.sha256 {
        let _ = fs::remove_file(&partial);
        return Err(format!(
            "{}: sha256 mismatch\n  expected {}\n  actual   {actual}",
            pin.id, pin.sha256
        ));
    }
    if let Err(message) = scan_archive(denylist, pin, kind, &partial) {
        let _ = fs::remove_file(&partial);
        return Err(message);
    }
    fs::rename(&partial, &dest).map_err(|error| format!("{}: {error}", dest.display()))?;
    println!("[xtask] fetch {}: verified {}", pin.id, pin.sha256);
    Ok(())
}

fn scan_archive(
    denylist: &Denylist,
    pin: &Pin,
    kind: ArchiveKind,
    archive: &Path,
) -> Result<(), String> {
    let listing = list_archive(kind, archive)?;
    let scanned = reject_denylisted(denylist, &pin.id, &listing)?;
    println!(
        "[xtask] fetch {}: scanned {scanned} archive paths, none denylisted",
        pin.id
    );
    Ok(())
}

/// The loader's gate: an archive whose listing matches an archive-scoped rule
/// is refused. Returns the number of paths scanned.
fn reject_denylisted(denylist: &Denylist, id: &str, listing: &[String]) -> Result<usize, String> {
    if listing.is_empty() {
        return Err(format!(
            "{id}: the archive listing is empty; refusing to trust it"
        ));
    }
    let hits = denylist.scan_archive(listing.iter().map(String::as_str));
    for hit in &hits {
        eprintln!("[xtask] denylist: {id}: {hit}");
    }
    if hits.is_empty() {
        Ok(listing.len())
    } else {
        Err(format!(
            "{id}: {} denylisted path(s) in the archive listing",
            hits.len()
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArchiveKind {
    Tar,
    TarZst,
    Zip,
    Pkg,
}

fn archive_kind(name: &str) -> Result<ArchiveKind, String> {
    let path = Path::new(name);
    let extension = path
        .extension()
        .map(|extension| extension.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let stem_is_tar = path
        .file_stem()
        .and_then(|stem| Path::new(stem).extension())
        .is_some_and(|inner| inner.eq_ignore_ascii_case("tar"));
    match extension.as_str() {
        "zst" if stem_is_tar => Ok(ArchiveKind::TarZst),
        "xz" | "gz" if stem_is_tar => Ok(ArchiveKind::Tar),
        "tar" | "tgz" => Ok(ArchiveKind::Tar),
        "zip" => Ok(ArchiveKind::Zip),
        "pkg" => Ok(ArchiveKind::Pkg),
        _ => Err(format!("{name}: unknown archive type")),
    }
}

/// Every path in the archive, one per entry, without extracting it.
fn list_archive(kind: ArchiveKind, archive: &Path) -> Result<Vec<String>, String> {
    match kind {
        ArchiveKind::Tar => command_lines("tar", &["-tf"], archive),
        ArchiveKind::TarZst => command_lines("tar", &["--zstd", "-tf"], archive),
        ArchiveKind::Zip => command_lines("unzip", &["-Z1"], archive),
        ArchiveKind::Pkg => list_pkg(archive),
    }
}

/// A package's listing: its xar entries plus the paths in the `Bom` beside
/// every `Payload` (the root `Bom` of a flat component package, or each
/// `<component>.pkg/Bom` of a product archive). `pkgutil --payload-files`
/// lists only the first component of a product archive, so the Boms are read
/// instead; they are a few kilobytes each and no Payload is expanded.
///
/// A Bom is unauthenticated metadata: nothing ties it to the Payload it sits
/// beside, so this listing is the pre-extraction gate only and the staged
/// tree scan after extraction (`build/gstreamer/fetch.sh`) stays mandatory
/// for every `.pkg` pin.
fn list_pkg(archive: &Path) -> Result<Vec<String>, String> {
    let entries = command_lines("xar", &["-tf"], archive)?;
    let boms = pkg_boms(&entries).map_err(|message| format!("{}: {message}", archive.display()))?;
    let scratch = std::env::temp_dir().join(format!("xtask-pkg-listing-{}", std::process::id()));
    let _ = fs::remove_dir_all(&scratch);
    fs::create_dir_all(&scratch).map_err(|error| format!("{}: {error}", scratch.display()))?;
    let result = list_boms(archive, &scratch, &boms).map(|paths| {
        let mut listing = entries.clone();
        listing.extend(paths);
        listing
    });
    let _ = fs::remove_dir_all(&scratch);
    result
}

/// The `Bom` beside every `Payload` in a xar listing. A `Payload` without
/// its `Bom` is refused: the listing would say nothing about what that
/// Payload expands to, and `pkgutil --expand-full` expands it all the same.
fn pkg_boms(entries: &[String]) -> Result<Vec<&str>, String> {
    let payloads = entries
        .iter()
        .map(String::as_str)
        .filter(|entry| *entry == "Payload" || entry.ends_with("/Payload"));
    let mut count = 0;
    for payload in payloads {
        let bom = format!("{}Bom", payload.strip_suffix("Payload").unwrap_or_default());
        if !entries.contains(&bom) {
            return Err(format!(
                "{payload} has no {bom} beside it; refusing to trust the listing"
            ));
        }
        count += 1;
    }
    if count == 0 {
        return Err("no Payload in the package".into());
    }
    Ok(entries
        .iter()
        .map(String::as_str)
        .filter(|entry| *entry == "Bom" || entry.ends_with("/Bom"))
        .collect())
}

fn list_boms(archive: &Path, scratch: &Path, boms: &[&str]) -> Result<Vec<String>, String> {
    let status = Command::new("xar")
        .arg("-xf")
        .arg(archive)
        .arg("-C")
        .arg(scratch)
        .args(boms)
        .status()
        .map_err(|error| format!("xar failed to start: {error}"))?;
    if !status.success() {
        return Err(format!(
            "xar -xf {} (component Boms) failed ({status})",
            archive.display()
        ));
    }
    let mut listing = Vec::new();
    for bom in boms {
        // `None` for a flat package's root `Bom`: its paths are the listing.
        let component = bom.strip_suffix("/Bom");
        for line in command_lines("lsbom", &["-s"], &scratch.join(bom))? {
            let relative = line.strip_prefix("./").unwrap_or(&line);
            if relative == "." {
                continue;
            }
            listing.push(match component {
                Some(component) => format!("{component}/{relative}"),
                None => relative.to_owned(),
            });
        }
    }
    Ok(listing)
}

fn command_lines(program: &str, args: &[&str], path: &Path) -> Result<Vec<String>, String> {
    let output = Command::new(program)
        .args(args)
        .arg(path)
        .output()
        .map_err(|error| format!("{program} failed to start: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "{program} {} {} failed ({}): {}",
            args.join(" "),
            path.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect())
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = fs::File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1 << 20];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(hex, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(hex)
}

/// What `--check` found in a payload tree.
struct Report {
    payload: PathBuf,
    paths: usize,
    machos: usize,
    /// The load chains present in the tree (the declared executables'
    /// real files and every executable under `wine/`), whose rpaths every
    /// library and module resolves through.
    chains: Vec<String>,
    hits: Vec<Hit>,
    violations: Vec<Violation>,
    /// `@rpath/` references the payload does not satisfy that a system
    /// `LC_RPATH` left to dyld instead of reporting.
    left_to_dyld: Vec<(String, String)>,
}

pub(crate) fn check(root: &Path, payload: &Path) -> Result<(), String> {
    let report = inspect(root, payload)?;
    for hit in &report.hits {
        eprintln!("[xtask] denylist: {hit}");
    }
    for violation in &report.violations {
        eprintln!("[xtask] self-contained: {violation}");
    }
    if !report.left_to_dyld.is_empty() {
        let binaries: BTreeSet<&str> = report
            .left_to_dyld
            .iter()
            .map(|(binary, _)| binary.as_str())
            .collect();
        eprintln!(
            "[xtask] self-contained: {} @rpath reference(s) left to dyld under a system LC_RPATH ({})",
            report.left_to_dyld.len(),
            binaries.into_iter().collect::<Vec<_>>().join(", ")
        );
    }
    println!(
        "[xtask] check {}: load chains: {}",
        report.payload.display(),
        if report.chains.is_empty() {
            "none present (no declared executable and no executable under wine/ in the tree)"
                .to_owned()
        } else {
            report.chains.join(", ")
        }
    );
    println!(
        "[xtask] check {}: {} paths, {} Mach-O files, {} denylist hits, {} link violations",
        report.payload.display(),
        report.paths,
        report.machos,
        report.hits.len(),
        report.violations.len()
    );
    if report.hits.is_empty() && report.violations.is_empty() {
        Ok(())
    } else {
        Err("payload check failed".into())
    }
}

fn inspect(root: &Path, payload: &Path) -> Result<Report, String> {
    let payload = payload
        .canonicalize()
        .map_err(|error| format!("{}: {error}", payload.display()))?;
    let denylist = load_denylist(root)?;
    let spec = read_spec(&payload)?;
    let declared = declared_executables(&spec);
    // Canonical like `payload`, so a checkout reached through a symlink still
    // reports "links into the build directory" rather than the generic reason.
    let build_dir = root.join(WORK_DIR);
    let build_dir = build_dir.canonicalize().unwrap_or(build_dir);
    let mut entries = Vec::new();
    walk(&payload, &payload, &mut entries)?;
    let paths: Vec<&str> = entries
        .iter()
        .map(|entry| entry.relative.as_str())
        .collect();
    let mut hits = denylist.scan(paths.iter().copied());
    let mut violations: Vec<Violation> = Vec::new();
    let mut machos: Vec<MachO> = Vec::new();
    for entry in &entries {
        if let Some(target) = &entry.link_target {
            let target_text = target.to_string_lossy();
            hits.extend(
                denylist
                    .scan([target_text.as_ref()])
                    .into_iter()
                    .map(|hit| Hit {
                        path: format!("{} -> {}", entry.relative, hit.path),
                        ..hit
                    }),
            );
            if let Some(reason) = symlink_reason(&payload, &entry.absolute, target) {
                violations.push(Violation {
                    binary: entry.relative.clone(),
                    reference: format!("-> {target_text}"),
                    reason: reason.to_owned(),
                });
            }
            continue;
        }
        if !entry.is_file || !is_macho(&entry.absolute)? {
            continue;
        }
        let listing = otool(&entry.absolute, "-L")?;
        let load_commands = otool(&entry.absolute, "-l")?;
        if !load_commands
            .lines()
            .any(|line| line.trim_start().starts_with("cmd "))
        {
            return Err(format!(
                "otool -l {}: no load commands in a file with a Mach-O magic",
                entry.absolute.display()
            ));
        }
        machos.push(MachO {
            relative: entry.relative.clone(),
            listing,
            load_commands,
        });
    }
    for macho in &machos {
        violations.extend(check_otool_listing(
            &macho.relative,
            &macho.listing,
            &payload,
            &build_dir,
        ));
        violations.extend(check_rpaths(
            &macho.relative,
            &macho.load_commands,
            &payload,
            &build_dir,
        ));
    }
    violations.extend(duplicate_install_names(&machos));
    violations.extend(shipped_elsewhere_violations(&spec, &machos));
    let resolves = |candidate: &Path| {
        fs::canonicalize(payload.join(candidate))
            .is_ok_and(|real| real.starts_with(&payload) && real.is_file())
    };
    let targets = declared_chain_targets(&payload, &declared, &machos);
    violations.extend(targets.violations);
    let targets: Vec<&str> = targets.targets.iter().map(String::as_str).collect();
    let chained = resolve_along_chains(&machos, &targets, &resolves);
    violations.extend(chained.violations);
    Ok(Report {
        payload,
        paths: entries.len(),
        machos: machos.len(),
        chains: chained.chains,
        hits,
        violations,
        left_to_dyld: chained.left_to_dyld,
    })
}

/// dyld keeps one image per install name per process, so two regular files
/// carrying the same `LC_ID_DYLIB` let load order pick the library every
/// client gets; the closure links such duplicates to one file (symlinks are
/// not inspected here, so they pass).
fn duplicate_install_names(machos: &[MachO]) -> Vec<Violation> {
    let mut by_id: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for macho in machos {
        if let Some(id) = id_entry(&macho.load_commands) {
            by_id.entry(id).or_default().push(&macho.relative);
        }
    }
    by_id
        .into_iter()
        .filter(|(_, files)| files.len() > 1)
        .map(|(id, files)| Violation {
            binary: files.join(", "),
            reference: id.to_owned(),
            reason: "several regular files carry this install name; dyld keeps one image per install name per process, so load order would pick the library".to_owned(),
        })
        .collect()
}

pub(crate) const BUNDLE_FILE: &str = "BUNDLE.toml";

/// The payload's `BUNDLE.toml`, which `--check` reads for the declared
/// executables and the library rows.
fn read_spec(payload: &Path) -> Result<BundleSpec, String> {
    let path = payload.join(BUNDLE_FILE);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(format!(
                "no {BUNDLE_FILE}: cannot determine load chains ({})",
                path.display()
            ));
        }
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    BundleSpec::parse(&text).map_err(|error| format!("{}: {error}", path.display()))
}

/// The executables `BUNDLE.toml` declares, bundle-relative: the loader, the
/// server, the preloader when present, and the probes. Together with the
/// executables under `wine/` these are the load chains that exist at run
/// time; an executable elsewhere (one of the tools a component ships, say)
/// loads nothing of the payload.
fn declared_executables(spec: &BundleSpec) -> Vec<String> {
    let mut declared = vec![spec.wine.loader.clone(), spec.wine.server.clone()];
    declared.extend(spec.wine.preloader.clone());
    declared.extend([
        spec.probes.cpuid.clone(),
        spec.probes.dx11_sample.clone(),
        spec.probes.xinput_check.clone(),
    ]);
    declared
}

/// Each library row `BUNDLE.toml` carries for a file inside another
/// component's tree names a regular Mach-O of the tree whose `LC_ID_DYLIB`
/// current version is the one `SHIPPED_ELSEWHERE` records: the comparison
/// `--close` makes before its first round, so a row left stale by a pin
/// bump is a violation here too.
fn shipped_elsewhere_violations(spec: &BundleSpec, machos: &[MachO]) -> Vec<Violation> {
    let mut violations = Vec::new();
    for elsewhere in SHIPPED_ELSEWHERE {
        if !spec.components.libraries.contains_key(elsewhere.key) {
            continue;
        }
        let reason = match machos.iter().find(|macho| macho.relative == elsewhere.file) {
            Some(macho) => match elsewhere_matches(elsewhere, &macho.load_commands) {
                Ok(()) => continue,
                Err(reason) => reason,
            },
            None => format!(
                "{}: not a regular Mach-O file of the payload, which the row describes",
                elsewhere.file
            ),
        };
        violations.push(Violation {
            binary: BUNDLE_FILE.to_owned(),
            reference: format!("[components.{}]", elsewhere.key),
            reason,
        });
    }
    violations
}

struct DeclaredTargets {
    /// The payload-relative real files of the declared Mach-O executables.
    targets: Vec<String>,
    violations: Vec<Violation>,
}

/// Resolves each declared executable through symlinks to its real file.
/// dyld sets `@executable_path` from the real file, so its `MachO` is the
/// chain. A declared path with no entry at all is a tree before Wine is
/// built and is skipped; a dangling link, a link out of the payload, and a
/// Mach-O that is no executable are violations. A PE probe is not a chain.
fn declared_chain_targets(
    payload: &Path,
    declared: &[String],
    machos: &[MachO],
) -> DeclaredTargets {
    let mut targets = Vec::new();
    let mut violations = Vec::new();
    let mut violation = |declared_path: &str, reason: &str| {
        violations.push(Violation {
            binary: BUNDLE_FILE.to_owned(),
            reference: declared_path.to_owned(),
            reason: reason.to_owned(),
        });
    };
    for declared_path in declared {
        let absolute = payload.join(declared_path);
        if fs::symlink_metadata(&absolute).is_err() {
            continue;
        }
        let Ok(real) = fs::canonicalize(&absolute) else {
            violation(declared_path, "declared executable is a dangling symlink");
            continue;
        };
        let Ok(relative) = real.strip_prefix(payload) else {
            violation(
                declared_path,
                "declared executable resolves outside the payload",
            );
            continue;
        };
        let relative = relative.to_string_lossy();
        match machos.iter().find(|macho| macho.relative == relative) {
            Some(macho) if is_executable(&macho.load_commands) => {
                targets.push(relative.into_owned());
            }
            Some(_) => violation(
                declared_path,
                "declared executable is a Mach-O without LC_MAIN or LC_UNIXTHREAD",
            ),
            None => {}
        }
    }
    DeclaredTargets {
        targets,
        violations,
    }
}

/// Every executable under this subtree is a load chain: Wine 11's
/// `bin/wine` is a wrapper that re-execs `lib/wine/x86_64-unix/wine`
/// (`dlls/ntdll/unix/loader.c`, `wineloader`), which is the process that
/// dlopens the Unix modules, and `BUNDLE.toml` does not name it.
const WINE_SUBTREE: &str = "wine/";

/// One executable's load chain: where `@executable_path` lands and the
/// rpaths dyld carries into every image loaded in its process.
struct Chain {
    relative: String,
    dir: Vec<PathBuf>,
    rpaths: Vec<PathBuf>,
    system_rpath: bool,
}

impl Chain {
    fn new(macho: &MachO) -> Self {
        let dir = vec![binary_dir(&macho.relative)];
        Self {
            relative: macho.relative.clone(),
            rpaths: relocated_rpaths(&macho.relative, &macho.load_commands, &dir),
            system_rpath: has_system_rpath(&macho.load_commands),
            dir,
        }
    }
}

struct Resolved {
    chains: Vec<String>,
    violations: Vec<Violation>,
    left_to_dyld: Vec<(String, String)>,
}

/// Resolves the relative references of every Mach-O along the load chains
/// present in the tree: the executables in `targets` (the declared
/// executables' real files, bundle-relative) and every executable under
/// [`WINE_SUBTREE`].
///
/// dyld searches `@rpath/` through the rpaths of the image and of the
/// executable whose process loaded it, so a library or module must resolve
/// under each chain's rpaths plus its own; a library one chain can load and
/// another cannot is a violation naming every chain that fails. An
/// executable loads only through itself, and one that is no chain
/// contributes nothing to any other binary's search. With no chain present
/// (a tree before Wine is built) each binary searches its own rpaths alone.
fn resolve_along_chains(
    machos: &[MachO],
    targets: &[&str],
    resolves: &dyn Fn(&Path) -> bool,
) -> Resolved {
    let chains: Vec<Chain> = machos
        .iter()
        .filter(|macho| {
            is_executable(&macho.load_commands)
                && (targets.contains(&macho.relative.as_str())
                    || macho.relative.starts_with(WINE_SUBTREE))
        })
        .map(Chain::new)
        .collect();
    let mut violations = Vec::new();
    let mut left_to_dyld = Vec::new();
    for macho in machos {
        let own_chain;
        let via: Vec<&Chain> = if is_executable(&macho.load_commands) {
            own_chain = Chain::new(macho);
            vec![&own_chain]
        } else {
            chains.iter().collect()
        };
        let own_system_rpath = has_system_rpath(&macho.load_commands);
        // One entry per failing reference, in load-command order, with the
        // chains it fails under (none when the binary loads through itself).
        let mut failing: Vec<(Violation, Vec<String>)> = Vec::new();
        let mut deferred: BTreeSet<&str> = BTreeSet::new();
        let mut search_chain = |exe_dirs: &[PathBuf],
                                rpaths: &[PathBuf],
                                system_rpath: bool,
                                chain: Option<&str>| {
            let mut rpath_dirs: BTreeSet<PathBuf> = rpaths.iter().cloned().collect();
            rpath_dirs.extend(relocated_rpaths(
                &macho.relative,
                &macho.load_commands,
                exe_dirs,
            ));
            let search = Search {
                rpath_dirs: &rpath_dirs,
                exe_dirs,
                system_rpath: system_rpath || own_system_rpath,
            };
            for violation in
                check_references_resolve(&macho.relative, &macho.load_commands, &search, resolves)
            {
                let index = failing
                    .iter()
                    .position(|(known, _)| known.reference == violation.reference)
                    .unwrap_or_else(|| {
                        failing.push((violation, Vec::new()));
                        failing.len() - 1
                    });
                failing[index].1.extend(chain.map(str::to_owned));
            }
            deferred.extend(references_left_to_dyld(
                &macho.relative,
                &macho.load_commands,
                &search,
                resolves,
            ));
        };
        if via.is_empty() {
            search_chain(&[], &[], false, None);
        }
        for chain in via {
            let name = (chain.relative != macho.relative).then_some(chain.relative.as_str());
            search_chain(&chain.dir, &chain.rpaths, chain.system_rpath, name);
        }
        let reported: BTreeSet<&str> = failing
            .iter()
            .map(|(violation, _)| violation.reference.as_str())
            .collect();
        left_to_dyld.extend(
            deferred
                .into_iter()
                .filter(|reference| !reported.contains(*reference))
                .map(|reference| (macho.relative.clone(), reference.to_owned())),
        );
        violations.extend(failing.into_iter().map(|(mut violation, chains)| {
            let suffix = match chains.as_slice() {
                [] => String::new(),
                [chain] => format!("; load chain {chain}"),
                chains => format!("; load chains {}", chains.join(", ")),
            };
            violation.reason.push_str(&suffix);
            violation
        }));
    }
    Resolved {
        chains: chains.into_iter().map(|chain| chain.relative).collect(),
        violations,
        left_to_dyld,
    }
}

/// One Mach-O of the payload with both `otool` outputs, kept so the
/// declared executables' load chains exist before any reference is resolved.
struct MachO {
    relative: String,
    listing: String,
    load_commands: String,
}

/// Why a symlink would not survive the payload moving to another machine.
///
/// The lexical walk from the link's payload-relative directory must never
/// climb above the root, even when the target comes back in through the
/// root's own name: the installer renames the root. The real resolution is
/// the second authority, since a `..` after a component that is itself a
/// symlink is popped lexically as though it were a directory. `payload` is
/// canonical, so `starts_with` on the canonical target is exact.
fn symlink_reason(payload: &Path, link: &Path, target: &Path) -> Option<&'static str> {
    if target.is_absolute() {
        return Some("absolute symlink target");
    }
    let link_dir = link
        .parent()
        .and_then(|dir| dir.strip_prefix(payload).ok())
        .unwrap_or(Path::new(""));
    if normalize_relative(&link_dir.join(target)).is_none() {
        return Some("symlink target climbs out of the payload");
    }
    match fs::canonicalize(link) {
        Err(_) => Some("dangling symlink"),
        Ok(real) if !real.starts_with(payload) => Some("symlink resolves outside the payload"),
        Ok(_) => None,
    }
}

fn check_listing(root: &Path, listing: &str) -> Result<(), String> {
    let denylist = load_denylist(root)?;
    let text = if listing == "-" {
        let mut text = String::new();
        io::stdin()
            .read_to_string(&mut text)
            .map_err(|error| format!("stdin: {error}"))?;
        text
    } else {
        fs::read_to_string(listing).map_err(|error| format!("{listing}: {error}"))?
    };
    let paths: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    // A lister that failed or produced nothing must not pass the gate.
    if paths.is_empty() {
        return Err("the listing is empty; refusing to pass it".into());
    }
    let hits = denylist.scan(paths.iter().copied());
    for hit in &hits {
        eprintln!("[xtask] denylist: {hit}");
    }
    if hits.is_empty() {
        println!("[xtask] check-listing: {} paths clean", paths.len());
        Ok(())
    } else {
        Err(format!("{} denylisted path(s) in the listing", hits.len()))
    }
}

const NOTICES_FILE: &str = "THIRD_PARTY_NOTICES.md";

/// Renders `BUNDLE.toml` to `out` and `THIRD_PARTY_NOTICES.md` beside it.
fn write_bundle_toml(spec: &Path, out: &Path) -> Result<(), String> {
    let text = fs::read_to_string(spec).map_err(|error| format!("{}: {error}", spec.display()))?;
    let parsed =
        BundleSpec::parse(&text).map_err(|error| format!("{}: {error}", spec.display()))?;
    let rendered = parsed
        .to_toml()
        .map_err(|error| format!("{}: {error}", spec.display()))?;
    if let Some(parent) = out.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
    }
    fs::write(out, rendered).map_err(|error| format!("{}: {error}", out.display()))?;
    println!("[xtask] wrote {}", out.display());
    let notices = out.with_file_name(NOTICES_FILE);
    let rendered = parsed
        .to_notices()
        .map_err(|error| format!("{}: {error}", spec.display()))?;
    fs::write(&notices, rendered).map_err(|error| format!("{}: {error}", notices.display()))?;
    println!("[xtask] wrote {}", notices.display());
    Ok(())
}

fn print_pin(root: &Path, id: &str, field: Option<&str>) -> Result<(), String> {
    let pins = load_pins(root)?;
    let pin = pins
        .get(id)
        .ok_or_else(|| format!("no pin with id `{id}` in {PINS_FILE}"))?;
    let fields = [
        ("id", pin.id.as_str()),
        ("kind", pin.kind.as_str()),
        ("url", pin.url.as_str()),
        ("sha256", pin.sha256.as_str()),
        ("version", pin.version.as_str()),
        ("license", pin.license.as_str()),
        ("dest", pin.dest.as_str()),
    ];
    match field {
        Some(name) => {
            let (_, value) = fields
                .iter()
                .find(|(key, _)| *key == name)
                .ok_or_else(|| format!("pin `{id}` has no field `{name}`"))?;
            println!("{value}");
        }
        None => {
            for (key, value) in fields {
                println!("{key}={value}");
            }
        }
    }
    Ok(())
}

fn build(root: &Path) -> Result<(), String> {
    run_script(root, "build/bootstrap.sh", &["--check"]).map_err(|message| {
        format!(
            "{message}\n  the host lacks build prerequisites; see docs/engines/payload-build.md"
        )
    })?;
    for script in BUILD_SCRIPTS {
        run_script(root, script, &[])?;
    }
    // assemble.sh already closed and checked the tree it swapped in; checking
    // the final location proves the swap kept it whole.
    let payload = payload_dir(root)?;
    println!(
        "[xtask] build scripts finished; checking {}",
        payload.display()
    );
    check(root, &payload)
}

/// Where `build/lib.sh`'s `payload_dir` assembles the tree:
/// `$APPLEJACK_PAYLOAD_DIR`, else `fixtures/payloads/rosetta-wine-<wine version>`.
fn payload_dir(root: &Path) -> Result<PathBuf, String> {
    if let Some(dir) = std::env::var_os("APPLEJACK_PAYLOAD_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let pins = load_pins(root)?;
    let wine = pins
        .get("wine")
        .ok_or_else(|| format!("no pin with id `wine` in {PINS_FILE}"))?;
    Ok(root
        .join("fixtures/payloads")
        .join(format!("rosetta-wine-{}", wine.version)))
}

fn run_script(root: &Path, script: &str, args: &[&str]) -> Result<(), String> {
    let path = root.join(script);
    if !path.is_file() {
        return Err(format!("{}: missing script", path.display()));
    }
    println!("[xtask] sh {script} {}", args.join(" "));
    let status = Command::new("sh")
        .arg(&path)
        .args(args)
        .current_dir(root)
        .status()
        .map_err(|error| format!("{script}: failed to start: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{script} failed ({status})"))
    }
}

pub(crate) struct Entry {
    pub(crate) relative: String,
    pub(crate) absolute: PathBuf,
    pub(crate) is_file: bool,
    /// Set for a symlink: where it points, as stored.
    pub(crate) link_target: Option<PathBuf>,
}

pub(crate) fn walk(base: &Path, dir: &Path, entries: &mut Vec<Entry>) -> Result<(), String> {
    let listing = fs::read_dir(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    let mut children: Vec<PathBuf> = listing
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<_, _>>()
        .map_err(|error| format!("{}: {error}", dir.display()))?;
    children.sort();
    for child in children {
        let metadata = fs::symlink_metadata(&child)
            .map_err(|error| format!("{}: {error}", child.display()))?;
        let relative = child
            .strip_prefix(base)
            .map_err(|error| format!("{}: {error}", child.display()))?
            .to_string_lossy()
            .into_owned();
        let file_type = metadata.file_type();
        let link_target = if file_type.is_symlink() {
            Some(fs::read_link(&child).map_err(|error| format!("{}: {error}", child.display()))?)
        } else {
            None
        };
        entries.push(Entry {
            relative,
            absolute: child.clone(),
            is_file: file_type.is_file(),
            link_target,
        });
        // Symlinked directories stay unexpanded: their targets are listed on
        // their own, and the layout must not depend on where a link points.
        if file_type.is_dir() {
            walk(base, &child, entries)?;
        }
    }
    Ok(())
}

// Thin 32/64-bit Mach-O and fat headers (FAT_MAGIC and FAT_MAGIC_64), both
// byte orders.
const MACHO_MAGICS: [[u8; 4]; 8] = [
    [0xfe, 0xed, 0xfa, 0xce],
    [0xce, 0xfa, 0xed, 0xfe],
    [0xfe, 0xed, 0xfa, 0xcf],
    [0xcf, 0xfa, 0xed, 0xfe],
    [0xca, 0xfe, 0xba, 0xbe],
    [0xbe, 0xba, 0xfe, 0xca],
    [0xca, 0xfe, 0xba, 0xbf],
    [0xbf, 0xba, 0xfe, 0xca],
];

pub(crate) fn is_macho(path: &Path) -> Result<bool, String> {
    let mut file = fs::File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut magic = [0_u8; 4];
    let mut read = 0;
    while read < magic.len() {
        let count = file
            .read(&mut magic[read..])
            .map_err(|error| format!("{}: {error}", path.display()))?;
        if count == 0 {
            return Ok(false);
        }
        read += count;
    }
    Ok(MACHO_MAGICS.contains(&magic))
}

pub(crate) fn otool(path: &Path, flag: &str) -> Result<String, String> {
    let output = Command::new("otool")
        .arg(flag)
        .arg(path)
        .output()
        .map_err(|error| format!("otool failed to start: {error}"))?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    // `otool -l` reports a malformed fat file on stderr and still exits 0.
    if !output.status.success() || stderr.contains("error:") {
        return Err(format!(
            "otool {flag} {} failed ({}): {}",
            path.display(),
            output.status,
            stderr.trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use super::{
        ArchiveKind, BUNDLE_FILE, MachO, archive_kind, check_listing, declared_chain_targets,
        inspect, is_macho, list_archive, load_denylist, pkg_boms, reject_denylisted,
        resolve_along_chains,
    };
    use crate::bundle_toml::{BundleSpec, SHIPPED_ELSEWHERE};

    /// The committed rendering of `build/bundle-spec.toml`: every `--check`
    /// reads the declared executables from it.
    const BUNDLE_TOML: &str = include_str!("../tests/fixtures/BUNDLE.toml");

    /// The fixture without the rows for the two framework libraries: these
    /// trees ship no framework, and a row whose file is absent is a
    /// violation of its own (`a_framework_row_without_its_file_is_a_violation`).
    fn write_bundle_fixture(payload: &Path) {
        let mut spec = BundleSpec::parse(BUNDLE_TOML).expect("the fixture parses");
        for elsewhere in SHIPPED_ELSEWHERE {
            spec.components.libraries.remove(elsewhere.key);
        }
        fs::write(payload.join(BUNDLE_FILE), spec.to_toml().expect("renders"))
            .expect("BUNDLE.toml");
    }

    /// The provenance reviewer's `clone-ver` in miniature: a tree whose
    /// `BUNDLE.toml` carries the two framework rows but holds neither file
    /// reports each row, naming the file.
    #[test]
    fn a_framework_row_without_its_file_is_a_violation() {
        let dir = scratch("framework-rows");
        let payload = dir.join("payload");
        fs::create_dir_all(payload.join("wine/lib")).expect("payload");
        fs::write(payload.join(BUNDLE_FILE), BUNDLE_TOML).expect("BUNDLE.toml");
        let report = inspect(&root(), &payload).expect("report");
        let violations: Vec<String> = report.violations.iter().map(ToString::to_string).collect();
        assert_eq!(
            violations,
            [
                "BUNDLE.toml: [components.freetype] (wine/lib/GStreamer.framework/Versions/1.0/lib/libfreetype.6.dylib: not a regular Mach-O file of the payload, which the row describes)",
                "BUNDLE.toml: [components.proxy-libintl] (wine/lib/GStreamer.framework/Versions/1.0/lib/libintl.8.dylib: not a regular Mach-O file of the payload, which the row describes)",
            ]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    fn root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root")
            .to_path_buf()
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "xtask-engine-payload-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[test]
    fn the_loader_refuses_a_denylisted_listing() {
        let denylist = load_denylist(&root()).expect("repository denylist");
        let listing = vec![
            "wine-11.0/configure".to_owned(),
            "lib/external/D3DMetal.framework/D3DMetal".to_owned(),
        ];
        let error = reject_denylisted(&denylist, "x", &listing).unwrap_err();
        assert!(error.contains("denylisted"), "{error}");
        let clean = vec!["wine-11.0/configure".to_owned()];
        assert_eq!(reject_denylisted(&denylist, "x", &clean), Ok(1));
        let error = reject_denylisted(&denylist, "x", &[]).unwrap_err();
        assert!(error.contains("empty"), "{error}");
    }

    #[test]
    fn a_tar_archive_is_listed_and_scanned_before_extraction() {
        let dir = scratch("tar");
        let tree = dir.join("tree/lib/external/D3DMetal.framework");
        fs::create_dir_all(&tree).expect("tree");
        fs::write(tree.join("D3DMetal"), b"x").expect("file");
        let archive = dir.join("bad.tar.gz");
        let status = Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(dir.join("tree"))
            .arg("lib")
            .status()
            .expect("tar");
        assert!(status.success());
        assert_eq!(archive_kind("bad.tar.gz").expect("kind"), ArchiveKind::Tar);
        let listing = list_archive(ArchiveKind::Tar, &archive).expect("listing");
        assert!(
            listing
                .iter()
                .any(|path| path.ends_with("lib/external/D3DMetal.framework/D3DMetal")),
            "{listing:?}"
        );
        let denylist = load_denylist(&root()).expect("repository denylist");
        let error = reject_denylisted(&denylist, "bad", &listing).unwrap_err();
        assert!(error.contains("denylisted"), "{error}");
        assert!(archive_kind("x.dmg").is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_listing_fails_closed() {
        let dir = scratch("listing");
        let empty = dir.join("empty.txt");
        fs::write(&empty, "\n\n").expect("file");
        let error = check_listing(&root(), &empty.to_string_lossy()).unwrap_err();
        assert!(error.contains("empty"), "{error}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn symlinks_out_of_the_payload_are_violations() {
        let dir = scratch("symlinks");
        let payload = dir.join("payload");
        fs::create_dir_all(payload.join("wine/lib")).expect("payload");
        write_bundle_fixture(&payload);
        fs::write(payload.join("wine/lib/libreal.dylib"), b"not a mach-o").expect("file");
        symlink(
            "/usr/local/lib/libfreetype.6.dylib",
            payload.join("wine/lib/libfreetype.6.dylib"),
        )
        .expect("absolute link");
        symlink("../../../outside", payload.join("wine/lib/escape")).expect("escaping link");
        symlink(
            "/Library/Apple/usr/lib/d3d/D3DMetal.framework",
            payload.join("wine/lib/gpu"),
        )
        .expect("denylisted link");
        symlink("missing.dylib", payload.join("wine/lib/dangling")).expect("dangling link");
        symlink("libreal.dylib", payload.join("wine/lib/libalias.dylib")).expect("good link");
        let report = inspect(&root(), &payload).expect("report");
        let mut violations: Vec<(&str, &str)> = report
            .violations
            .iter()
            .map(|violation| (violation.binary.as_str(), violation.reason.as_str()))
            .collect();
        violations.sort_unstable();
        assert_eq!(
            violations,
            [
                ("wine/lib/dangling", "dangling symlink"),
                (
                    "wine/lib/escape",
                    "symlink target climbs out of the payload"
                ),
                ("wine/lib/gpu", "absolute symlink target"),
                ("wine/lib/libfreetype.6.dylib", "absolute symlink target"),
            ]
        );
        assert_eq!(report.hits.len(), 1, "{:?}", report.hits);
        assert!(
            report.hits[0].path.starts_with("wine/lib/gpu -> "),
            "{}",
            report.hits[0]
        );
        assert_eq!(report.machos, 0);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A `..` after a component that is itself a symlink: lexically the
    /// target stays inside, on disk it does not (the Fix Verifier's
    /// `fv/esc/payload` shape, with `/bin/sh` as the outside file so the test
    /// does not depend on Homebrew).
    #[test]
    fn a_relative_symlink_through_an_upward_link_is_a_violation() {
        let dir = scratch("symlink-escape");
        let payload = dir.join("payload");
        let mut deep = payload.join("wine/lib");
        for level in 1..=14 {
            deep.push(format!("d{level}"));
        }
        fs::create_dir_all(&deep).expect("deep tree");
        write_bundle_fixture(&payload);
        // 16 levels below the payload root; `up` resolves to that root.
        symlink("../".repeat(16).trim_end_matches('/'), deep.join("up")).expect("up link");
        symlink(
            format!("up/{}bin/sh", "../".repeat(12)),
            deep.join("libfreetype.6.dylib"),
        )
        .expect("escaping link");
        let report = inspect(&root(), &payload).expect("report");
        let violations: Vec<(&str, &str)> = report
            .violations
            .iter()
            .map(|violation| (violation.binary.as_str(), violation.reason.as_str()))
            .collect();
        assert_eq!(
            violations,
            [(
                "wine/lib/d1/d2/d3/d4/d5/d6/d7/d8/d9/d10/d11/d12/d13/d14/libfreetype.6.dylib",
                "symlink resolves outside the payload"
            )]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The Fix Verifier's `reenter` fixture: the target climbs above the
    /// root and comes back in through the root's name. On the build host it
    /// resolves; after the installer renames the root it dangles.
    #[test]
    fn reenter_symlink_through_the_root_name_is_a_violation() {
        let dir = scratch("symlink-reenter");
        let payload = dir.join("payload");
        fs::create_dir_all(payload.join("wine/lib")).expect("payload");
        write_bundle_fixture(&payload);
        fs::write(payload.join("wine/lib/libfoo.real.dylib"), b"not a mach-o").expect("file");
        symlink(
            "../../../payload/wine/lib/libfoo.real.dylib",
            payload.join("wine/lib/libfoo.dylib"),
        )
        .expect("re-entering link");
        assert!(payload.join("wine/lib/libfoo.dylib").is_file());
        let report = inspect(&root(), &payload).expect("report");
        let violations: Vec<(&str, &str)> = report
            .violations
            .iter()
            .map(|violation| (violation.binary.as_str(), violation.reason.as_str()))
            .collect();
        assert_eq!(
            violations,
            [(
                "wine/lib/libfoo.dylib",
                "symlink target climbs out of the payload"
            )]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pkg_payload_without_a_bom_is_refused() {
        let listing =
            |entries: &[&str]| entries.iter().map(|e| (*e).to_owned()).collect::<Vec<_>>();
        let flat = listing(&["Bom", "Payload", "PackageInfo"]);
        assert_eq!(pkg_boms(&flat), Ok(vec!["Bom"]));
        let product = listing(&[
            "Distribution",
            "a.pkg",
            "a.pkg/Bom",
            "a.pkg/Payload",
            "a.pkg/PackageInfo",
            "bad.pkg",
            "bad.pkg/Bom",
            "bad.pkg/Payload",
            "bad.pkg/PackageInfo",
        ]);
        assert_eq!(pkg_boms(&product), Ok(vec!["a.pkg/Bom", "bad.pkg/Bom"]));
        let bomless = listing(&[
            "Distribution",
            "a.pkg",
            "a.pkg/Bom",
            "a.pkg/Payload",
            "a.pkg/PackageInfo",
            "bad.pkg",
            "bad.pkg/Payload",
            "bad.pkg/PackageInfo",
        ]);
        let error = pkg_boms(&bomless).unwrap_err();
        assert!(
            error.contains("bad.pkg/Payload has no bad.pkg/Bom"),
            "{error}"
        );
        assert!(pkg_boms(&listing(&["Distribution"])).is_err());

        // The real lister: a flat package built with pkgbuild lists its
        // denylisted file; the same package with its Bom removed is refused.
        let dir = scratch("pkg");
        let tree = dir.join("root/lib/external/D3DMetal.framework");
        fs::create_dir_all(&tree).expect("tree");
        fs::write(tree.join("D3DMetal"), b"x").expect("file");
        let flat = dir.join("flat.pkg");
        let status = Command::new("pkgbuild")
            .args(["--root"])
            .arg(dir.join("root"))
            .args(["--identifier", "com.applejack.xtask.test", "--version", "1"])
            .arg(&flat)
            .output()
            .expect("pkgbuild");
        assert!(status.status.success(), "{status:?}");
        let listed = list_archive(ArchiveKind::Pkg, &flat).expect("flat listing");
        assert!(
            listed.contains(&"lib/external/D3DMetal.framework/D3DMetal".to_owned()),
            "{listed:?}"
        );
        let unpacked = dir.join("unpacked");
        fs::create_dir_all(&unpacked).expect("unpack dir");
        let status = Command::new("xar")
            .arg("-xf")
            .arg(&flat)
            .arg("-C")
            .arg(&unpacked)
            .args(["Payload", "PackageInfo"])
            .status()
            .expect("xar -x");
        assert!(status.success());
        // `-C` applies to extraction only; creation names paths from the cwd.
        let bomless = dir.join("bomless.pkg");
        let status = Command::new("xar")
            .arg("-cf")
            .arg(&bomless)
            .args(["Payload", "PackageInfo"])
            .current_dir(&unpacked)
            .status()
            .expect("xar -c");
        assert!(status.success());
        let entries = super::command_lines("xar", &["-tf"], &bomless).expect("xar -t");
        assert_eq!(entries, ["Payload", "PackageInfo"]);
        let error = list_archive(ArchiveKind::Pkg, &bomless).unwrap_err();
        assert!(error.contains("Payload has no Bom"), "{error}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_payload_without_bundle_toml_cannot_be_checked() {
        let dir = scratch("no-bundle");
        let payload = dir.join("payload");
        fs::create_dir_all(payload.join("wine/lib")).expect("payload");
        let Err(error) = inspect(&root(), &payload) else {
            panic!("a payload without BUNDLE.toml was checked");
        };
        assert!(
            error.starts_with("no BUNDLE.toml: cannot determine load chains"),
            "{error}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    fn macho(relative: &str, load_commands: &str) -> MachO {
        MachO {
            relative: relative.to_owned(),
            listing: String::new(),
            load_commands: format!("{relative}:\n{load_commands}"),
        }
    }

    fn files(paths: &[&str]) -> std::collections::BTreeSet<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }

    fn describe(violations: &[super::Violation]) -> Vec<(String, String, String)> {
        violations
            .iter()
            .map(|violation| {
                (
                    violation.binary.clone(),
                    violation.reference.clone(),
                    violation.reason.clone(),
                )
            })
            .collect()
    }

    // The Fix Verifier's `two-exe` fixture: `wine/bin/a` reaches `deps/`
    // through its rpath, `tools/bin/b` has none; both load `libmid.dylib`,
    // which names `@rpath/libfoo.dylib` with no rpath of its own. dyld loads
    // it under `a` and fails under `b`.
    const TWO_EXE_A: &str = "\
Load command 10
          cmd LC_MAIN
Load command 11
          cmd LC_LOAD_DYLIB
         name @loader_path/../lib/libmid.dylib (offset 24)
Load command 12
          cmd LC_RPATH
         path @loader_path/../lib/deps (offset 12)
";
    const TWO_EXE_B: &str = "\
Load command 10
          cmd LC_MAIN
Load command 11
          cmd LC_LOAD_DYLIB
         name @loader_path/../../wine/lib/libmid.dylib (offset 24)
";
    const TWO_EXE_MID: &str = "\
Load command 0
          cmd LC_ID_DYLIB
         name @loader_path/../lib/libmid.dylib (offset 24)
Load command 1
          cmd LC_LOAD_DYLIB
         name @rpath/libfoo.dylib (offset 24)
";
    const TWO_EXE_FOO: &str = "\
Load command 0
          cmd LC_ID_DYLIB
         name @rpath/libfoo.dylib (offset 24)
";

    #[test]
    fn a_library_only_one_declared_executable_can_load_is_a_violation() {
        let machos = [
            macho("tools/bin/b", TWO_EXE_B),
            macho("wine/bin/a", TWO_EXE_A),
            macho("wine/lib/deps/libfoo.dylib", TWO_EXE_FOO),
            macho("wine/lib/libmid.dylib", TWO_EXE_MID),
        ];
        let files = files(&[
            "tools/bin/b",
            "wine/bin/a",
            "wine/lib/deps/libfoo.dylib",
            "wine/lib/libmid.dylib",
        ]);
        let resolves = |candidate: &Path| files.contains(candidate);

        let outcome = resolve_along_chains(&machos, &["wine/bin/a", "tools/bin/b"], &resolves);
        assert_eq!(outcome.chains, ["tools/bin/b", "wine/bin/a"]);
        assert_eq!(
            describe(&outcome.violations),
            [(
                "wine/lib/libmid.dylib".to_owned(),
                "@rpath/libfoo.dylib".to_owned(),
                "@rpath reference resolves through no LC_RPATH of the binary or of the executable loading it; load chain tools/bin/b".to_owned(),
            )]
        );
        assert_eq!(outcome.left_to_dyld, Vec::<(String, String)>::new());

        // Declared alone, `a` loads everything; `b` is then an executable
        // outside `wine/` that is no chain and loads only through itself.
        let outcome = resolve_along_chains(&machos, &["wine/bin/a"], &resolves);
        assert_eq!(outcome.chains, ["wine/bin/a"]);
        assert_eq!(describe(&outcome.violations), []);

        // Declared alone, `b` cannot load `libmid.dylib`; `a` stays a chain
        // under `wine/` but its rpath does not stand in for `b`'s.
        let outcome = resolve_along_chains(&machos, &["tools/bin/b"], &resolves);
        assert_eq!(outcome.chains, ["tools/bin/b", "wine/bin/a"]);
        assert_eq!(outcome.violations.len(), 1, "{:?}", outcome.violations);
        assert_eq!(outcome.violations[0].binary, "wine/lib/libmid.dylib");
        assert!(
            outcome.violations[0]
                .reason
                .ends_with("; load chain tools/bin/b"),
            "{}",
            outcome.violations[0]
        );
    }

    // The Fix Verifier's `three` fixture: `probes/cpuid` has no rpath
    // either, so `libmid.dylib` fails under it and under `tools/bin/b`.
    const THREE_CPUID: &str = "\
Load command 10
          cmd LC_MAIN
";

    #[test]
    fn a_reference_failing_under_several_chains_names_each_of_them() {
        let machos = [
            macho("probes/cpuid", THREE_CPUID),
            macho("tools/bin/b", TWO_EXE_B),
            macho("wine/bin/a", TWO_EXE_A),
            macho("wine/lib/deps/libfoo.dylib", TWO_EXE_FOO),
            macho("wine/lib/libmid.dylib", TWO_EXE_MID),
        ];
        let files = files(&[
            "probes/cpuid",
            "tools/bin/b",
            "wine/bin/a",
            "wine/lib/deps/libfoo.dylib",
            "wine/lib/libmid.dylib",
        ]);
        let resolves = |candidate: &Path| files.contains(candidate);
        let outcome = resolve_along_chains(
            &machos,
            &["wine/bin/a", "tools/bin/b", "probes/cpuid"],
            &resolves,
        );
        assert_eq!(
            outcome.chains,
            ["probes/cpuid", "tools/bin/b", "wine/bin/a"]
        );
        assert_eq!(
            describe(&outcome.violations),
            [(
                "wine/lib/libmid.dylib".to_owned(),
                "@rpath/libfoo.dylib".to_owned(),
                "@rpath reference resolves through no LC_RPATH of the binary or of the executable loading it; load chains probes/cpuid, tools/bin/b".to_owned(),
            )]
        );
    }

    /// The Fix Verifier's `two-exe-symlink` fixture: the declared loader
    /// `wine/bin/link-b` is a link to `tools/bin/b`, whose chain (no rpath,
    /// `@executable_path` at `tools/bin`) is the one dyld follows.
    #[test]
    fn a_declared_executable_that_is_a_symlink_contributes_its_targets_chain() {
        let dir = scratch("declared-symlink");
        let payload = dir.join("payload");
        fs::create_dir_all(payload.join("tools/bin")).expect("tools");
        fs::create_dir_all(payload.join("wine/bin")).expect("bin");
        fs::create_dir_all(payload.join("wine/lib/deps")).expect("lib");
        for file in [
            "tools/bin/b",
            "wine/bin/a",
            "wine/lib/deps/libfoo.dylib",
            "wine/lib/libmid.dylib",
        ] {
            fs::write(payload.join(file), b"stand-in").expect("file");
        }
        symlink("../../tools/bin/b", payload.join("wine/bin/link-b")).expect("link");
        symlink("../../tools/bin/missing", payload.join("wine/bin/dangling")).expect("link");
        symlink("/bin/sh", payload.join("wine/bin/outside")).expect("link");
        let payload = payload.canonicalize().expect("canonical payload");
        let machos = [
            macho("tools/bin/b", TWO_EXE_B),
            macho("wine/bin/a", TWO_EXE_A),
            macho("wine/lib/deps/libfoo.dylib", TWO_EXE_FOO),
            macho("wine/lib/libmid.dylib", TWO_EXE_MID),
        ];
        let declared = |paths: &[&str]| paths.iter().map(|p| (*p).to_owned()).collect::<Vec<_>>();

        let targets = declared_chain_targets(
            &payload,
            &declared(&["wine/bin/link-b", "wine/bin/a", "probes/cpuid"]),
            &machos,
        );
        assert_eq!(targets.targets, ["tools/bin/b", "wine/bin/a"]);
        assert_eq!(describe(&targets.violations), []);
        let resolves = |candidate: &Path| payload.join(candidate).is_file();
        let outcome = resolve_along_chains(&machos, &["tools/bin/b", "wine/bin/a"], &resolves);
        assert_eq!(outcome.chains, ["tools/bin/b", "wine/bin/a"]);
        assert_eq!(
            describe(&outcome.violations),
            [(
                "wine/lib/libmid.dylib".to_owned(),
                "@rpath/libfoo.dylib".to_owned(),
                "@rpath reference resolves through no LC_RPATH of the binary or of the executable loading it; load chain tools/bin/b".to_owned(),
            )]
        );

        let targets = declared_chain_targets(
            &payload,
            &declared(&[
                "wine/bin/dangling",
                "wine/bin/outside",
                "wine/lib/libmid.dylib",
                "wine/bin/a",
            ]),
            &machos,
        );
        assert_eq!(targets.targets, ["wine/bin/a"]);
        assert_eq!(
            describe(&targets.violations),
            [
                (
                    "BUNDLE.toml".to_owned(),
                    "wine/bin/dangling".to_owned(),
                    "declared executable is a dangling symlink".to_owned(),
                ),
                (
                    "BUNDLE.toml".to_owned(),
                    "wine/bin/outside".to_owned(),
                    "declared executable resolves outside the payload".to_owned(),
                ),
                (
                    "BUNDLE.toml".to_owned(),
                    "wine/lib/libmid.dylib".to_owned(),
                    "declared executable is a Mach-O without LC_MAIN or LC_UNIXTHREAD".to_owned(),
                ),
            ]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    // The Fix Verifier's `wrapper` fixture: Wine 11's `wine/bin/wine` is a
    // wrapper that re-execs `wine/lib/wine/x86_64-unix/wine`, the process
    // that dlopens the Unix modules. `BUNDLE.toml` declares the wrapper
    // only; the real loader is a chain because it lives under `wine/`.
    const WRAPPER: &str = "\
Load command 8
          cmd LC_MAIN
Load command 9
          cmd LC_RPATH
         path @loader_path/../lib (offset 12)
";
    const REAL_LOADER: &str = "\
Load command 8
          cmd LC_MAIN
Load command 9
          cmd LC_RPATH
         path @loader_path/../.. (offset 12)
";
    const RPATH_MODULE: &str = "\
Load command 0
          cmd LC_ID_DYLIB
         name @rpath/winefake.so (offset 24)
Load command 1
          cmd LC_LOAD_DYLIB
         name @rpath/libfoo.dylib (offset 24)
";
    const EXE_PATH_MODULE: &str = "\
Load command 0
          cmd LC_ID_DYLIB
         name @rpath/winefake.so (offset 24)
Load command 1
          cmd LC_LOAD_DYLIB
         name @executable_path/../lib/libfoo.dylib (offset 24)
";
    const REAL_LOADER_PATH: &str = "wine/lib/wine/x86_64-unix/wine";

    #[test]
    fn the_real_unix_loader_under_wine_is_a_chain() {
        let module = "wine/lib/wine/x86_64-unix/winefake.so";
        let files = files(&[
            "wine/bin/wine",
            REAL_LOADER_PATH,
            module,
            "wine/lib/libfoo.dylib",
        ]);
        let resolves = |candidate: &Path| files.contains(candidate);

        // The module has no rpath: `@rpath/libfoo.dylib` resolves through
        // the real loader's `@loader_path/../..` (and the wrapper's
        // `@loader_path/../lib`), so it passes under both chains.
        let machos = [
            macho("wine/bin/wine", WRAPPER),
            macho(REAL_LOADER_PATH, REAL_LOADER),
            macho(module, RPATH_MODULE),
        ];
        let outcome = resolve_along_chains(&machos, &["wine/bin/wine"], &resolves);
        assert_eq!(outcome.chains, ["wine/bin/wine", REAL_LOADER_PATH]);
        assert_eq!(describe(&outcome.violations), []);

        // Without the real loader's rpath the module resolves under the
        // wrapper alone, and the report names the chain that fails.
        let machos = [
            macho("wine/bin/wine", WRAPPER),
            macho(REAL_LOADER_PATH, THREE_CPUID),
            macho(module, RPATH_MODULE),
        ];
        let outcome = resolve_along_chains(&machos, &["wine/bin/wine"], &resolves);
        assert_eq!(
            describe(&outcome.violations),
            [(
                module.to_owned(),
                "@rpath/libfoo.dylib".to_owned(),
                format!(
                    "@rpath reference resolves through no LC_RPATH of the binary or of the executable loading it; load chain {REAL_LOADER_PATH}"
                ),
            )]
        );

        // `@executable_path/../lib/libfoo.dylib` is `wine/lib/libfoo.dylib`
        // from the wrapper and `wine/lib/wine/lib/libfoo.dylib` from the
        // real loader: the shape the verifier ran to a dlopen failure.
        let machos = [
            macho("wine/bin/wine", WRAPPER),
            macho(REAL_LOADER_PATH, REAL_LOADER),
            macho(module, EXE_PATH_MODULE),
        ];
        let outcome = resolve_along_chains(&machos, &["wine/bin/wine"], &resolves);
        assert_eq!(
            describe(&outcome.violations),
            [(
                module.to_owned(),
                "@executable_path/../lib/libfoo.dylib".to_owned(),
                format!(
                    "@executable_path reference does not resolve against the executable's directory; load chain {REAL_LOADER_PATH}"
                ),
            )]
        );
    }

    // The Fix Verifier's `realtree-nobin` fixture: GStreamer's
    // `gst-plugin-scanner` (undeclared, but a chain under `wine/`) reaches
    // the framework's `lib/` through `@executable_path/../../lib`; a
    // dlopen'd Wine module with no rpath names `@rpath/libfreetype.6.dylib`
    // from that directory. Under the declared loader `wine/bin/wine` dyld
    // tries `wine/lib` only.
    const LOADER: &str = "\
Load command 8
          cmd LC_MAIN
Load command 9
          cmd LC_RPATH
         path @loader_path/../lib (offset 12)
";
    const SCANNER: &str = "\
Load command 8
          cmd LC_MAIN
Load command 9
          cmd LC_LOAD_DYLIB
         name @rpath/libglib-2.0.0.dylib (offset 24)
Load command 10
          cmd LC_RPATH
         path @executable_path/../../lib (offset 12)
";
    const MODULE: &str = "\
Load command 0
          cmd LC_ID_DYLIB
         name @rpath/winefake.so (offset 24)
Load command 1
          cmd LC_LOAD_DYLIB
         name @rpath/libfreetype.6.dylib (offset 24)
";
    const GST_LIB: &str = "wine/lib/GStreamer.framework/Versions/1.0/lib";
    const GST_SCANNER: &str =
        "wine/lib/GStreamer.framework/Versions/1.0/libexec/gstreamer-1.0/gst-plugin-scanner";

    #[test]
    fn a_module_only_a_framework_tool_can_load_is_a_violation() {
        let module = "wine/lib/wine/x86_64-unix/winefake.so";
        let machos = [
            macho("wine/bin/wine", LOADER),
            macho(GST_SCANNER, SCANNER),
            macho(module, MODULE),
        ];
        let files = files(&[
            "wine/bin/wine",
            GST_SCANNER,
            module,
            &format!("{GST_LIB}/libfreetype.6.dylib"),
            &format!("{GST_LIB}/libglib-2.0.0.dylib"),
        ]);
        let resolves = |candidate: &Path| files.contains(candidate);

        let outcome = resolve_along_chains(&machos, &["wine/bin/wine"], &resolves);
        assert_eq!(outcome.chains, ["wine/bin/wine", GST_SCANNER]);
        assert_eq!(
            describe(&outcome.violations),
            [(
                module.to_owned(),
                "@rpath/libfreetype.6.dylib".to_owned(),
                "@rpath reference resolves through no LC_RPATH of the binary or of the executable loading it; load chain wine/bin/wine".to_owned(),
            )]
        );

        // The scanner's own reference resolves through its own rpath, and
        // the module stays a violation when no chain is present yet (the
        // tree before Wine is built).
        let machos = [macho(module, MODULE)];
        let outcome = resolve_along_chains(&machos, &["wine/bin/wineserver"], &resolves);
        assert_eq!(outcome.chains, Vec::<String>::new());
        assert_eq!(outcome.violations.len(), 1, "{:?}", outcome.violations);
        assert_eq!(outcome.violations[0].binary, module);
        assert!(
            !outcome.violations[0].reason.contains("load chain"),
            "{}",
            outcome.violations[0]
        );
    }

    #[test]
    fn a_system_rpath_on_the_chain_counts_what_it_leaves_to_dyld() {
        let swift_loader = "\
Load command 8
          cmd LC_MAIN
Load command 9
          cmd LC_RPATH
         path /usr/lib/swift (offset 12)
";
        let module = "wine/lib/wine/x86_64-unix/winefake.so";
        let machos = [macho("wine/bin/wine", swift_loader), macho(module, MODULE)];
        let files = files(&["wine/bin/wine", module]);
        let resolves = |candidate: &Path| files.contains(candidate);
        let outcome = resolve_along_chains(&machos, &["wine/bin/wine"], &resolves);
        assert_eq!(describe(&outcome.violations), []);
        assert_eq!(
            outcome.left_to_dyld,
            [(module.to_owned(), "@rpath/libfreetype.6.dylib".to_owned())]
        );
    }

    /// The closed payload's own trap: Homebrew's `libfreetype.6.dylib` beside
    /// the framework's, both `@rpath/libfreetype.6.dylib`; the framework's
    /// `libpng16.16.56.0.dylib` and a `libpng16.16.dylib` differ and pass.
    #[test]
    fn two_regular_files_with_one_install_name_are_a_violation() {
        let id = |name: &str| {
            format!(
                "Load command 0\n          cmd LC_ID_DYLIB\n         name @rpath/{name} (offset 24)\n"
            )
        };
        let machos = [
            macho("wine/bin/wine", "Load command 0\n          cmd LC_MAIN\n"),
            macho("wine/lib/libfreetype.6.dylib", &id("libfreetype.6.dylib")),
            macho(
                "wine/lib/GStreamer.framework/Versions/1.0/lib/libfreetype.6.dylib",
                &id("libfreetype.6.dylib"),
            ),
            macho("wine/lib/libpng16.16.dylib", &id("libpng16.16.dylib")),
            macho(
                "wine/lib/GStreamer.framework/Versions/1.0/lib/libpng16.16.56.0.dylib",
                &id("libpng16.16.56.0.dylib"),
            ),
        ];
        assert_eq!(
            describe(&super::duplicate_install_names(&machos)),
            [(
                "wine/lib/libfreetype.6.dylib, wine/lib/GStreamer.framework/Versions/1.0/lib/libfreetype.6.dylib".to_owned(),
                "@rpath/libfreetype.6.dylib".to_owned(),
                "several regular files carry this install name; dyld keeps one image per install name per process, so load order would pick the library".to_owned(),
            )]
        );
        assert_eq!(super::duplicate_install_names(&machos[..2]), []);
    }

    #[test]
    fn fat64_headers_count_as_macho() {
        let dir = scratch("fat64");
        let fat64 = dir.join("fat64");
        fs::write(&fat64, [0xca, 0xfe, 0xba, 0xbf, 0, 0, 0, 1]).expect("file");
        assert!(is_macho(&fat64).expect("read"));
        let swapped = dir.join("fat64-swapped");
        fs::write(&swapped, [0xbf, 0xba, 0xfe, 0xca, 0, 0, 0, 1]).expect("file");
        assert!(is_macho(&swapped).expect("read"));
        let text = dir.join("text");
        fs::write(&text, b"#!/bin/sh\n").expect("file");
        assert!(!is_macho(&text).expect("read"));
        let _ = fs::remove_dir_all(&dir);
    }
}
