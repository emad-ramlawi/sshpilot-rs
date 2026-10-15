//! Command lines for the system `ssh`.
//!
//! Everything about a host (address, user, port, keys, jump hosts…) comes
//! from `~/.ssh/config`: we only ever pass the alias. The terminal and the
//! file manager share one SSH connection per host (OpenSSH multiplexing), so
//! a password typed in the terminal also unlocks the file manager.
//!
//! The shared connection runs in its own background ssh (`ControlPersist`),
//! not inside the first tab: otherwise closing that tab would cut every other
//! tab to the same host. It stays up 10 minutes after its last tab closes,
//! so reopening a tab is instant (no new login), and the app ends it right
//! away when it quits (`exit_args`). Keep-alives keep idle sessions through
//! NAT/firewall timeouts and notice a dead link within about two minutes.

use std::path::PathBuf;

/// Private folder for the multiplexing sockets, or `None` without a safe
/// per-user runtime dir (sharing is then skipped rather than put in /tmp).
fn control_dir() -> Option<PathBuf> {
    crate::store::runtime_dir()
}

fn base_args() -> Vec<String> {
    let mut args = vec!["ssh".to_owned()];
    args.extend([
        "-o".into(),
        "ServerAliveInterval=30".into(),
        "-o".into(),
        "ServerAliveCountMax=4".into(),
    ]);
    if let Some(dir) = control_dir() {
        args.extend([
            "-o".into(),
            "ControlMaster=auto".into(),
            "-o".into(),
            format!("ControlPath={}/%C", dir.display()),
            "-o".into(),
            "ControlPersist=10m".into(),
        ]);
    }
    args
}

/// Interactive login shell.
pub fn terminal_args(alias: &str) -> Vec<String> {
    let mut args = base_args();
    // `--` ends option parsing: the alias can never be read as an option.
    args.extend(["--".into(), alias.to_owned()]);
    args
}

/// The SFTP subsystem over stdin/stdout. Never prompts: it reuses the
/// terminal's connection, or logs in with a key/agent.
pub fn sftp_args(alias: &str) -> Vec<String> {
    let mut args = base_args();
    args.extend([
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ConnectTimeout=15".into(),
        "-s".into(),
        "--".into(),
        alias.to_owned(),
        "sftp".into(),
    ]);
    args
}

/// Close the shared connection to `alias`, if there is one. Tabs still
/// using it are cut, so this is only for when the app quits.
pub fn exit_args(alias: &str) -> Option<Vec<String>> {
    control_dir()?;
    let mut args = base_args();
    args.extend(["-O".into(), "exit".into(), "--".into(), alias.to_owned()]);
    Some(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alias_always_follows_double_dash() {
        let t = terminal_args("web");
        assert_eq!(&t[t.len() - 2..], ["--", "web"]);
        let s = sftp_args("web");
        assert_eq!(&s[s.len() - 3..], ["--", "web", "sftp"]);
        if let Some(e) = exit_args("web") {
            assert_eq!(&e[e.len() - 4..], ["-O", "exit", "--", "web"]);
        }
    }
}
