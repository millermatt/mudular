//! End-to-end smoke test: the real binary, driven through a real pty.
//!
//! Everything below `main` already has unit and integration coverage. What
//! only this exercises is the assembly — CLI args → config dir → profile →
//! compiled rules → session → terminal — plus the quit keybinding actually
//! gating the event loop, which cannot be observed without a terminal.
//!
//! Deliberately thin: one launch that proves the wiring, one that proves a
//! remapped key, and one that proves two characters run at once with a rule
//! crossing between them. Behaviour worth asserting in detail belongs in the
//! fast tests next to the code that implements it.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// ratatui writes cells in diff order, so a phrase with spaces can reach
/// the terminal split by cursor moves. A single unspaced token always
/// arrives contiguously, which makes it safe to search for.
const BANNER: &str = "MUDULAR-SMOKE-BANNER";

const CTRL_C: &[u8] = b"\x03";
const CTRL_Q: &[u8] = b"\x11";
const CTRL_S: &[u8] = b"\x13";
const DOWN: &[u8] = b"\x1b[B";
const ESC: &[u8] = b"\x1b";

#[test]
fn plays_from_a_profile_with_its_rules_and_quits() {
    let mud = FakeMud::start();
    let config = TempDir::new();
    write_config(config.path(), mud.port, None);

    let mut app = App::launch(&["tank", "--config-dir", config.path_str()]);

    app.wait_for(BANNER, "the server banner should reach the screen");

    // `k` is an alias defined in the shared module, using a variable the
    // profile overrides — if main.rs did not compile and pass the rules,
    // the server would see the raw "k" instead.
    app.type_line("k");
    mud.wait_for_command("kill dragon", "the profile's rules should apply");

    app.send(CTRL_C);
    app.expect_exit("the default quit key should quit");
}

/// The client has to stay reachable while it is busy drawing: a burst of
/// server output must not cost the player the one key they need most.
/// This is also the regression guard for `expect_exit` — waiting for the
/// process without draining its pty blocks it mid-`draw`, and the failure
/// looks exactly like a client that ignored the quit key.
#[test]
fn quits_on_the_key_even_while_output_is_flooding_in() {
    let mud = FakeMud::start();
    let config = TempDir::new();
    write_config(config.path(), mud.port, None);

    let mut app = App::launch(&["tank", "--config-dir", config.path_str()]);
    app.wait_for(BANNER, "the server banner should reach the screen");

    // Enough to overrun the pty's buffer several times over.
    for i in 0..20_000 {
        mud.send_line(&format!("flood line {i} ................................"));
    }
    // Deliberately not draining: the point is to let the buffer fill before
    // the key is sent, which is what makes this deterministic rather than a
    // race the suite loses once in a hundred runs.
    std::thread::sleep(Duration::from_millis(1500));

    app.send(CTRL_C);
    app.expect_exit("the quit key should work under a flood of output");
}

/// Completion changes what reaches the server, so the assembly worth
/// proving end to end is exactly that: what the MUD printed, through the
/// vocabulary, onto the input line, and back out as a different command
/// than the one that was typed.
#[test]
fn sends_the_completion_of_what_the_mud_printed() {
    let mud = FakeMud::start();
    let config = TempDir::new();
    write_config(config.path(), mud.port, None);

    let mut app = App::launch(&["tank", "--config-dir", config.path_str()]);
    app.wait_for(BANNER, "the server banner should reach the screen");

    mud.send_line("A bullywug is here.");
    app.wait_for("bullywug", "the room's occupant should reach the screen");

    app.type_line("look bull");

    mud.wait_for_command("look bullywug", "Enter should send the completed line");
}

#[test]
fn honours_a_remapped_quit_key() {
    let mud = FakeMud::start();
    let config = TempDir::new();
    write_config(config.path(), mud.port, Some("ctrl+q"));

    let mut app = App::launch(&["tank", "--config-dir", config.path_str()]);
    app.wait_for(BANNER, "the server banner should reach the screen");

    app.send(CTRL_C);
    assert!(
        app.still_running(),
        "ctrl+c must be inert once quit is remapped"
    );

    app.send(CTRL_Q);
    app.expect_exit("the remapped quit key should quit");
}

