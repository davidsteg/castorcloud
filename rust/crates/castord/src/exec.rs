//! Running the Docker and SSH CLIs, as `apps/provisioner/src/node.ts` does.
//! Behind a trait so tests can record what would run.

use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
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
}

/// No command may outlive a job's lease.
const TIMEOUT: Duration = Duration::from_secs(20 * 60);
const ERR_TAIL: usize = 2000;
const MAX_OUTPUT: usize = 32 * 1024 * 1024;

pub struct Real;

#[async_trait::async_trait]
impl Exec for Real {
    async fn run(&self, cmd: Cmd) -> Result<String, String> {
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
        if let Some(mut stdin) = child.stdin.take() {
            if let Some(input) = &cmd.input {
                stdin
                    .write_all(input)
                    .await
                    .map_err(|e| format!("{label} failed: stdin: {e}"))?;
            }
            drop(stdin);
        }
        let out = tokio::time::timeout(TIMEOUT, child.wait_with_output())
            .await
            .map_err(|_| format!("{label} failed: timed out after {} min", TIMEOUT.as_secs() / 60))?
            .map_err(|e| format!("{label} failed: {e}"))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let msg = if stderr.trim().is_empty() {
                out.status.to_string()
            } else {
                tail(stderr.trim(), ERR_TAIL).to_owned()
            };
            return Err(format!("{label} failed: {msg}"));
        }
        if out.stdout.len() > MAX_OUTPUT {
            return Err(format!("{label} failed: output too large"));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
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

    #[test]
    fn tails_on_char_boundaries() {
        assert_eq!(tail("abc", 5), "abc");
        assert_eq!(tail("aéb", 2), "b");
    }
}
