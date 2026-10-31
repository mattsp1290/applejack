//! What makes the DXMT release load under the payload's Wine, without
//! replacing Wine's own D3D DLLs, so one bundle keeps every graphics backend.
//!
//! The release's DLLs carry Wine's builtin marker. Wine takes a marked DLL in
//! `system32` for a placeholder of the builtin and loads its own from
//! `wine/lib/wine` instead (`dlls/ntdll/unix/loader.c`, `set_dll_path`:
//! `dll_dir` precedes `WINEDLLPATH`), so d3d11, dxgi, and d3d10core lose the
//! marker and load as native from the prefix. `winemetal.dll` keeps it: it
//! needs `winemetal.so`, which only a builtin gets, so it has to be where Wine
//! looks for builtins.

use std::fs;
use std::path::Path;
use std::process::Command;

use crate::self_contained::Violation;

const ARCH_DIRS: [&str; 2] = ["x86_64-windows", "i386-windows"];
const NATIVE_STEMS: [&str; 3] = ["d3d11", "dxgi", "d3d10core"];
const WINEMETAL: &str = "winemetal.dll";

/// `builtin_signature` in `dlls/ntdll/loader.c`, compared with its NUL, right
/// after the 64-byte DOS header.
const BUILTIN_MARKER: &[u8] = b"Wine builtin DLL\0";
const MARKER_OFFSET: usize = 0x40;

const WINE_MODULES: &str = "wine/lib/wine";
const WINEMAC: &str = "wine/lib/wine/x86_64-unix/winemac.so";
/// As `nm` prints it, with the Mach-O underscore.
const MACDRV_FUNCTIONS: &str = "_macdrv_functions";

fn has_marker(image: &[u8]) -> bool {
    image
        .get(MARKER_OFFSET..)
        .is_some_and(|rest| rest.starts_with(BUILTIN_MARKER))
}

/// Clears the marker on the DXMT DLLs that must load as native and places
/// `winemetal.dll` among Wine's builtins, for both architectures. Rerunning it
/// changes nothing.
pub fn place(payload: &Path) -> Result<(), String> {
    for arch in ARCH_DIRS {
        let dxmt = payload.join("dxmt").join(arch);
        for stem in NATIVE_STEMS {
            let path = dxmt.join(format!("{stem}.dll"));
            let mut image =
                fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?;
            if !image.starts_with(b"MZ") {
                return Err(format!("{}: not a PE image", path.display()));
            }
            if has_marker(&image) {
                image[MARKER_OFFSET..MARKER_OFFSET + BUILTIN_MARKER.len()].fill(0);
                fs::write(&path, image).map_err(|error| format!("{}: {error}", path.display()))?;
            }
        }
        let modules = payload.join(WINE_MODULES).join(arch);
        fs::create_dir_all(&modules).map_err(|error| format!("{}: {error}", modules.display()))?;
        let from = dxmt.join(WINEMETAL);
        fs::copy(&from, modules.join(WINEMETAL))
            .map_err(|error| format!("{}: {error}", from.display()))?;
    }
    Ok(())
}

/// What `--check` reports. A tree with no `dxmt/` ships no such backend and a
/// tree with no built Wine has nothing to place into, as in a partial assembly.
pub fn violations(payload: &Path) -> Result<Vec<Violation>, String> {
    let mut violations = Vec::new();
    if !payload.join("dxmt").is_dir() {
        return Ok(violations);
    }
    for arch in ARCH_DIRS {
        for stem in NATIVE_STEMS {
            let relative = format!("dxmt/{arch}/{stem}.dll");
            let path = payload.join(&relative);
            if path.is_file()
                && has_marker(&fs::read(&path).map_err(|error| format!("{relative}: {error}"))?)
            {
                violations.push(Violation {
                    binary: relative,
                    reference: "Wine builtin DLL".to_owned(),
                    reason: "marked DLLs in system32 are placeholders, so Wine would load its own instead (xtask engine-payload --place-dxmt)".to_owned(),
                });
            }
        }
    }
    if !payload.join(WINE_MODULES).is_dir() {
        return Ok(violations);
    }
    for arch in ARCH_DIRS {
        let relative = format!("{WINE_MODULES}/{arch}/{WINEMETAL}");
        if !payload.join(&relative).is_file() {
            violations.push(Violation {
                binary: relative,
                reference: WINEMETAL.to_owned(),
                reason: "missing; it only reaches winemetal.so as a builtin (xtask engine-payload --place-dxmt)".to_owned(),
            });
        }
    }
    let winemac = payload.join(WINEMAC);
    if !winemac.is_file() || !exports(&winemac, MACDRV_FUNCTIONS)? {
        violations.push(Violation {
            binary: WINEMAC.to_owned(),
            reference: MACDRV_FUNCTIONS.to_owned(),
            reason:
                "no such export; DXMT cannot create a Metal view without it (build/wine/patches)"
                    .to_owned(),
        });
    }
    Ok(violations)
}