/// §10.2's acceptance bar, end to end: `/config` opens the in-client
/// profile editor, a new trigger is added entirely through the keyboard,
/// `Ctrl+S` saves it, and — without a reconnect — the live session reacts
/// to it and a backup of the pre-edit file appears on disk.
#[test]
fn adds_a_trigger_through_the_config_editor_and_saves_it_live() {
    let mud = FakeMud::start();
    let config = TempDir::new();
    write_config(config.path(), mud.port, None);

    let mut app = App::launch(&["tank", "--config-dir", config.path_str()]);
    app.wait_for(BANNER, "the server banner should reach the screen");

    app.type_line("/config");
    app.wait_for("BROWSE", "the editor should open");

    app.send(b"4"); // jump to the Triggers section
    app.send(b"a"); // add a new trigger, opening its form on `id`
    app.send(DOWN); // -> pattern
    app.send(b"\r"); // begin editing it
    app.send(b"^zap$");
    app.send(b"\r"); // commit the pattern
    app.send(DOWN); // -> when
    app.send(DOWN); // -> send
    app.send(b"\r"); // begin editing it
    app.send(b"say ouch");
    app.send(b"\r"); // commit the send list

    app.send(CTRL_S); // save
    app.wait_for(
        "backed up",
        "saving should report success and where the backup went",
    );

    // Two bare `Esc` presses sent back to back can be read by crossterm as
    // one Alt-modified key rather than two separate ones; a short pause
    // between them keeps the pty's raw bytes unambiguous.
    app.send(ESC); // form -> browse (the rule is no longer blank)
    std::thread::sleep(Duration::from_millis(100));
    app.send(ESC); // browse -> close (nothing left unsaved)

    app.wait_for("reloaded", "saving should reload the live session's rules");

    mud.send_line("zap");
    mud.wait_for_command("say ouch", "the trigger just added should already be live");

    let backups = std::fs::read_dir(config.path().join("backups/profiles/tank"))
        .expect("a backup directory should exist")
        .count();
    assert_eq!(
        backups, 1,
        "the pre-edit version should have been backed up exactly once"
    );

    app.send(CTRL_C);
    app.expect_exit("the default quit key should quit");
}

/// The scrollback line-cursor (§10.2/§11.5): `Alt+V` picks a line, `Enter`
/// opens the profile editor straight into a new trigger with that line's
/// text, regex-escaped, as its starting pattern.
#[test]
fn picks_a_scrollback_line_into_a_new_trigger_pattern() {
    let mud = FakeMud::start();
    let config = TempDir::new();
    write_config(config.path(), mud.port, None);

    let mut app = App::launch(&["tank", "--config-dir", config.path_str()]);
    app.wait_for(BANNER, "the server banner should reach the screen");

    mud.send_line("A rat scurries by.");
    app.wait_for("scurries", "the server's line should reach the pane");

    app.send(b"\x1bv"); // Alt+V: enter the line-cursor
    app.send(b"\r"); // pick the last line

    app.wait_for(
        "scurries",
        "the picked line's text should prefill the new trigger's pattern",
    );
    app.wait_for(
        "EDIT trigger",
        "picking a line should open straight into a trigger form",
    );

    app.send(CTRL_C);
    app.expect_exit("the default quit key should quit");
}

/// First run, §15's acceptance bar: launched with no profile, no `--host`,
/// and an empty config dir, the wizard should appear, and answering it
/// should both write a loadable profile and connect with it — no
/// hand-edited YAML anywhere in the path.
#[test]
fn creates_a_profile_from_the_wizard_and_connects() {
    let mud = FakeMud::start();
    let config = TempDir::new();

    let mut app = App::launch(&["--config-dir", config.path_str()]);
    // A single unspaced token: ratatui writes cells in diff order, so a
    // multi-word phrase can reach the terminal split across writes (see
    // the module doc comment on `BANNER`).
    app.wait_for("YAML", "the wizard should appear on first run");

    app.type_line("tank");
    app.type_line("127.0.0.1");
    app.type_line(&mud.port.to_string());
    app.type_line("n");

    app.wait_for(BANNER, "the wizard-built profile should connect");
    // A single unspaced token, like BANNER above: "press F1" is two cells
    // ratatui's diff renderer can (and does) reach the pty split across
    // separate writes, which raw substring matching on the byte stream
    // would miss even though it renders correctly — confirmed live via a
    // real terminal emulator, not just this raw-byte check.
    app.wait_for(
        "F1",
        "the newly connected session's first screen should hint at F1 (UX_REVIEW.md C)",
    );

    let saved = std::fs::read_to_string(config.path().join("profiles/tank.yaml"))
        .expect("the wizard should have saved a loadable profile");
    assert!(saved.contains("127.0.0.1"), "{saved}");

    app.send(CTRL_C);
    app.expect_exit("the default quit key should quit");
}

