//! A test binary of its own: it spawns children as fast as it can, which
//! would disturb tests that hold a `flock` and expect a close to release it.

use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

use applejack_engine::rosetta_wine::snapshot::PrefixLock;
use applejack_engine::{EngineId, EngineLayout, InstallLock};

/// How many of 3000 acquire-and-drop rounds are refused while two threads
/// spawn children. A child mid-spawn holds a copy of the lock's descriptor
/// until it execs, so a lock released only by closing it outlives the drop.
fn refused_while_children_spawn(acquire: impl Fn() -> bool + Sync) -> usize {
    let done = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let spawner = || {
            while !done.load(Ordering::Relaxed) {
                let _ = Command::new("/usr/bin/true").status();
            }
        };
        scope.spawn(spawner);
        scope.spawn(spawner);
        let refused = (0..3000).filter(|_| !acquire()).count();
        done.store(true, Ordering::Relaxed);
        refused
    })
}

#[test]
fn a_dropped_prefix_lock_is_free_at_once_while_children_spawn() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join(".snapshots");
    let prefix = dir.path().join("steam");
    let refused =
        refused_while_children_spawn(|| PrefixLock::acquire(&prefix, &root, "steam").is_ok());
    assert_eq!(refused, 0);
}

#[test]
fn a_dropped_install_lock_is_free_at_once_while_children_spawn() {
    let dir = tempfile::tempdir().unwrap();
    let layout = EngineLayout::new(dir.path());
    let refused = refused_while_children_spawn(|| {
        InstallLock::acquire(&layout, EngineId::RosettaWine).is_ok()
    });
    assert_eq!(refused, 0);
}
