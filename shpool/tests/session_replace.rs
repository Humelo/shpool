use std::{io::BufReader, os::unix::net::UnixStream};

use anyhow::{anyhow, Context};
use ntest::timeout;
use serde_json::Value;

mod support;

use crate::support::daemon::DaemonArgs;

fn next_event(reader: &mut BufReader<UnixStream>) -> anyhow::Result<Value> {
    let mut line = String::new();
    let n = std::io::BufRead::read_line(reader, &mut line).context("reading event line")?;
    if n == 0 {
        return Err(anyhow!("events socket closed unexpectedly"));
    }
    serde_json::from_str(&line).with_context(|| format!("parsing event JSON: {line:?}"))
}

/// Regression test for an in-place restart: a session is killed and a new
/// session is created under the same name while a client of the old session
/// is still unwinding. The old connection's cleanup must leave the new
/// session in the table, without publishing a removal for it.
#[test]
#[timeout(30000)]
fn old_connection_cleanup_keeps_replacement_session() -> anyhow::Result<()> {
    let mut daemon_proc = support::daemon::Proc::new("norc.toml", DaemonArgs::default())
        .context("starting daemon proc")?;
    let mut sub = daemon_proc.connect_events()?;

    // Hold the old connection after its bidi loop ends but before it
    // touches the session table.
    daemon_proc.send_event_command("pause-at daemon-bidi-stream-done")?;

    let mut old_attach =
        daemon_proc.attach("sh1", Default::default()).context("attaching to old session")?;
    let mut old_lines = old_attach.line_matcher()?;
    old_attach.run_cmd("echo old-session")?;
    old_lines.scan_until_re("old-session$")?;
    assert_eq!(next_event(&mut sub)?["type"], "session.created");
    assert_eq!(next_event(&mut sub)?["type"], "session.attached");

    let kill_out = daemon_proc.kill(vec![String::from("sh1")]).context("killing old session")?;
    assert!(kill_out.status.success(), "kill failed: {kill_out:?}");
    assert_eq!(next_event(&mut sub)?["type"], "session.removed");
    daemon_proc.await_event("paused-at daemon-bidi-stream-done")?;

    let mut new_attach =
        daemon_proc.attach("sh1", Default::default()).context("attaching to new session")?;
    let mut new_lines = new_attach.line_matcher()?;
    new_attach.run_cmd("echo new-session")?;
    new_lines.scan_until_re("new-session$")?;
    assert_eq!(next_event(&mut sub)?["type"], "session.created");
    assert_eq!(next_event(&mut sub)?["type"], "session.attached");

    daemon_proc.send_event_command("release daemon-bidi-stream-done")?;
    daemon_proc.await_event("daemon-handle-attach-exited-cleanup")?;

    let list_out = daemon_proc.list()?;
    assert!(list_out.status.success(), "list failed: {list_out:?}");
    let list_stdout = String::from_utf8_lossy(&list_out.stdout[..]);
    assert!(list_stdout.contains("sh1"), "replacement session missing from list: {list_stdout}");

    // A stale removal would surface here instead of the detach.
    let detach_out = daemon_proc.detach(vec![String::from("sh1")]).context("detaching")?;
    assert!(detach_out.status.success(), "detach failed: {detach_out:?}");
    assert_eq!(next_event(&mut sub)?["type"], "session.detached");

    Ok(())
}
