//! `cargo xtask engine-payload`: fetches the pinned inputs of the Rosetta
//! engine payload, checks an assembled payload tree, and writes `BUNDLE.toml`.
//! The Wine build itself runs through the scripts under `build/`.

use std::fmt::Write as _;
use std::fs;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitCode};

use sha2::{Digest, Sha256};

use crate::bundle_toml::BundleSpec;
use crate::denylist::{Denylist, Hit};
use crate::pins::{Pin, Pins};
use crate::self_contained::{Violation, check_otool_listing, check_rpaths};

pub const USAGE: &str = "\
usage: cargo xtask engine-payload <mode>
  --fetch [id ...]                    download every pin (or the named ones) into build/work/, verify SHA-256, and denylist-scan the listing
  --check <payload-dir>               denylist scan plus self-containment check of every Mach-O and symlink
  --check-listing <file|->            denylist scan of an archive listing, one path per line (an empty listing fails)
  --write-bundle-toml <spec> <out>    render a BUNDLE.toml from a typed spec file
  --print-pin <id> [field]            print a pin (all fields as key=value, or one field)
  --build                             run the build scripts under build/ in order";

const PINS_FILE: &str = "build/pins.toml";
const DENYLIST_FILE: &str = "build/denylist.toml";
const WORK_DIR: &str = "build/work";

const BUILD_SCRIPTS: &[&str] = &[
    "build/wine/fetch.sh",
    "build/wine/build.sh",
    "build/dxmt/fetch.sh",
    "build/vkd3d/fetch.sh",
    "build/vulkan/fetch.sh",
    "build/gstreamer/fetch.sh",
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

fn load_denylist(root: &Path) -> Result<Denylist, String> {
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

/// A product archive's listing: its xar entries plus, for each component,
/// the paths in that component's `Bom`. `pkgutil --payload-files` lists only
/// the first component of a product archive, so the Boms are read instead;
/// they are a few kilobytes each and no Payload is expanded.
fn list_pkg(archive: &Path) -> Result<Vec<String>, String> {
    let entries = command_lines("xar", &["-tf"], archive)?;
    let boms: Vec<&str> = entries
        .iter()
        .map(String::as_str)
        .filter(|entry| entry.ends_with("/Bom"))
        .collect();
    if boms.is_empty() {
        return Err(format!(
            "{}: no component Bom in the package",
            archive.display()
        ));
    }
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
        let component = bom.strip_suffix("/Bom").unwrap_or(bom);
        for line in command_lines("lsbom", &["-s"], &scratch.join(bom))? {
            let relative = line.strip_prefix("./").unwrap_or(&line);
            if relative == "." {
                continue;
            }
            listing.push(format!("{component}/{relative}"));
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
    hits: Vec<Hit>,
    violations: Vec<Violation>,
}

fn check(root: &Path, payload: &Path) -> Result<(), String> {
    let report = inspect(root, payload)?;
    for hit in &report.hits {
        eprintln!("[xtask] denylist: {hit}");
    }
    for violation in &report.violations {
        eprintln!("[xtask] self-contained: {violation}");
    }
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
    let mut machos = 0_usize;
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
        machos += 1;
        let listing = otool(&entry.absolute, "-L")?;
        violations.extend(check_otool_listing(
            &entry.relative,
            &listing,
            &payload,
            &build_dir,
        ));
        let load_commands = otool(&entry.absolute, "-l")?;
        violations.extend(check_rpaths(
            &entry.relative,
            &load_commands,
            &payload,
            &build_dir,
        ));
    }
    Ok(Report {
        payload,
        paths: entries.len(),
        machos,
        hits,
        violations,
    })
}

/// Why a symlink would not survive the payload moving to another machine.
fn symlink_reason(payload: &Path, link: &Path, target: &Path) -> Option<&'static str> {
    if target.is_absolute() {
        return Some("absolute symlink target");
    }
    let resolved = lexical_normalize(&link.parent().unwrap_or(payload).join(target));
    if !resolved.starts_with(payload) {
        return Some("symlink target escapes the payload");
    }
    if fs::metadata(link).is_err() {
        return Some("dangling symlink");
    }
    None
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
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

fn write_bundle_toml(spec: &Path, out: &Path) -> Result<(), String> {
    let text = fs::read_to_string(spec).map_err(|error| format!("{}: {error}", spec.display()))?;
    let rendered = BundleSpec::parse(&text)
        .and_then(|spec| spec.to_toml())
        .map_err(|error| format!("{}: {error}", spec.display()))?;
    fs::write(out, rendered).map_err(|error| format!("{}: {error}", out.display()))?;
    println!("[xtask] wrote {}", out.display());
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
    println!(
        "[xtask] build scripts finished; the dylib closure (docs/engines/payload-build.md step 7) is not scripted yet"
    );
    Ok(())
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

struct Entry {
    relative: String,
    absolute: PathBuf,
    is_file: bool,
    /// Set for a symlink: where it points, as stored.
    link_target: Option<PathBuf>,
}

fn walk(base: &Path, dir: &Path, entries: &mut Vec<Entry>) -> Result<(), String> {
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

fn is_macho(path: &Path) -> Result<bool, String> {
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

fn otool(path: &Path, flag: &str) -> Result<String, String> {
    let output = Command::new("otool")
        .arg(flag)
        .arg(path)
        .output()
        .map_err(|error| format!("otool failed to start: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "otool {flag} {} failed ({}): {}",
            path.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
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
        ArchiveKind, archive_kind, check_listing, inspect, is_macho, list_archive, load_denylist,
        reject_denylisted,
    };

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
                ("wine/lib/escape", "symlink target escapes the payload"),
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
