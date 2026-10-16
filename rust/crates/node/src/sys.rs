//! Running external commands (zfs, zpool, du, docker, sudo). Behind a trait
//! so the tools can be tested without a pool; the real implementation never
//! goes through a shell and clears the environment.

use std::io::Write as _;
use std::process::{Command, Stdio};

use crate::{Fail, Result};

/// What a command reads on stdin.
pub enum Input<'a> {
    /// Nothing (/dev/null).
    Null,
    /// These bytes (a key: written to a pipe, never to disk).
    Bytes(&'a [u8]),
    /// This process's stdin (a stream, e.g. `zfs receive`).
    Inherit,
}

pub trait Sys {
    /// Runs `prog args`, returning its exit code and stdout. stderr passes
    /// through to the caller.
    fn output(&mut self, prog: &str, args: &[&str], stdin: Input<'_>) -> Result<(i32, String)>;

    /// Replaces this process with `prog args` (stdio inherited), for streams
    /// like `zfs send` and `docker system dial-stdio`. Returns only on failure.
    fn exec(&mut self, prog: &str, args: &[&str], env: &[(&str, &str)]) -> Fail;

    /// Runs and fails like `set -e` would: with the command's own exit code
    /// (it has printed its error already).
    fn run(&mut self, prog: &str, args: &[&str], stdin: Input<'_>) -> Result<String> {
        match self.output(prog, args, stdin)? {
            (0, out) => Ok(out),
            (code, _) => Err(Fail::new(code, "")),
        }
    }

    /// Whether it exits 0, for probes like `zfs list <name>`.
    fn ok(&mut self, prog: &str, args: &[&str]) -> Result<bool> {
        Ok(self.output(prog, args, Input::Null)?.0 == 0)
    }
}

/// PATH for the root tools: OpenZFS on macOS and system directories only.
/// Never Homebrew's, which belongs to the admin user, not root.
pub const ROOT_PATH: &str = "/usr/local/zfs/bin:/usr/sbin:/usr/bin:/sbin:/bin";
/// PATH for the gate (unprivileged): where the Docker CLI lives on a Mac.
pub const GATE_PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";

pub struct Real {
    pub path: &'static str,
}

impl Real {
    fn command(&self, prog: &str, args: &[&str], env: &[(&str, &str)]) -> Command {
        let mut c = Command::new(prog);
        c.args(args).env_clear().env("PATH", self.path).env("LC_ALL", "C");
        for (k, v) in env {
            c.env(k, v);
        }
        c
    }
}

impl Sys for Real {
    fn output(&mut self, prog: &str, args: &[&str], stdin: Input<'_>) -> Result<(i32, String)> {
        let mut c = self.command(prog, args, &[]);
        c.stdout(Stdio::piped()).stderr(Stdio::inherit());
        c.stdin(match stdin {
            Input::Null => Stdio::null(),
            Input::Bytes(_) => Stdio::piped(),
            Input::Inherit => Stdio::inherit(),
        });
        let mut child = c.spawn().map_err(|e| Fail::new(127, format!("{prog}: {e}")))?;
        if let Input::Bytes(bytes) = stdin
            && let Some(mut pipe) = child.stdin.take()
        {
            // A command that exits without reading closes the pipe; its exit
            // status then tells what happened.
            let _ = pipe.write_all(bytes);
        }
        let out = child
            .wait_with_output()
            .map_err(|e| Fail::new(1, format!("{prog}: {e}")))?;
        let text = String::from_utf8(out.stdout).map_err(|_| Fail::new(1, format!("{prog}: output is not UTF-8")))?;
        // Killed by a signal: 128 + signal, as a shell reports it.
        let code = out
            .status
            .code()
            .unwrap_or_else(|| 128 + std::os::unix::process::ExitStatusExt::signal(&out.status).unwrap_or(0));
        Ok((code, text))
    }

    fn exec(&mut self, prog: &str, args: &[&str], env: &[(&str, &str)]) -> Fail {
        use std::os::unix::process::CommandExt as _;
        let err = self.command(prog, args, env).exec();
        Fail::new(127, format!("{prog}: {err}"))
    }
}
