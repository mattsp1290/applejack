//! `steam login` on a real pty. The prompt puts the terminal in raw mode, and
//! a process that leaves without undoing it leaves the next secret typed
//! there on screen. Nothing here reaches Steam: every test ends at a prompt,
//! and the child runs with the network denied, so a broken harness cannot
//! either.

use std::env;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use rustix::process::{ioctl_tiocsctty, setsid};
use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
use rustix::termios::{LocalModes, tcgetattr};
use tempfile::TempDir;

const TERMINAL: &str = "APPLEJACK_TEST_TERMINAL";
const OUTCOME: &str = "APPLEJACK_TEST_OUTCOME";
const WAIT: Duration = Duration::from_secs(20);
const NO_NETWORK: &str = "(version 1)(allow default)(deny network*)";

/// The `applejack` child, the only reader of `/dev/tty`.
fn login_in_session(terminal: &Path) -> Child {
    let tty = File::options()
        .read(true)
        .write(true)
        .open(terminal)
        .unwrap();
    ioctl_tiocsctty(&tty).unwrap();
    Command::new("/usr/bin/sandbox-exec")
        .args(["-p", NO_NETWORK, env!("CARGO_BIN_EXE_applejack")])
        .args(["steam", "login"])
        .stdin(tty.try_clone().unwrap())
        .stdout(tty.try_clone().unwrap())
        .stderr(tty)
        .spawn()
        .unwrap()
}

/// Not a test of its own. It leads the session the pty belongs to, and
/// outlives the CLI: a session leader's exit revokes the terminal, so the
/// modes can be read only from in here. Then it stands in for a wrapper or a
/// script, the case with no shell to repair the terminal.
#[test]
fn session_leader() {
    let (Ok(terminal), Ok(outcome)) = (env::var(TERMINAL), env::var(OUTCOME)) else {
        return;
    };
    setsid().unwrap();
    let mut login = login_in_session(Path::new(&terminal));
    fs::write(format!("{outcome}.pid"), login.id().to_string()).unwrap();
    let status = login.wait().unwrap();
    let tty = File::open(&terminal).unwrap();
    let modes = tcgetattr(&tty).unwrap().local_modes;
    let on = |mode| u8::from(modes.contains(mode));
    fs::write(
        &outcome,
        format!(
            "{:?} echo={} icanon={} isig={}",
            status.code(),
            on(LocalModes::ECHO),
            on(LocalModes::ICANON),
            on(LocalModes::ISIG),
        ),
    )
    .unwrap();
}

/// The session leader and, through the pid it writes, the login it spawned.
/// A test that fails at a prompt would otherwise leave both there, with
/// launchd as their parent.
struct Leader {
    child: Child,
    pid_file: PathBuf,
}

impl Leader {
    fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait()
    }
}

