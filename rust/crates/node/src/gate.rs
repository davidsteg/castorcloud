//! castor-ssh: the SSH gate for the provisioner's key on a node (TrueNAS
//! SCALE or macOS). authorized_keys runs it as a forced command, so the key
//! can do exactly this and nothing else:
//!
//!   docker system dial-stdio          the Docker CLI's ssh:// transport
//!   castor-zfs <args...>              tenant datasets, via sudo
//!   castor-snap <args...>             snapshots and send/receive for backups, via sudo
//!   castor-put <path> [644]           stdin -> ROOT/<path> (tenant config files, mode 600;
//!                                     644 for files unprivileged containers read)
//!   castor-rm tenants/<slug>[/<dir>]  removes a tenant's config directory (or part of it)
//!   castor-untar <templates|node|brand|ca>   replaces ROOT/files/<name> with a tar from stdin
//!   castor-info                       "info <os-version> <memory-mb> <cpus>"

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

use crate::conf::GateConf;
use crate::sys::{Input, Sys};
use crate::{Fail, Result, say, valid};

pub struct Ctx<'a> {
    pub sys: &'a mut dyn Sys,
    pub conf: GateConf,
    /// Where castor-zfs and castor-snap are installed (next to the gate).
    pub bin: PathBuf,
    pub stdin: &'a mut dyn Read,
    pub out: &'a mut dyn Write,
}

/// Splits SSH_ORIGINAL_COMMAND into words. Newer Docker CLIs quote each
/// word; real arguments never contain quotes or spaces, so quotes are
/// dropped and every word must be plain (`[A-Za-z0-9._/=-]`).
pub fn words(command: &str) -> Result<Vec<String>> {
    let plain: String = command.chars().filter(|c| *c != '\'' && *c != '"').collect();
    plain
        .split_ascii_whitespace()
        .map(|w| valid::gate_word(w).map(str::to_owned))
        .collect()
}

fn io_fail(what: &Path, e: io::Error) -> Fail {
    Fail::new(1, format!("castor-ssh: {}: {e}", what.display()))
}

pub fn main(c: &mut Ctx<'_>, command: &str) -> Result {
    let words = words(command)?;
    let w: Vec<&str> = words.iter().map(String::as_str).collect();
    let root = PathBuf::from(&c.conf.root);
    match w.first().copied().unwrap_or("") {
        "docker" => {
            if w != ["docker", "system", "dial-stdio"] {
                return Err(Fail::invalid("castor-ssh: docker: only dial-stdio"));
            }
            let Some((prog, pre)) = c.conf.docker.split_first() else {
                return Err(Fail::new(1, "castor-ssh: DOCKER is empty"));
            };
            let mut args: Vec<&str> = pre.iter().map(String::as_str).collect();
            args.extend(["system", "dial-stdio"]);
            let home = std::env::var("HOME").unwrap_or_default();
            let mut env = vec![("HOME", home.as_str())];
            if let Some(h) = &c.conf.docker_host {
                env.push(("DOCKER_HOST", h));
            }
            Err(c.sys.exec(prog, &args, &env))
        }
        tool @ ("castor-zfs" | "castor-snap") => {
            let helper = c.bin.join(tool);
            let helper = helper.to_string_lossy();
            let mut args = vec!["-n", helper.as_ref()];
            args.extend(&w[1..]);
            Err(c.sys.exec("sudo", &args, &[]))
        }
        "castor-put" => {
            let (path, mode) = match w[1..] {
                [p] => (p, 0o600),
                [p, "644"] => (p, 0o644),
                [_, _] => return Err(Fail::invalid("castor-ssh: bad mode")),
                _ => return Err(Fail::invalid("castor-ssh: usage: castor-put <path> [644]")),
            };
            put(c.stdin, &root.join(valid::rel_path(path)?), mode)
        }
        "castor-rm" => {
            let path = match w[1..] {
                [p] if p
                    .strip_prefix("tenants/")
                    .is_some_and(|s| s.starts_with(|ch: char| ch.is_ascii_lowercase() || ch.is_ascii_digit())) =>
                {
                    valid::rel_path(p)?
                }
                _ => return Err(Fail::invalid("castor-ssh: bad path")),
            };
            remove(&root.join(path))
        }
        "castor-untar" => {
            let name = match w[1..] {
                [n @ ("templates" | "node" | "brand" | "ca")] => n,
                _ => return Err(Fail::invalid("castor-ssh: bad name")),
            };
            untar(c.stdin, &root.join("files"), name)
        }
        "castor-info" => {
            let line = info(c.sys)?;
            say(c.out, &line)
        }
        other => Err(Fail::invalid(format!(
            "castor-ssh: not allowed: {}",
            if other.is_empty() { "<empty>" } else { other }
        ))),
    }
}

