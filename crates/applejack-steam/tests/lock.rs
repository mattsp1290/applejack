use std::fs::{self, File};
use std::os::unix::fs::PermissionsExt;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use applejack_steam::Error;
use applejack_steam::lock::{Holder, Lock, LockDir, Mode};

/// Above every platform's pid range, so `kill(pid, 0)` reports `ESRCH`.
const DEAD_PID: u32 = i32::MAX as u32;

#[test]
fn non_blocking_acquire_fails_with_the_holders_pid_and_command() {
    let dir = tempfile::tempdir().unwrap();
    let mut locks = LockDir::new(dir.path(), "install 0");
    let mut app = locks.app(0, Mode::NonBlocking).unwrap();
    let library = app.library(Mode::NonBlocking).unwrap();

    let holder: Holder =
        serde_json::from_str(fs::read_to_string(library.lock().path()).unwrap().trim()).unwrap();
    assert_eq!(holder.pid, std::process::id());
    assert_eq!(holder.command, "install 0");
    assert!(holder.acquired_at > 0);

    let (tx, rx) = mpsc::channel();
    let data_dir = dir.path().to_path_buf();
    thread::spawn(move || {
        let mut other = LockDir::new(&data_dir, "run 0");
        let app = other.app(0, Mode::NonBlocking).map(|_| ());
        let mut other = LockDir::new(&data_dir, "move");
        let library = other.library_exclusive(Mode::NonBlocking).map(|_| ());
        tx.send((app, library)).unwrap();
    });
    let (app_result, library_result) = rx.recv().unwrap();
    match app_result {
        Err(Error::LockHeld { name, holder }) => {
            assert_eq!(name, "app-0");
            let holder = holder.expect("holder is recorded and alive");
            assert_eq!(holder.pid, std::process::id());
            assert_eq!(holder.command, "install 0");
        }
        other => panic!("expected LockHeld, got {other:?}"),
    }
    match library_result {
        Err(Error::LockHeld { name, holder }) => {
            assert_eq!(name, "library");
            assert_eq!(
                holder.map(|holder| holder.command).as_deref(),
                Some("install 0")
            );
        }
        other => panic!("expected LockHeld, got {other:?}"),
    }
}