/// M7's acceptance criterion, end to end through the real binary: two
/// characters connected at once, and a tank trigger firing a heal in the
/// cleric's session (docs/ARCHITECTURE.md §14 M7).
#[test]
fn plays_two_characters_and_routes_a_trigger_across_them() {
    let tank_mud = FakeMud::start();
    let cleric_mud = FakeMud::start();
    let config = TempDir::new();
    write_two_profiles(config.path(), tank_mud.port, cleric_mud.port);

    let mut app = App::launch(&["tank", "cleric", "--config-dir", config.path_str()]);

    app.wait_for(
        BANNER,
        "the focused session's banner should reach the screen",
    );
    app.wait_for("cleric", "the second session should appear in the tab bar");

    // Only the tank's MUD says this; only the tank's rules react to it.
    tank_mud.send_line("You are badly wounded!");
    cleric_mud.wait_for_command(
        "cast 'major heal' Grunk",
        "the tank's trigger should drive the cleric's session",
    );

    // The heal must not also go to the character that raised it.
    let tank_saw = String::from_utf8_lossy(&tank_mud.received.lock().unwrap()).into_owned();
    assert!(
        !tank_saw.contains("major heal"),
        "a cross-session action must not reach its own server: {tank_saw:?}"
    );

    app.send(CTRL_C);
    app.expect_exit("the default quit key should quit");
}

/// `/connect` (§7.5, ARCH_REVIEW.md "Features that would break the
/// architecture"): a session already running learns about one added
/// after it, not just the reverse — the direction that needed the peer
/// mesh to stop being built once before the event loop and never
/// revisited. Only `tank` is named on the command line; `cleric` is
/// added live, and `tank`'s own trigger reads `cleric`'s peer state to
/// prove the mesh actually updated rather than just the tab bar.
#[test]
fn connects_a_character_into_a_running_instance() {
    let tank_mud = FakeMud::start();
    let cleric_mud = FakeMud::start();
    let config = TempDir::new();
    write_connect_profiles(config.path(), tank_mud.port, cleric_mud.port);

    let mut app = App::launch(&["tank", "--config-dir", config.path_str()]);
    app.wait_for(
        BANNER,
        "the first character's banner should reach the screen",
    );

    app.type_line("/connect cleric");
    app.wait_for(
        "cleric",
        "the newly connected character should join the tab bar",
    );

    // cleric connecting, its banner-matching trigger firing, and that
    // publishing across tank's newly-added receiver are all real async
    // work with no single event this test can wait on directly — retried
    // rather than a fixed sleep, so the first attempt racing that
    // (reading `${@cleric.hp}` before it exists yet) doesn't fail the
    // test outright.
    for _ in 0..10 {
        tank_mud.send_line("check cleric");
        std::thread::sleep(Duration::from_millis(200));
        let seen = String::from_utf8_lossy(&tank_mud.received.lock().unwrap()).into_owned();
        if seen.lines().any(|line| line.trim() == "say 42") {
            break;
        }
    }
    tank_mud.wait_for_command(
        "say 42",
        "tank's trigger should read cleric's peer state, added after tank was already running",
    );

    app.send(CTRL_C);
    app.expect_exit("the default quit key should quit");
}

