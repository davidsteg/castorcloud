//! Running the Docker and SSH CLIs, as `apps/provisioner/src/node.ts` does.
//! Behind a trait so tests can record what would run.

use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zeroize::Zeroizing;

/// One command. `input` goes to stdin (keys) and is wiped afterwards.
pub struct Cmd {
    pub program: &'static str,
    pub args: Vec<String>,
    pub env: Vec<(&'static str, String)>,
    pub input: Option<Zeroizing<Vec<u8>>>,
}

impl Cmd {
    pub fn new(program: &'static str, args: Vec<String>) -> Self {
        Self {
            program,
            args,
            env: Vec::new(),
            input: None,
        }
    }

    /// The same short form node.ts puts in its errors.
    pub fn label(&self) -> String {
        let head: Vec<&str> = self.args.iter().take(4).map(String::as_str).collect();
        format!("{} {} …", self.program, head.join(" "))
    }
}

#[async_trait::async_trait]
pub trait Exec: Send + Sync {
    /// Stdout, or an error with the end of stderr.
    async fn run(&self, cmd: Cmd) -> Result<String, String>;

    /// Stdout as bytes, for binary output (a tar archive).
    async fn run_bytes(&self, cmd: Cmd) -> Result<Vec<u8>, String> {
        self.run(cmd).await.map(String::into_bytes)
    }

    /// `from`'s stdout into `to`'s stdin (a ZFS stream between two hosts,
    /// which never passes through memory whole). The bytes moved.
    async fn pipe(&self, from: Cmd, to: Cmd) -> Result<u64, String> {
        Err(format!(
            "{} | {}: piping is not supported here",
            from.label(),
            to.label()
        ))
    }
}

/// No command may outlive a job's lease.
const TIMEOUT: Duration = Duration::from_secs(20 * 60);
const ERR_TAIL: usize = 2000;
const MAX_OUTPUT: usize = 32 * 1024 * 1024;
/// The templates, brand assets and CA bundle sent to a node (node.ts allows the same).
const MAX_BYTES: usize = 256 * 1024 * 1024;
/// A full ZFS send can take hours; the job's lease is renewed meanwhile.
const PIPE_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

pub struct Real;

#[async_trait::async_trait]
impl Exec for Real {
    async fn run(&self, cmd: Cmd) -> Result<String, String> {
        let label = cmd.label();
        let out = run(cmd).await?;
        if out.len() > MAX_OUTPUT {
            return Err(format!("{label} failed: output too large"));
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    async fn run_bytes(&self, cmd: Cmd) -> Result<Vec<u8>, String> {
        let label = cmd.label();
        let out = run(cmd).await?;
        if out.len() > MAX_BYTES {
            return Err(format!("{label} failed: output too large"));
        }
        Ok(out)
    }

    async fn pipe(&self, from: Cmd, to: Cmd) -> Result<u64, String> {
        pipe(from, to).await
    }
}

fn spawn(cmd: &Cmd, stdin: Stdio) -> Result<tokio::process::Child, String> {
    tokio::process::Command::new(cmd.program)
        .args(&cmd.args)
        .envs(cmd.env.iter().map(|(k, v)| (*k, v.as_str())))
        .stdin(stdin)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("{} failed: {e}", cmd.label()))
}

/// The end of a child's stderr, read to the end so it never blocks on it.
fn stderr_tail(child: &mut tokio::process::Child) -> tokio::task::JoinHandle<String> {
    let stderr = child.stderr.take();
    tokio::spawn(async move {
        let Some(mut stderr) = stderr else {
            return String::new();
        };
        let mut kept: Vec<u8> = Vec::new();
        let mut buf = [0u8; 8192];
        while let Ok(n) = stderr.read(&mut buf).await {
            if n == 0 {
                break;
            }
            kept.extend_from_slice(&buf[..n]);
            if kept.len() > 2 * ERR_TAIL {
                kept.drain(..kept.len() - ERR_TAIL);
            }
        }
        tail(String::from_utf8_lossy(&kept).trim(), ERR_TAIL).to_owned()
    })
}

/// When either side fails, both are stopped: zfs receive rejects a half
/// stream, and a sender with no reader would only wait.
async fn pipe(from: Cmd, to: Cmd) -> Result<u64, String> {
    let (from_label, to_label) = (from.label(), to.label());
    let mut src = spawn(&from, Stdio::null())?;
    let mut dst = spawn(&to, Stdio::piped())?;
    let (src_err, dst_err) = (stderr_tail(&mut src), stderr_tail(&mut dst));
    let (Some(mut out), Some(mut inp)) = (src.stdout.take(), dst.stdin.take()) else {
        return Err(format!("{from_label} | {to_label}: no pipe"));
    };
    let copy = async move {
        let mut moved = 0u64;
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            let n = out.read(&mut buf).await.map_err(|e| (true, e))?;
            if n == 0 {
                break;
            }
            inp.write_all(&buf[..n]).await.map_err(|e| (false, e))?;
            moved += n as u64;
        }
        // Closing stdin tells the receiver the stream is complete.
        drop(inp);
        Ok::<u64, (bool, std::io::Error)>(moved)
    };
    let result = tokio::time::timeout(PIPE_TIMEOUT, async {
        let copied = copy.await;
        if copied.is_err() {
            let _ = src.start_kill();
            let _ = dst.start_kill();
        }
        let src_status = src.wait().await;
        let dst_status = dst.wait().await;
        (copied, src_status, dst_status)
    })
    .await;
    let (src_msg, dst_msg) = (src_err.await.unwrap_or_default(), dst_err.await.unwrap_or_default());
    let Ok((copied, src_status, dst_status)) = result else {
        return Err(format!(
            "{from_label} | {to_label} failed: timed out after {} h",
            PIPE_TIMEOUT.as_secs() / 3600
        ));
    };
    let failed = |label: &str, status: std::io::Result<std::process::ExitStatus>, msg: &str| match status {
        Ok(s) if s.success() => None,
        Ok(s) => Some(format!(
            "{label} failed: {}",
            if msg.is_empty() { s.to_string() } else { msg.to_owned() }
        )),
        Err(e) => Some(format!("{label} failed: {e}")),
    };
    let src_failed = failed(&from_label, src_status, &src_msg);
    let dst_failed = failed(&to_label, dst_status, &dst_msg);
    // A receiver that died first breaks the pipe, and the sender is then killed:
    // its error is the cause.
    let writing_broke = matches!(copied, Err((false, _)));
    let first = if writing_broke {
        dst_failed.or(src_failed)
    } else {
        src_failed.or(dst_failed)
    };
    if let Some(msg) = first {
        return Err(msg);
    }
    copied.map_err(|(_, e)| format!("{from_label} | {to_label} failed: {e}"))
}

/// Stdin is written while stdout is read, so neither side can block the
/// other; the timeout covers both.
async fn run(cmd: Cmd) -> Result<Vec<u8>, String> {
    let label = cmd.label();
    let mut child = tokio::process::Command::new(cmd.program)
        .args(&cmd.args)
        .envs(cmd.env.iter().map(|(k, v)| (*k, v.as_str())))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("{label} failed: {e}"))?;
    let stdin = child.stdin.take();
    let input = cmd.input;
    let feed = async move {
        if let (Some(mut stdin), Some(input)) = (stdin, input.as_ref()) {
            stdin.write_all(input).await?;
        }
        Ok::<(), std::io::Error>(())
    };
    let (fed, out) = tokio::time::timeout(TIMEOUT, async { tokio::join!(feed, child.wait_with_output()) })
        .await
        .map_err(|_| format!("{label} failed: timed out after {} min", TIMEOUT.as_secs() / 60))?;
    let out = out.map_err(|e| format!("{label} failed: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let msg = if stderr.trim().is_empty() {
            out.status.to_string()
        } else {
            tail(stderr.trim(), ERR_TAIL).to_owned()
        };
        return Err(format!("{label} failed: {msg}"));
    }
    fed.map_err(|e| format!("{label} failed: stdin: {e}"))?;
    Ok(out.stdout)
}

