use std::fs;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use applejack_steam::Error;
use applejack_steam::lock::{Holder, Lock, LockDir, Mode};

#[test]
fn non_blocking_acquire_fails_with_the_holders_pid_and_command() {
    let dir = tempfile::tempdir().unwrap();
    let locks = LockDir::new(dir.path(), "install 0");
    let app = locks.app(0, Mode::NonBlocking).unwrap();
    let library = app.library(Mode::NonBlocking).unwrap();

    let holder: Holder =
        serde_json::from_str(fs::read_to_string(library.lock().path()).unwrap().trim()).unwrap();
    assert_eq!(holder.pid, std::process::id());
    assert_eq!(holder.command, "install 0");
    assert!(holder.acquired_at > 0);

    let (tx, rx) = mpsc::channel();
    let root = locks.root().to_path_buf();
    thread::spawn(move || {
        let other = LockDir::new(root.parent().unwrap(), "run 0");
        tx.send(other.app(0, Mode::NonBlocking).map(|_| ()))
            .unwrap();
    });
    match rx.recv().unwrap() {
        Err(Error::LockHeld { name, pid, command }) => {
            assert_eq!(name, "app-0");
            assert_eq!(pid, Some(std::process::id()));
            assert_eq!(command.as_deref(), Some("install 0"));
        }
        other => panic!("expected LockHeld, got {other:?}"),
    }
    assert!(matches!(
        Lock::acquire(locks.root(), "library", Mode::NonBlocking, "x"),
        Err(Error::LockHeld { .. })
    ));
}

#[test]
fn blocking_acquire_succeeds_after_release() {
    let dir = tempfile::tempdir().unwrap();
    let locks = LockDir::new(dir.path(), "first");
    let first = locks.app(7, Mode::NonBlocking).unwrap();

    let (started, wait_started) = mpsc::channel();
    let (done, wait_done) = mpsc::channel();
    let root = locks.root().to_path_buf();
    let waiter = thread::spawn(move || {
        started.send(()).unwrap();
        let lock = Lock::acquire(&root, "app-7", Mode::Blocking, "second").unwrap();
        done.send(Lock::holder(&root, "app-7").unwrap()).unwrap();
        drop(lock);
    });
    wait_started.recv().unwrap();
    assert!(
        wait_done.recv_timeout(Duration::from_millis(200)).is_err(),
        "blocking acquire returned while the lock was held"
    );
    drop(first);
    let holder = wait_done.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(holder.command, "second");
    waiter.join().unwrap();

    assert!(locks.app(7, Mode::NonBlocking).is_ok());
}

#[test]
fn lock_files_live_under_data_locks() {
    let dir = tempfile::tempdir().unwrap();
    let locks = LockDir::new(dir.path(), "x");
    let app = locks.app(42, Mode::NonBlocking).unwrap();
    assert_eq!(app.appid(), 42);
    assert_eq!(app.lock().name(), "app-42");
    assert_eq!(app.lock().path(), dir.path().join("locks/app-42.lock"));
    let library = app.library(Mode::Blocking).unwrap();
    assert_eq!(library.lock().path(), dir.path().join("locks/library.lock"));
}
