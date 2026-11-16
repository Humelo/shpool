use std::{fs, io::Read as _, os::fd::OwnedFd, process, thread, time};

use anyhow::Context;
use nix::{
    pty::{openpty, Winsize},
    sys::signal::{self, Signal},
    unistd::Pid,
};
use ntest::timeout;

mod support;

use crate::support::daemon::{AttachArgs, DaemonArgs};

nix::ioctl_write_ptr_bad!(tiocswinsz, nix::libc::TIOCSWINSZ, nix::libc::winsize);

/// An `shpool attach` running on a pty whose size the test controls.
struct TtyAttach {
    child: process::Child,
    master: OwnedFd,
}

impl TtyAttach {
    fn spawn(
        daemon: &mut support::daemon::Proc,
        name: &str,
        rows: u16,
        cols: u16,
    ) -> anyhow::Result<Self> {
        let size = Winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
        let pty = openpty(Some(&size), None).context("opening pty")?;
        let child = daemon.attach_on_tty(name, pty.slave)?;
        // Drain the output so the attach process never blocks on a full pty.
        let mut master = fs::File::from(pty.master.try_clone().context("cloning pty master")?);
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = master.read(&mut buf) {
                if n == 0 {
                    break;
                }
            }
        });
        Ok(TtyAttach { child, master: pty.master })
    }

    /// Resizes the pty and tells the attach process, as a terminal would.
    fn resize(&mut self, rows: u16, cols: u16) -> anyhow::Result<()> {
        let size = nix::libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
        // Safety: size lives for the whole call and master is an open pty.
        unsafe {
            use std::os::fd::AsRawFd as _;
            tiocswinsz(self.master.as_raw_fd(), &size).context("resizing pty")?;
        }
        signal::kill(Pid::from_raw(self.child.id() as i32), Signal::SIGWINCH)
            .context("sending SIGWINCH")?;
        Ok(())
    }
}

/// Detaches `name` and waits for its attach process to exit. An attach that
/// already exited (the daemon dropped it) is not an error here: whether the
/// session still works is what the tests assert.
fn detach(
    daemon: &mut support::daemon::Proc,
    name: &str,
    child: &mut process::Child,
) -> anyhow::Result<()> {
    if child.try_wait().context("polling attach")?.is_none() {
        let out = daemon.detach(vec![String::from(name)]).context("detaching")?;
        if !out.status.success() {
            eprintln!("detach {name} failed: {out:?}");
            let _ = child.kill();
        }
    }
    child.wait().context("waiting for attach to exit")?;
    Ok(())
}

/// Waits until the daemon lists `name` as disconnected, so the next attach is
/// a reattach rather than a "busy" refusal.
fn wait_disconnected(daemon: &mut support::daemon::Proc, name: &str) -> anyhow::Result<()> {
    let deadline = time::Instant::now() + time::Duration::from_secs(10);
    loop {
        let out = daemon.list()?;
        let listed = String::from_utf8_lossy(&out.stdout[..]).to_string();
        if listed.lines().any(|l| l.starts_with(name) && l.contains("disconnected")) {
            return Ok(());
        }
        if time::Instant::now() > deadline {
            anyhow::bail!("{name} never became disconnected: {listed}");
        }
        thread::sleep(time::Duration::from_millis(50));
    }
}

/// A normal attach to `name` must still get the shell's output.
fn assert_session_usable(
    daemon: &mut support::daemon::Proc,
    name: &str,
    marker: &str,
) -> anyhow::Result<()> {
    let mut attach =
        daemon.attach(name, AttachArgs::default()).context("attaching after the 0x0 client")?;
    let mut lines = attach.line_matcher()?;
    attach.run_cmd(&format!("echo {marker}"))?;
    lines.scan_until_re(&format!("{marker}$")).context("session output after the 0x0 client")?;
    Ok(())
}

const SETTLE: time::Duration = time::Duration::from_millis(1500);

/// Reattach from a terminal that reports 0x0 (a terminal tab not laid out
/// yet, or a pty nobody sized). The restore spool used to be resized to zero
/// rows and panic, killing the session's output thread for good: every later
/// attach then failed with "sending on a disconnected channel".
fn zero_size_reattach(config: &str) -> anyhow::Result<()> {
    let mut daemon = support::daemon::Proc::new(config, DaemonArgs::default())
        .context("starting daemon proc")?;

    let mut first = daemon.attach("sh1", AttachArgs::default()).context("first attach")?;
    let mut first_lines = first.line_matcher()?;
    first.run_cmd("echo before-zero")?;
    first_lines.scan_until_re("before-zero$")?;
    detach(&mut daemon, "sh1", &mut first.proc)?;
    wait_disconnected(&mut daemon, "sh1")?;

    let mut zero = TtyAttach::spawn(&mut daemon, "sh1", 0, 0)?;
    thread::sleep(SETTLE);
    detach(&mut daemon, "sh1", &mut zero.child)?;
    wait_disconnected(&mut daemon, "sh1")?;

    assert_session_usable(&mut daemon, "sh1", "after-zero")
}

#[test]
#[timeout(30000)]
fn zero_size_reattach_vt100_screen() -> anyhow::Result<()> {
    zero_size_reattach("restore_screen.toml")
}

#[test]
#[timeout(30000)]
fn zero_size_reattach_vterm_screen() -> anyhow::Result<()> {
    zero_size_reattach("vterm_screen.toml")
}

/// A new session created from a 0x0 terminal gets a usable size.
#[test]
#[timeout(30000)]
fn zero_size_new_session() -> anyhow::Result<()> {
    let mut daemon = support::daemon::Proc::new("restore_screen.toml", DaemonArgs::default())
        .context("starting daemon proc")?;

    let mut zero = TtyAttach::spawn(&mut daemon, "sh1", 0, 0)?;
    thread::sleep(SETTLE);
    detach(&mut daemon, "sh1", &mut zero.child)?;
    wait_disconnected(&mut daemon, "sh1")?;

    assert_session_usable(&mut daemon, "sh1", "after-zero-create")
}

/// A terminal that shrinks to 0x0 while attached is ignored.
#[test]
#[timeout(30000)]
fn zero_size_resize() -> anyhow::Result<()> {
    let mut daemon = support::daemon::Proc::new("restore_screen.toml", DaemonArgs::default())
        .context("starting daemon proc")?;

    let mut tty = TtyAttach::spawn(&mut daemon, "sh1", 24, 80)?;
    thread::sleep(SETTLE);
    tty.resize(0, 0)?;
    thread::sleep(SETTLE);
    detach(&mut daemon, "sh1", &mut tty.child)?;
    wait_disconnected(&mut daemon, "sh1")?;

    assert_session_usable(&mut daemon, "sh1", "after-zero-resize")
}