impl Drop for Leader {
    fn drop(&mut self) {
        if self.child.try_wait().is_ok_and(|status| status.is_some()) {
            return;
        }
        if let Ok(pid) = fs::read_to_string(&self.pid_file) {
            let _ = Command::new("/bin/kill")
                .args(["-TERM", pid.trim()])
                .status();
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Pty {
    master: File,
    /// Held so the modes stay readable while the child runs.
    follower: File,
    output: Receiver<String>,
    seen: String,
}

impl Pty {
    fn open() -> (Self, PathBuf) {
        let master: OwnedFd = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).unwrap();
        grantpt(&master).unwrap();
        unlockpt(&master).unwrap();
        let path = PathBuf::from(ptsname(&master, Vec::new()).unwrap().into_string().unwrap());
        let follower = File::options().read(true).write(true).open(&path).unwrap();
        let master = File::from(master);
        let mut reader = master.try_clone().unwrap();
        let (lines, output) = mpsc::channel();
        thread::spawn(move || {
            let mut buffer = [0; 256];
            while let Ok(read) = reader.read(&mut buffer) {
                if read == 0
                    || lines
                        .send(String::from_utf8_lossy(&buffer[..read]).into_owned())
                        .is_err()
                {
                    break;
                }
            }
        });
        let pty = Self {
            master,
            follower,
            output,
            seen: String::new(),
        };
        (pty, path)
    }

    fn expect(&mut self, text: &str) {
        let deadline = Instant::now() + WAIT;
        while !self.seen.contains(text) {
            let left = deadline.saturating_duration_since(Instant::now());
            let chunk = self.output.recv_timeout(left);
            self.seen
                .push_str(&chunk.unwrap_or_else(|_| panic!("no `{text}` in {:?}", self.seen)));
        }
    }

    fn wait_for_raw_mode(&self) {
        let deadline = Instant::now() + WAIT;
        while tcgetattr(&self.follower)
            .unwrap()
            .local_modes
            .contains(LocalModes::ECHO)
        {
            assert!(Instant::now() < deadline, "the prompt never cleared echo");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        self.master.write_all(bytes).unwrap();
        self.master.flush().unwrap();
    }
}

fn wait_for_file(path: &Path) -> String {
    let deadline = Instant::now() + WAIT;
    loop {
        match fs::read_to_string(path) {
            Ok(text) if !text.is_empty() => return text,
            _ => {
                assert!(
                    Instant::now() < deadline,
                    "nothing at {}: the login did not stop",
                    path.display()
                );
                thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

/// At the account name prompt. Returns the outcome file.
fn at_the_name_prompt(dir: &TempDir) -> (Pty, Leader, PathBuf) {
    let (mut pty, path) = Pty::open();
    let outcome = dir.path().join("outcome");
    let child = Command::new(env::current_exe().unwrap())
        .args(["--exact", "session_leader", "--nocapture"])
        .env(TERMINAL, path)
        .env(OUTCOME, &outcome)
        .env("APPLEJACK_DATA_DIR", dir.path().join("data"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let leader = Leader {
        child,
        pid_file: dir.path().join("outcome.pid"),
    };
    pty.expect("account name");
    (pty, leader, outcome)
}

/// At the password prompt, in raw mode. Returns the outcome file.
fn at_the_prompt(dir: &TempDir) -> (Pty, Leader, PathBuf) {
    let (mut pty, leader, outcome) = at_the_name_prompt(dir);
    pty.send(b"fixture-user\n");
    pty.expect("password");
    pty.wait_for_raw_mode();
    (pty, leader, outcome)
}

#[test]
fn control_c_at_the_password_prompt_restores_the_terminal() {
    let dir = TempDir::new().unwrap();
    let (mut pty, mut leader, outcome) = at_the_prompt(&dir);
    pty.send(b"\x03");
    let text = wait_for_file(&outcome);
    leader.wait().unwrap();
    assert_eq!(text, "Some(130) echo=1 icanon=1 isig=1");
}

#[test]
fn termination_at_the_password_prompt_restores_the_terminal() {
    for (signal, code) in [("-TERM", 143), ("-HUP", 129), ("-QUIT", 131)] {
        let dir = TempDir::new().unwrap();
        let (_pty, mut leader, outcome) = at_the_prompt(&dir);
        let pid = wait_for_file(&dir.path().join("outcome.pid"));
        let killed = Command::new("/bin/kill")
            .args([signal, &pid])
            .status()
            .unwrap();
        assert!(killed.success());
        let text = wait_for_file(&outcome);
        leader.wait().unwrap();
        assert_eq!(text, format!("Some({code}) echo=1 icanon=1 isig=1"));
    }
}

/// Red if a blank name goes on to the password prompt, and from there to
/// Steam.
#[test]
fn a_blank_account_name_is_refused_at_the_prompt() {
    for line in [&b"\n"[..], b"   \n"] {
        let dir = TempDir::new().unwrap();
        let (mut pty, mut leader, outcome) = at_the_name_prompt(&dir);
        pty.send(line);
        pty.expect("account name is empty");
        let text = wait_for_file(&outcome);
        leader.wait().unwrap();
        assert!(text.starts_with("Some(1) "), "{text}");
        assert!(!pty.seen.contains("password"), "{:?}", pty.seen);
    }
}