fn exports(path: &Path, symbol: &str) -> Result<bool, String> {
    let output = Command::new("nm")
        .arg("-gU")
        .arg(path)
        .output()
        .map_err(|error| format!("nm failed to start: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "nm -gU {} failed ({}): {}",
            path.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line.split_whitespace().last() == Some(symbol)))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    use super::{
        ARCH_DIRS, BUILTIN_MARKER, MARKER_OFFSET, NATIVE_STEMS, WINEMAC, WINEMETAL, place,
        violations,
    };

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("xtask-dxmt-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn marked_dll() -> Vec<u8> {
        let mut image = vec![0_u8; 0x100];
        image[..2].copy_from_slice(b"MZ");
        image[MARKER_OFFSET..MARKER_OFFSET + BUILTIN_MARKER.len()].copy_from_slice(BUILTIN_MARKER);
        image[0xf0] = 0xaa;
        image
    }

    /// A shared library defining `macdrv_functions` with this source line.
    fn winemac(payload: &Path, definition: &str) {
        let path = payload.join(WINEMAC);
        fs::create_dir_all(path.parent().expect("parent")).expect("unix dir");
        let mut cc = Command::new("cc")
            .args(["-shared", "-x", "c", "-", "-o"])
            .arg(&path)
            .stdin(Stdio::piped())
            .spawn()
            .expect("cc");
        std::io::Write::write_all(cc.stdin.as_mut().expect("stdin"), definition.as_bytes())
            .expect("source");
        assert!(cc.wait().expect("cc").success());
    }

    const EXPORTED: &str =
        "__attribute__((visibility(\"default\"))) const int macdrv_functions = 1;\n";
    const HIDDEN: &str = "__attribute__((visibility(\"hidden\"))) const int macdrv_functions = 1;\nconst int *keep(void) { return &macdrv_functions; }\n";

    /// A tree as `assemble.sh` copies it, before `place`.
    fn unplaced(payload: &Path) {
        for arch in ARCH_DIRS {
            let dxmt = payload.join("dxmt").join(arch);
            fs::create_dir_all(&dxmt).expect("dxmt");
            for stem in NATIVE_STEMS {
                fs::write(dxmt.join(format!("{stem}.dll")), marked_dll()).expect("dll");
            }
            fs::write(dxmt.join(WINEMETAL), marked_dll()).expect("winemetal");
        }
        winemac(payload, EXPORTED);
    }

    fn reported(payload: &Path) -> Vec<String> {
        violations(payload)
            .expect("violations")
            .into_iter()
            .map(|violation| violation.binary)
            .collect()
    }

    #[test]
    fn a_placed_tree_has_no_violations_and_placing_again_changes_nothing() {
        let payload = scratch("placed");
        unplaced(&payload);
        place(&payload).expect("place");
        assert_eq!(reported(&payload), Vec::<String>::new());
        let d3d11 = payload.join("dxmt/i386-windows/d3d11.dll");
        let once = fs::read(&d3d11).expect("d3d11");
        place(&payload).expect("place again");
        assert_eq!(fs::read(&d3d11).expect("d3d11"), once);

        let mut expected = marked_dll();
        expected[MARKER_OFFSET..MARKER_OFFSET + BUILTIN_MARKER.len()].fill(0);
        assert_eq!(once, expected, "only the marker's bytes change");
        let _ = fs::remove_dir_all(&payload);
    }

    #[test]
    fn winemetal_keeps_its_marker_in_both_places() {
        let payload = scratch("winemetal");
        unplaced(&payload);
        place(&payload).expect("place");
        for arch in ARCH_DIRS {
            for dir in ["dxmt", "wine/lib/wine"] {
                let path = payload.join(dir).join(arch).join(WINEMETAL);
                assert_eq!(fs::read(path).expect("winemetal"), marked_dll());
            }
        }
        let _ = fs::remove_dir_all(&payload);
    }

    #[test]
    fn every_marked_dll_is_a_violation() {
        let payload = scratch("marked");
        unplaced(&payload);
        place(&payload).expect("place");
        for arch in ARCH_DIRS {
            for stem in NATIVE_STEMS {
                let relative = format!("dxmt/{arch}/{stem}.dll");
                fs::write(payload.join(&relative), marked_dll()).expect("dll");
                assert_eq!(reported(&payload), std::slice::from_ref(&relative));
                place(&payload).expect("place");
            }
        }
        let _ = fs::remove_dir_all(&payload);
    }

    #[test]
    fn a_missing_winemetal_is_a_violation_for_either_architecture() {
        let payload = scratch("missing");
        unplaced(&payload);
        place(&payload).expect("place");
        for arch in ARCH_DIRS {
            let relative = format!("wine/lib/wine/{arch}/{WINEMETAL}");
            fs::remove_file(payload.join(&relative)).expect("remove");
            assert_eq!(reported(&payload), std::slice::from_ref(&relative));
            place(&payload).expect("place");
        }
        let _ = fs::remove_dir_all(&payload);
    }

    #[test]
    fn winemac_must_export_the_table() {
        let payload = scratch("winemac");
        unplaced(&payload);
        place(&payload).expect("place");
        for broken in [HIDDEN, "const int unrelated = 1;\n"] {
            winemac(&payload, broken);
            assert_eq!(reported(&payload), [WINEMAC]);
        }
        fs::remove_file(payload.join(WINEMAC)).expect("remove");
        assert_eq!(reported(&payload), [WINEMAC]);
        winemac(&payload, EXPORTED);
        assert_eq!(reported(&payload), Vec::<String>::new());
        let _ = fs::remove_dir_all(&payload);
    }

    #[test]
    fn a_tree_without_dxmt_or_without_wine_is_not_judged_on_what_it_lacks() {
        let payload = scratch("partial");
        assert_eq!(reported(&payload), Vec::<String>::new());
        fs::create_dir_all(payload.join("dxmt/x86_64-windows")).expect("dxmt");
        assert_eq!(reported(&payload), Vec::<String>::new());
        let _ = fs::remove_dir_all(&payload);
    }

    #[test]
    fn placing_refuses_a_missing_or_foreign_dll() {
        let payload = scratch("refuse");
        assert!(place(&payload).is_err());
        unplaced(&payload);
        fs::write(payload.join("dxmt/x86_64-windows/dxgi.dll"), b"not a dll").expect("dll");
        let error = place(&payload).unwrap_err();
        assert!(error.contains("not a PE image"), "{error}");
        let _ = fs::remove_dir_all(&payload);
    }
}
