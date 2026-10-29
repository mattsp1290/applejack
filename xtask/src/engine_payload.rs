//! `cargo xtask engine-payload`: fetches the pinned inputs of the Rosetta
//! engine payload, checks an assembled payload tree, and writes `BUNDLE.toml`.
//! The Wine build itself runs through the scripts under `build/`.

use std::fmt::Write as _;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use sha2::{Digest, Sha256};

use crate::bundle_toml::BundleSpec;
use crate::denylist::Denylist;
use crate::pins::{Pin, Pins};
use crate::self_contained::{Violation, check_otool_listing};

pub const USAGE: &str = "\
usage: cargo xtask engine-payload <mode>
  --fetch [id ...]                    download every pin (or the named ones) into build/work/ and verify SHA-256
  --check <payload-dir>               denylist scan plus self-containment check of every Mach-O
  --check-listing <file|->            denylist scan of an archive listing, one path per line
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
        fetch_pin(&work, pin)?;
    }
    Ok(())
}

fn fetch_pin(work: &Path, pin: &Pin) -> Result<(), String> {
    let dest = work.join(&pin.dest);
    if dest.is_file() {
        let actual = sha256_file(&dest)?;
        if actual == pin.sha256 {
            println!(
                "[xtask] fetch {}: present, hash ok ({})",
                pin.id,
                dest.display()
            );
            return Ok(());
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
    let status = Command::new("curl")
        .args(["-fsSL", "--retry", "3", "-o"])
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
    fs::rename(&partial, &dest).map_err(|error| format!("{}: {error}", dest.display()))?;
    println!("[xtask] fetch {}: verified {}", pin.id, pin.sha256);
    Ok(())
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

fn check(root: &Path, payload: &Path) -> Result<(), String> {
    let payload = payload
        .canonicalize()
        .map_err(|error| format!("{}: {error}", payload.display()))?;
    let denylist = load_denylist(root)?;
    let build_dir = root.join(WORK_DIR);
    let mut entries = Vec::new();
    walk(&payload, &payload, &mut entries)?;
    let paths: Vec<&str> = entries
        .iter()
        .map(|entry| entry.relative.as_str())
        .collect();
    let hits = denylist.scan(paths.iter().copied());
    for hit in &hits {
        eprintln!("[xtask] denylist: {hit}");
    }
    let mut violations: Vec<Violation> = Vec::new();
    let mut checked = 0_usize;
    for entry in entries.iter().filter(|entry| entry.is_file) {
        if !is_macho(&entry.absolute)? {
            continue;
        }
        checked += 1;
        let listing = otool(&entry.absolute)?;
        violations.extend(check_otool_listing(
            &entry.relative,
            &listing,
            &payload,
            &build_dir,
        ));
    }
    for violation in &violations {
        eprintln!("[xtask] self-contained: {violation}");
    }
    println!(
        "[xtask] check {}: {} paths, {} Mach-O files, {} denylist hits, {} link violations",
        payload.display(),
        entries.len(),
        checked,
        hits.len(),
        violations.len()
    );
    if hits.is_empty() && violations.is_empty() {
        Ok(())
    } else {
        Err("payload check failed".into())
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
    let hits = denylist.scan(text.lines().map(str::trim).filter(|line| !line.is_empty()));
    for hit in &hits {
        eprintln!("[xtask] denylist: {hit}");
    }
    if hits.is_empty() {
        println!(
            "[xtask] check-listing: {} paths clean",
            text.lines().count()
        );
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
        "[xtask] build scripts finished; payload assembly beyond the scripts is not implemented yet"
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
        entries.push(Entry {
            relative,
            absolute: child.clone(),
            is_file: file_type.is_file(),
        });
        // Symlinked directories stay unexpanded: their targets are listed on
        // their own, and the layout must not depend on where a link points.
        if file_type.is_dir() {
            walk(base, &child, entries)?;
        }
    }
    Ok(())
}

const MACHO_MAGICS: [[u8; 4]; 6] = [
    [0xfe, 0xed, 0xfa, 0xce],
    [0xce, 0xfa, 0xed, 0xfe],
    [0xfe, 0xed, 0xfa, 0xcf],
    [0xcf, 0xfa, 0xed, 0xfe],
    [0xca, 0xfe, 0xba, 0xbe],
    [0xbe, 0xba, 0xfe, 0xca],
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

fn otool(path: &Path) -> Result<String, String> {
    let output = Command::new("otool")
        .arg("-L")
        .arg(path)
        .output()
        .map_err(|error| format!("otool failed to start: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "otool -L {} failed ({}): {}",
            path.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