/// One profile named on the command line (`tank`), one meant to be added
/// with `/connect` (`cleric`) — the second is not in `cli.profiles`.
fn write_connect_profiles(dir: &Path, tank_port: u16, cleric_port: u16) {
    std::fs::create_dir_all(dir.join("profiles")).unwrap();
    std::fs::write(
        dir.join("profiles/tank.yaml"),
        format!(
            "name: tank\nhost: 127.0.0.1\nport: {tank_port}\n\
             triggers:\n  - pattern: 'check cleric'\n    send: [\"say ${{@cleric.hp}}\"]\n"
        ),
    )
    .unwrap();
    std::fs::write(
        dir.join("profiles/cleric.yaml"),
        format!(
            "name: cleric\nhost: 127.0.0.1\nport: {cleric_port}\n\
             triggers:\n  - pattern: '{BANNER}'\n    set: {{hp: \"42\"}}\n"
        ),
    )
    .unwrap();
}

/// Two profiles on their own ports, the first with a cross-session rule.
fn write_two_profiles(dir: &Path, tank_port: u16, cleric_port: u16) {
    std::fs::create_dir_all(dir.join("profiles")).unwrap();
    std::fs::write(
        dir.join("profiles/tank.yaml"),
        format!(
            "name: tank\nhost: 127.0.0.1\nport: {tank_port}\n\
             triggers:\n  - pattern: 'badly wounded'\n    send_to:\n      \
             cleric: [\"cast 'major heal' Grunk\"]\n"
        ),
    )
    .unwrap();
    std::fs::write(
        dir.join("profiles/cleric.yaml"),
        format!("name: cleric\nhost: 127.0.0.1\nport: {cleric_port}\n"),
    )
    .unwrap();
}

/// A config dir with a shared module the profile pulls in and overrides.
fn write_config(dir: &Path, port: u16, quit: Option<&str>) {
    std::fs::create_dir_all(dir.join("profiles")).unwrap();
    std::fs::create_dir_all(dir.join("modules")).unwrap();

    if let Some(quit) = quit {
        std::fs::write(
            dir.join("mudular.yaml"),
            format!("keybinds:\n  quit: {quit}\n"),
        )
        .unwrap();
    }
    std::fs::write(
        dir.join("modules/combat.yaml"),
        "name: combat\nvariables:\n  target: kobold\naliases:\n  - id: kill\n    pattern: '^k$'\n    send: [\"kill ${target}\"]\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("profiles/tank.yaml"),
        format!(
            "name: tank\nhost: 127.0.0.1\nport: {port}\nmodules: [combat]\nvariables:\n  target: dragon\n"
        ),
    )
    .unwrap();
}

/// A minimal MUD: greets on connect, then records what the client sends.
struct FakeMud {
    port: u16,
    received: Arc<Mutex<Vec<u8>>>,
    /// Lines to push at the client once it has connected.
    outbound: std::sync::mpsc::Sender<String>,
}

impl FakeMud {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let received = Arc::new(Mutex::new(Vec::new()));

        let sink = Arc::clone(&received);
        let (outbound, pending) = std::sync::mpsc::channel::<String>();
        std::thread::spawn(move || {
            let Ok((mut sock, _)) = listener.accept() else {
                return;
            };
            let _ = sock.write_all(format!("{BANNER}\r\n").as_bytes());
            if let Ok(mut writer) = sock.try_clone() {
                std::thread::spawn(move || {
                    while let Ok(line) = pending.recv() {
                        let _ = writer.write_all(format!("{line}\r\n").as_bytes());
                    }
                });
            }
            let mut buf = [0u8; 1024];
            while let Ok(n) = sock.read(&mut buf) {
                if n == 0 {
                    break;
                }
                sink.lock()
                    .unwrap()
                    .extend_from_slice(&strip_telnet(&buf[..n]));
            }
        });

        FakeMud {
            port,
            received,
            outbound,
        }
    }

    /// Pushes a line to the connected client.
    fn send_line(&self, line: &str) {
        self.outbound.send(line.to_string()).unwrap();
    }

    fn wait_for_command(&self, expected: &str, why: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let seen = String::from_utf8_lossy(&self.received.lock().unwrap()).into_owned();
            if seen.lines().any(|line| line.trim() == expected) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let seen = String::from_utf8_lossy(&self.received.lock().unwrap()).into_owned();
        panic!("{why}: never received {expected:?}; got {seen:?}");
    }
}

