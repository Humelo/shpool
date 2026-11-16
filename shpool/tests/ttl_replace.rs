use std::{thread, time};

use anyhow::Context;
use ntest::timeout;

mod support;

use crate::support::daemon::{AttachArgs, DaemonArgs};

const OLD_TTL: time::Duration = time::Duration::from_secs(2);

/// Kill a session that registered `OLD_TTL` with the reaper, create a
/// new session under the same name with `new_ttl`, then wait past the
/// old ttl. The old registration must leave the new session alone, and
/// the new session's own ttl must still reap it.
fn old_ttl_keeps_replacement_session(new_ttl: Option<time::Duration>) -> anyhow::Result<()> {
    let mut daemon_proc = support::daemon::Proc::new("norc.toml", DaemonArgs::default())
        .context("starting daemon proc")?;

    let old_registered_at = time::Instant::now();
    let mut old_attach = daemon_proc
        .attach("sh1", AttachArgs { ttl: Some(OLD_TTL), ..Default::default() })
        .context("attaching to old session")?;
    let mut old_lines = old_attach.line_matcher()?;
    old_attach.run_cmd("echo old-session")?;
    old_lines.scan_until_re("old-session$")?;

    let kill_out = daemon_proc.kill(vec![String::from("sh1")]).context("killing old session")?;
    assert!(kill_out.status.success(), "kill failed: {kill_out:?}");
    old_attach.proc.wait().context("waiting for old attach to exit")?;

    let new_registered_at = time::Instant::now();
    let mut new_attach = daemon_proc
        .attach("sh1", AttachArgs { ttl: new_ttl, ..Default::default() })
        .context("attaching to new session")?;
    let mut new_lines = new_attach.line_matcher()?;
    new_attach.run_cmd("echo new-session")?;
    new_lines.scan_until_re("new-session$")?;
    assert!(
        old_registered_at.elapsed() < OLD_TTL,
        "replacement took longer than the old ttl, the test proves nothing"
    );

    // Sleep past the old ttl so the reaper has woken up for it.
    thread::sleep(
        (OLD_TTL + time::Duration::from_secs(1)).saturating_sub(old_registered_at.elapsed()),
    );

    let list_out = daemon_proc.list()?;
    assert!(list_out.status.success(), "list failed: {list_out:?}");
    let list_stdout = String::from_utf8_lossy(&list_out.stdout[..]);
    assert!(list_stdout.contains("sh1"), "replacement session missing from list: {list_stdout}");

    // The replacement shell is still alive and attached.
    new_attach.run_cmd("echo still-here")?;
    new_lines.scan_until_re("still-here$")?;

    if let Some(new_ttl) = new_ttl {
        let reap_check_at = new_ttl + time::Duration::from_secs(1);
        thread::sleep(reap_check_at.saturating_sub(new_registered_at.elapsed()));
        let list_out = daemon_proc.list()?;
        assert!(list_out.status.success(), "list failed: {list_out:?}");
        let list_stdout = String::from_utf8_lossy(&list_out.stdout[..]);
        assert!(
            !list_stdout.contains("sh1"),
            "replacement session outlived its own ttl: {list_stdout}"
        );
    }

    Ok(())
}

#[test]
#[timeout(30000)]
fn old_ttl_keeps_replacement_without_ttl() -> anyhow::Result<()> {
    old_ttl_keeps_replacement_session(None)
}

#[test]
#[timeout(30000)]
fn old_ttl_keeps_replacement_with_later_ttl() -> anyhow::Result<()> {
    old_ttl_keeps_replacement_session(Some(time::Duration::from_secs(4)))
}