fn tail(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut start = s.len() - max;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[tokio::test]
    async fn runs_and_reports_failures() {
        let mut ok = Cmd::new("sh", vec!["-c".into(), "cat; echo out".into()]);
        ok.input = Some(Zeroizing::new(b"in ".to_vec()));
        assert_eq!(Real.run(ok).await.unwrap(), "in out\n");
        let bad = Cmd::new("sh", vec!["-c".into(), "echo nope >&2; exit 3".into()]);
        assert_eq!(
            Real.run(bad).await.unwrap_err(),
            "sh -c echo nope >&2; exit 3 … failed: nope"
        );
    }

    #[tokio::test]
    async fn large_input_and_output_at_once() {
        let mut echo = Cmd::new("cat", vec![]);
        echo.input = Some(Zeroizing::new(vec![b'x'; 4 * 1024 * 1024]));
        assert_eq!(Real.run_bytes(echo).await.unwrap().len(), 4 * 1024 * 1024);
    }

    #[tokio::test]
    async fn pipes_and_stops_both_sides() {
        let from = Cmd::new("sh", vec!["-c".into(), "head -c 3000000 /dev/zero".into()]);
        let to = Cmd::new("sh", vec!["-c".into(), "wc -c >/dev/null".into()]);
        assert_eq!(Real.pipe(from, to).await.unwrap(), 3_000_000);
        let from = Cmd::new("sh", vec!["-c".into(), "cat /dev/zero".into()]);
        let to = Cmd::new(
            "sh",
            vec!["-c".into(), "head -c 10 >/dev/null; echo full >&2; exit 4".into()],
        );
        assert_eq!(
            Real.pipe(from, to).await.unwrap_err(),
            "sh -c head -c 10 >/dev/null; echo full >&2; exit 4 … failed: full"
        );
        let from = Cmd::new("sh", vec!["-c".into(), "echo half; echo gone >&2; exit 2".into()]);
        let to = Cmd::new("sh", vec!["-c".into(), "cat >/dev/null".into()]);
        assert_eq!(
            Real.pipe(from, to).await.unwrap_err(),
            "sh -c echo half; echo gone >&2; exit 2 … failed: gone"
        );
    }

    #[test]
    fn tails_on_char_boundaries() {
        assert_eq!(tail("abc", 5), "abc");
        assert_eq!(tail("aéb", 2), "b");
    }
}