#[test]
fn blocking_acquire_succeeds_after_release() {
    let dir = tempfile::tempdir().unwrap();
    let mut locks = LockDir::new(dir.path(), "first");
    let first = locks.app(7, Mode::NonBlocking).unwrap();

    let (started, wait_started) = mpsc::channel();
    let (done, wait_done) = mpsc::channel();
    let data_dir = dir.path().to_path_buf();
    let waiter = thread::spawn(move || {
        started.send(()).unwrap();
        let mut other = LockDir::new(&data_dir, "second");
        let root = other.root().to_path_buf();
        let lock = other.app(7, Mode::Blocking).unwrap();
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
fn wait_mode_times_out_with_the_holder_then_succeeds_once_released() {
    let dir = tempfile::tempdir().unwrap();
    let mut locks = LockDir::new(dir.path(), "holder");
    let held = locks.app(3, Mode::NonBlocking).unwrap();

    let mut other = LockDir::new(dir.path(), "waiter");
    let started = Instant::now();
    match other.app(3, Mode::Wait(Duration::from_millis(150))) {
        Err(Error::LockHeld { name, holder }) => {
            assert_eq!(name, "app-3");
            assert_eq!(
                holder.map(|holder| holder.command).as_deref(),
                Some("holder")
            );
        }
        other => panic!("expected LockHeld, got {other:?}"),
    }
    assert!(started.elapsed() >= Duration::from_millis(150));

    let data_dir = dir.path().to_path_buf();
    let waiter = thread::spawn(move || {
        let mut other = LockDir::new(&data_dir, "waiter");
        other
            .app(3, Mode::Wait(Duration::from_secs(5)))
            .map(|lock| lock.lock().name().to_owned())
    });
    thread::sleep(Duration::from_millis(50));
    drop(held);
    assert_eq!(waiter.join().unwrap().unwrap(), "app-3");
}

#[test]
fn stale_holder_line_is_not_reported_after_release() {
    let dir = tempfile::tempdir().unwrap();
    let mut locks = LockDir::new(dir.path(), "old-dead-command");
    let app = locks.app(5, Mode::NonBlocking).unwrap();
    let path = app.lock().path().to_path_buf();
    assert_ne!(fs::read_to_string(&path).unwrap(), "");
    drop(app);
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "",
        "release must clear the holder line"
    );

    // A holder that has `flock`ed but not yet written its line, with a stale
    // line from a crashed process still in the file.
    let stale = Holder {
        pid: DEAD_PID,
        command: "old-dead-command".to_owned(),
        acquired_at: 1,
    };
    fs::write(
        &path,
        format!("{}\n", serde_json::to_string(&stale).unwrap()),
    )
    .unwrap();
    let raw = File::options().read(true).write(true).open(&path).unwrap();
    raw.try_lock().unwrap();

    let mut other = LockDir::new(dir.path(), "new");
    match other.app(5, Mode::NonBlocking) {
        Err(Error::LockHeld { name, holder }) => {
            assert_eq!(name, "app-5");
            assert_eq!(
                holder, None,
                "a dead pid must not be reported as the holder"
            );
        }
        other => panic!("expected LockHeld, got {other:?}"),
    }
    assert_eq!(Lock::holder(other.root(), "app-5"), None);
    assert!(
        !other
            .app(5, Mode::NonBlocking)
            .unwrap_err()
            .to_string()
            .contains("old-dead-command")
    );

    // An unparsable line is reported as unknown too.
    fs::write(&path, "not json\n").unwrap();
    assert!(matches!(
        other.app(5, Mode::NonBlocking),
        Err(Error::LockHeld { holder: None, .. })
    ));
    drop(raw);
    assert!(other.app(5, Mode::NonBlocking).is_ok());
}

#[test]
fn lock_files_are_private_and_live_under_data_locks() {
    let dir = tempfile::tempdir().unwrap();
    let mut locks = LockDir::new(dir.path(), "x");
    let mut app = locks.app(42, Mode::NonBlocking).unwrap();
    assert_eq!(app.appid(), 42);
    assert_eq!(app.lock().name(), "app-42");
    let app_path = app.lock().path().to_path_buf();
    assert_eq!(app_path, dir.path().join("locks/app-42.lock"));
    // The library lock borrows `app` mutably, so its path is captured first.
    let library = app.library(Mode::Blocking).unwrap();
    assert_eq!(library.lock().path(), dir.path().join("locks/library.lock"));
    for path in [app_path.as_path(), library.lock().path()] {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn wait_mode_with_duration_max_polls_without_a_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let mut locks = LockDir::new(dir.path(), "holder");
    // Unheld: `Instant + Duration::MAX` would panic before the first poll.
    drop(locks.app(4, Mode::Wait(Duration::MAX)).unwrap());

    let held = locks.app(4, Mode::NonBlocking).unwrap();
    let data_dir = dir.path().to_path_buf();
    let waiter = thread::spawn(move || {
        let mut other = LockDir::new(&data_dir, "waiter");
        other
            .app(4, Mode::Wait(Duration::MAX))
            .map(|lock| lock.lock().name().to_owned())
    });
    thread::sleep(Duration::from_millis(50));
    assert!(
        !waiter.is_finished(),
        "a Wait(MAX) acquire gave up on a held lock"
    );
    drop(held);
    assert_eq!(waiter.join().unwrap().unwrap(), "app-4");
}

#[test]
fn shared_library_lock_admits_two_app_holders_and_exclusive_waits_for_both() {
    let dir = tempfile::tempdir().unwrap();
    let mut first = LockDir::new(dir.path(), "install 1");
    let mut app_1 = first.app(1, Mode::NonBlocking).unwrap();
    let shared_1 = app_1.library(Mode::NonBlocking).unwrap();
    let mut second = LockDir::new(dir.path(), "install 2");
    let mut app_2 = second.app(2, Mode::NonBlocking).unwrap();
    let shared_2 = app_2
        .library(Mode::NonBlocking)
        .expect("a second app holder shares the library lock");
    assert_eq!(shared_1.lock().path(), shared_2.lock().path());

    let mut admin = LockDir::new(dir.path(), "move");
    match admin.library_exclusive(Mode::NonBlocking) {
        Err(Error::LockHeld { name, holder }) => {
            assert_eq!(name, "library");
            assert_eq!(
                holder.map(|holder| holder.command).as_deref(),
                Some("install 2"),
                "the last shared holder to write is reported"
            );
        }
        other => panic!("expected LockHeld, got {other:?}"),
    }

    let data_dir = dir.path().to_path_buf();
    let waiter = thread::spawn(move || {
        let mut admin = LockDir::new(&data_dir, "move");
        admin
            .library_exclusive(Mode::Wait(Duration::from_secs(5)))
            .map(|lock| lock.lock().name().to_owned())
    });
    thread::sleep(Duration::from_millis(50));
    drop(shared_1);
    thread::sleep(Duration::from_millis(50));
    assert!(
        !waiter.is_finished(),
        "the exclusive acquire returned while a shared holder remained"
    );
    drop(shared_2);
    assert_eq!(waiter.join().unwrap().unwrap(), "library");

    // Exclusive keeps new shared holders out, and names itself to them.
    let exclusive = admin.library_exclusive(Mode::NonBlocking).unwrap();
    match app_1.library(Mode::NonBlocking) {
        Err(Error::LockHeld { name, holder }) => {
            assert_eq!(name, "library");
            assert_eq!(holder.map(|holder| holder.command).as_deref(), Some("move"));
        }
        other => panic!("expected LockHeld, got {other:?}"),
    }
    drop(exclusive);
    assert!(app_1.library(Mode::NonBlocking).is_ok());
}

#[test]
fn removed_locks_dir_is_recreated_while_waiting() {
    let dir = tempfile::tempdir().unwrap();
    let mut locks = LockDir::new(dir.path(), "holder");
    let root = locks.root().to_path_buf();
    let held = locks.app(6, Mode::NonBlocking).unwrap();

    let data_dir = dir.path().to_path_buf();
    let waiter = thread::spawn(move || {
        let mut other = LockDir::new(&data_dir, "waiter");
        other.app(6, Mode::Wait(Duration::from_secs(5))).map(|_| ())
    });
    thread::sleep(Duration::from_millis(50));
    // Unlinking the held file releases nobody; the waiter's next poll must
    // recreate `locks/` and lock the new file once `held` is dropped.
    fs::remove_dir_all(&root).unwrap();
    drop(held);
    waiter.join().unwrap().unwrap();
    assert!(root.join("app-6.lock").is_file());
}
