//! A test binary of its own: it spawns children as fast as it can, which
//! would disturb tests that hold a `flock` and expect a close to release it.

use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

use applejack_engine::rosetta_wine::snapshot::PrefixLock;

/// A child mid-spawn on another thread holds a copy of the lock's descriptor
/// until it execs, so a lock released only by closing it outlives the drop.
#[test]
fn a_dropped_prefix_lock_is_free_at_once_while_children_spawn() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join(".snapshots");
    let done = AtomicBool::new(false);
    let refused = std::thread::scope(|scope| {
        let spawner = || {
            while !done.load(Ordering::Relaxed) {
                let _ = Command::new("/usr/bin/true").status();
            }
        };
        scope.spawn(spawner);
        scope.spawn(spawner);
        let refused = (0..3000)
            .filter(|_| PrefixLock::acquire(&root, "steam").is_err())
            .count();
        done.store(true, Ordering::Relaxed);
        refused
    });
    assert_eq!(refused, 0);
}
