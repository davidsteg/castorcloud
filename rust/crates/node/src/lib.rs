//! Node-side helpers, one binary for three tools that keep their old names
//! and their exact command lines and output (the provisioner parses it):
//!
//!   castor-ssh   the SSH forced command for the provisioner's key (gate)
//!   castor-zfs   per-tenant encrypted ZFS datasets (root, via sudo)
//!   castor-snap  backup snapshots and raw send/receive (root, via sudo)
//!
//! The tool is picked by the name the binary runs under (setup installs it
//! three times), or by the first argument of `castor-node <tool> ...`.
//!
//! Compared with the shell scripts it replaces: every argument is checked
//! before anything runs, external commands get a cleared environment and a
//! fixed PATH, castor.conf is parsed as data (never executed) and refused
//! unless root-owned and not writable by others, keys are checked and wiped
//! after use, tar uploads may not contain links or leave their directory,
//! and usage never follows symlinks a tenant container planted.

pub mod conf;
pub mod gate;
pub mod snap;
pub mod sys;
pub mod valid;
pub mod zfs;

use std::fmt;

/// How a tool run ends unsuccessfully: a message for stderr (may be empty
/// when the failing command already printed its own) and the exit code.
#[derive(Debug, PartialEq, Eq)]
pub struct Fail {
    pub code: i32,
    pub msg: String,
}

impl Fail {
    pub fn new(code: i32, msg: impl Into<String>) -> Self {
        Self { code, msg: msg.into() }
    }

    /// Exit 2: refused input. Nothing has been changed.
    pub fn invalid(msg: impl Into<String>) -> Self {
        Self::new(2, msg)
    }
}

impl fmt::Display for Fail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.msg)
    }
}

pub type Result<T = ()> = std::result::Result<T, Fail>;

/// Result of a tool that printed `line` and stops with `code` (e.g. "missing", 3).
pub fn exit_with(out: &mut dyn std::io::Write, line: &str, code: i32) -> Result {
    say(out, line)?;
    Err(Fail::new(code, ""))
}

pub fn say(out: &mut dyn std::io::Write, line: &str) -> Result {
    writeln!(out, "{line}").map_err(|e| Fail::new(1, format!("stdout: {e}")))
}

pub const MB: u64 = 1_048_576;