/// Removes Telnet negotiation from client→server bytes, as a real server
/// would before treating input as a command.
fn strip_telnet(bytes: &[u8]) -> Vec<u8> {
    const IAC: u8 = 255;
    let mut out = Vec::new();
    let mut iter = bytes.iter().copied().peekable();
    while let Some(byte) = iter.next() {
        if byte != IAC {
            out.push(byte);
            continue;
        }
        match iter.next() {
            Some(IAC) => out.push(IAC),
            // Subnegotiation: skip through the terminating IAC SE.
            Some(250) => {
                while let Some(b) = iter.next() {
                    if b == IAC && iter.peek() == Some(&240) {
                        iter.next();
                        break;
                    }
                }
            }
            // WILL/WONT/DO/DONT carry one option byte.
            Some(251..=254) => {
                iter.next();
            }
            _ => {}
        }
    }
    out
}

/// The binary under test, running on the slave side of a pty.
struct App {
    child: Child,
    master: OwnedFd,
    output: String,
}

impl App {
    fn launch(args: &[&str]) -> Self {
        let (master, slave) = open_pty();

        // The child needs its own session with this pty as controlling
        // terminal, or crossterm cannot open /dev/tty for key events.
        let slave_fd = slave.as_raw_fd();
        let mut command = Command::new(env!("CARGO_BIN_EXE_mudular"));
        command
            .args(args)
            .env("TERM", "xterm-256color")
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave.try_clone().unwrap()));
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::ioctl(slave_fd, libc::TIOCSCTTY, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().expect("the binary should start");
        drop(slave);

        // Non-blocking, so reads can poll instead of hanging on a child
        // that has nothing to say.
        unsafe {
            let flags = libc::fcntl(master.as_raw_fd(), libc::F_GETFL);
            libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
        }

        App {
            child,
            master,
            output: String::new(),
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        let mut file = unsafe { std::fs::File::from_raw_fd(libc::dup(self.master.as_raw_fd())) };
        file.write_all(bytes).expect("writing to the pty");
        file.flush().ok();
    }

    fn type_line(&mut self, line: &str) {
        self.send(line.as_bytes());
        self.send(b"\r");
    }

    fn pump(&mut self) {
        let mut file = unsafe { std::fs::File::from_raw_fd(libc::dup(self.master.as_raw_fd())) };
        let mut buf = [0u8; 8192];
        while let Ok(n) = file.read(&mut buf) {
            if n == 0 {
                break;
            }
            self.output.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
    }

    fn wait_for(&mut self, needle: &str, why: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            self.pump();
            if self.output.contains(needle) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("{why}: never saw {needle:?} in the terminal output");
    }

    fn still_running(&mut self) -> bool {
        // Give a quit that should not happen time to happen anyway, and keep
        // draining while waiting — see `expect_exit`. A client wedged on a
        // full pty is "still running" for the wrong reason, which would let
        // this assertion pass over a client that had actually stopped
        // responding.
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            self.pump();
            std::thread::sleep(Duration::from_millis(50));
        }
        matches!(self.child.try_wait(), Ok(None))
    }

    fn expect_exit(&mut self, why: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            // Drain while waiting. The client is writing frames to this pty
            // the whole time; a test that stops reading fills the buffer and
            // blocks the client mid-`draw`, where it never polls for the key
            // it is being asked to react to. That reads as "the process was
            // still running" — a harness deadlock wearing a product bug's
            // clothes.
            self.pump();
            if let Ok(Some(status)) = self.child.try_wait() {
                assert!(status.success(), "{why}: exited with {status}");
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("{why}: the process was still running");
    }
}

impl Drop for App {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn open_pty() -> (OwnedFd, std::fs::File) {
    let mut master: libc::c_int = 0;
    let mut slave: libc::c_int = 0;
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(rc, 0, "openpty failed: {}", std::io::Error::last_os_error());

    // A window large enough that the banner is not wrapped or truncated.
    let size = libc::winsize {
        ws_row: 30,
        ws_col: 100,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe {
        libc::ioctl(slave, libc::TIOCSWINSZ, &size);
    }

    unsafe {
        (
            OwnedFd::from_raw_fd(master),
            std::fs::File::from_raw_fd(slave),
        )
    }
}

/// Scratch directory that cleans up after itself.
struct TempDir(PathBuf, String);

impl TempDir {
    fn new() -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("mudular-pty-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        let display = path.to_string_lossy().into_owned();
        TempDir(path, display)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn path_str(&self) -> &str {
        &self.1
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}