/// stdin -> `target`, written to a fresh temporary file next to it and
/// renamed into place, so a reader never sees half a file.
fn put(stdin: &mut dyn Read, target: &Path, mode: u32) -> Result {
    use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
    let dir = target.parent().ok_or_else(|| Fail::invalid("castor-ssh: bad path"))?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| io_fail(dir, e))?;
    let mut tmp = target.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    // create_new (O_EXCL) never follows a symlink planted at the temp path.
    match fs::remove_file(&tmp) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(io_fail(&tmp, e)),
        _ => {}
    }
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|e| io_fail(&tmp, e))?;
    io::copy(stdin, &mut f).map_err(|e| io_fail(&tmp, e))?;
    f.sync_all().map_err(|e| io_fail(&tmp, e))?;
    if mode != 0o600 {
        fs::set_permissions(&tmp, fs::Permissions::from_mode(mode)).map_err(|e| io_fail(&tmp, e))?;
    }
    fs::rename(&tmp, target).map_err(|e| io_fail(target, e))
}

/// rm -rf, without following symlinks.
fn remove(path: &Path) -> Result {
    let r = match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => Err(e),
        Ok(m) if m.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
    };
    r.map_err(|e| io_fail(path, e))
}

/// Replaces `files/<name>` with the tar on stdin. Only plain files and
/// directories, all inside `<name>`: links, devices and paths that would
/// leave the directory are refused, and nothing is replaced unless the whole
/// archive unpacked.
fn untar(stdin: &mut dyn Read, files: &Path, name: &str) -> Result {
    fs::create_dir_all(files).map_err(|e| io_fail(files, e))?;
    let new = files.join(format!("{name}.new"));
    let old = files.join(format!("{name}.old"));
    let dest = files.join(name);
    remove(&new)?;
    fs::create_dir(&new).map_err(|e| io_fail(&new, e))?;
    if let Err(e) = unpack(stdin, &new) {
        let _ = remove(&new);
        return Err(e);
    }
    remove(&old)?;
    if fs::symlink_metadata(&dest).is_ok() {
        fs::rename(&dest, &old).map_err(|e| io_fail(&dest, e))?;
    }
    fs::rename(&new, &dest).map_err(|e| io_fail(&dest, e))?;
    remove(&old)
}

fn unpack(stdin: &mut dyn Read, into: &Path) -> Result {
    let bad = |what: &str| Fail::invalid(format!("castor-ssh: tar: {what}"));
    let mut archive = tar::Archive::new(stdin);
    archive.set_preserve_permissions(false);
    archive.set_preserve_ownerships(false);
    archive.set_unpack_xattrs(false);
    archive.set_overwrite(true);
    for entry in archive.entries().map_err(|e| bad(&e.to_string()))? {
        let mut entry = entry.map_err(|e| bad(&e.to_string()))?;
        let kind = entry.header().entry_type();
        if !(kind.is_file() || kind.is_dir()) {
            return Err(bad("only files and directories are allowed"));
        }
        let path = entry.path().map_err(|e| bad(&e.to_string()))?.into_owned();
        if !path
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
        {
            return Err(bad(&format!("refused path {}", path.display())));
        }
        // unpack_in refuses anything that would end up outside `into`.
        if !entry.unpack_in(into).map_err(|e| bad(&e.to_string()))? {
            return Err(bad(&format!("refused path {}", path.display())));
        }
    }
    Ok(())
}

fn info(sys: &mut dyn Sys) -> Result<String> {
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
    if cfg!(target_os = "macos") {
        let version = sys.run("sw_vers", &["-productVersion"], Input::Null)?;
        let mem: u64 = sys
            .run("sysctl", &["-n", "hw.memsize"], Input::Null)?
            .trim()
            .parse()
            .unwrap_or(0);
        return Ok(format!("info macOS-{} {} {cpus}", version.trim(), mem / crate::MB));
    }
    // TrueNAS SCALE keeps its version in /etc/version.
    let version: String = match fs::read_to_string("/etc/version") {
        Ok(v) => v.chars().filter(|c| !c.is_whitespace()).collect(),
        Err(_) => sys
            .run("uname", &["-r"], Input::Null)
            .map(|v| v.trim().to_owned())
            .unwrap_or_default(),
    };
    let version = if version.is_empty() {
        "unknown".to_owned()
    } else {
        version
    };
    let meminfo = fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let kb: u64 = meminfo
        .lines()
        .find_map(|l| l.strip_prefix("MemTotal:"))
        .and_then(|v| v.split_whitespace().next())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    Ok(format!("info TrueNAS-{version} {} {cpus}", kb / 1024))
}
