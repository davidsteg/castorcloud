//! castor-zfs: per-tenant encrypted ZFS datasets on a node, TrueNAS SCALE
//! (OpenZFS on Linux) or a Mac mini (OpenZFS on OS X). Runs as root via
//! sudo; the provisioner reaches it over SSH (see gate).
//!
//!   castor-zfs pool                                "pool <size-mb> <alloc-mb> <health>"
//!   castor-zfs create  <slug> <quota-mb> <dirs...> key on stdin; creates, unlocks, mounts (idempotent)
//!   castor-zfs open    <slug>                      key on stdin; unlocks + mounts (idempotent)
//!   castor-zfs quota   <slug> <quota-mb>           sets the quota (online, grows and shrinks)
//!   castor-zfs close   <slug>                      unmounts + locks
//!   castor-zfs destroy <slug>                      destroys the dataset and its snapshots
//!   castor-zfs status  <slug>                      "open <used-mb> <quota-mb> <quota-mb>" or "closed"/"missing"
//!   castor-zfs usage   <slug>                      "<dir> <used-mb>" per data directory
//!
//! Custom stacks (STACKS nodes only, docs/stacks.md): one encrypted dataset per
//! tenant whose quota is the booked stack storage, and one child per stack
//! (inherits the key) with the stack's own quota.
//!
//!   castor-zfs stacks-open    <slug> <quota-mb>    key on stdin; creates or unlocks + mounts, sets the quota
//!   castor-zfs stacks-close   <slug>               unmounts + locks
//!   castor-zfs stacks-destroy <slug>               destroys every stack's data
//!   castor-zfs stacks-status  <slug>               like status
//!   castor-zfs stack-create   <slug> <stack> <quota-mb> <dir>=<uid>.<gid>...
//!                                                  child dataset + data/<dir> owned by the (userns-mapped) container user
//!   castor-zfs stack-destroy  <slug> <stack>       destroys one stack's data
//!   castor-zfs stack-usage    <slug>               "<stack> <used-mb>" per stack
//!
//! Layout: dataset <DATASET_ROOT>/<slug> (aes-256-gcm, own key) mounted at
//! <MOUNT_ROOT>/<slug>; the stack's volumes are directories under data/.
//! The key is 64 hex characters on stdin; it goes to zfs through a pipe and
//! is wiped here afterwards, it never touches the disk.

use std::io::{Read, Write};
use std::path::Path;

use zeroize::Zeroizing;

use crate::conf::Layout;
use crate::sys::{Input, Sys};
use crate::{Fail, MB, Result, exit_with, say, valid};

pub struct Ctx<'a> {
    pub sys: &'a mut dyn Sys,
    pub layout: Layout,
    pub stdin: &'a mut dyn Read,
    pub out: &'a mut dyn Write,
    /// macOS: keep tenant data out of Finder.
    pub darwin: bool,
}

struct Dataset<'c, 'a> {
    c: &'c mut Ctx<'a>,
    ds: String,
    mnt: String,
}

impl Dataset<'_, '_> {
    fn exists(&mut self) -> Result<bool> {
        self.c.sys.ok("zfs", &["list", "-H", "-o", "name", &self.ds])
    }

    fn prop(&mut self, name: &str) -> Result<String> {
        Ok(self
            .c
            .sys
            .run("zfs", &["get", "-Hp", "-o", "value", name, &self.ds], Input::Null)?
            .trim()
            .to_owned())
    }

    fn unlocked(&mut self) -> Result<bool> {
        Ok(self.prop("keystatus")? == "available")
    }

    fn mounted(&mut self) -> Result<bool> {
        Ok(self.prop("mounted")? == "yes")
    }

    fn zfs(&mut self, args: &[&str]) -> Result {
        self.c.sys.run("zfs", args, Input::Null).map(drop)
    }

    fn open(&mut self, key: &[u8]) -> Result {
        if !self.unlocked()? {
            let ds = self.ds.clone();
            self.c.sys.run("zfs", &["load-key", &ds], Input::Bytes(key))?;
        }
        if !self.mounted()? {
            let ds = self.ds.clone();
            self.zfs(&["mount", &ds])?;
        }
        Ok(())
    }

    fn mb_prop(&mut self, name: &str) -> Result<u64> {
        let v = self.prop(name)?;
        v.parse::<u64>()
            .map(|b| b / MB)
            .map_err(|_| Fail::new(1, format!("zfs {name}: {v}")))
    }
}

/// Reads the dataset key from stdin: 64 hex characters, newline-terminated
/// for zfs. Bounded, checked, and wiped when dropped.
fn read_key(stdin: &mut dyn Read) -> Result<Zeroizing<Vec<u8>>> {
    let mut raw = Zeroizing::new(Vec::with_capacity(80));
    stdin
        .take(80)
        .read_to_end(&mut raw)
        .map_err(|e| Fail::new(1, format!("stdin: {e}")))?;
    let key = valid::hex_key(&raw)?;
    let mut line = Zeroizing::new(Vec::with_capacity(65));
    line.extend_from_slice(key);
    line.push(b'\n');
    Ok(line)
}

pub fn main(c: &mut Ctx<'_>, args: &[&str]) -> Result {
    let cmd = *args
        .first()
        .ok_or_else(|| Fail::invalid("usage: castor-zfs <command> ..."))?;
    if cmd == "pool" {
        let pool = c.layout.pool.clone();
        let line = c
            .sys
            .run("zpool", &["list", "-Hp", "-o", "size,alloc,health", &pool], Input::Null)?;
        let f: Vec<&str> = line.split_whitespace().collect();
        let [size, alloc, health] = f[..] else {
            return Err(Fail::new(1, format!("zpool list: {line}")));
        };
        let n = |v: &str| {
            v.parse::<u64>()
                .map(|b| b / MB)
                .map_err(|_| Fail::new(1, format!("zpool list: {line}")))
        };
        return say(c.out, &format!("pool {} {} {health}", n(size)?, n(alloc)?));
    }
    let slug = valid::slug(args.get(1).copied().unwrap_or(""))?;
    if cmd.starts_with("stacks-") || cmd.starts_with("stack-") {
        let Some((root, mount)) = c.layout.stacks.clone() else {
            return Err(Fail::new(
                1,
                "castor.conf: no place for stacks (set STACKS_DATASET_ROOT and STACKS_MOUNT_ROOT)",
            ));
        };
        let ds = format!("{root}/{slug}");
        let mnt = format!("{mount}/{slug}");
        return stacks(&mut Dataset { c, ds, mnt }, &root, cmd, &args[2..]);
    }
    let ds = format!("{}/{slug}", c.layout.dataset_root);
    let mnt = format!("{}/{slug}", c.layout.mount_root);
    let mut d = Dataset { c, ds, mnt };
    let arg = |i: usize, what: &str| {
        args.get(i)
            .copied()
            .ok_or_else(|| Fail::invalid(format!("missing {what}")))
    };

    match cmd {
        "create" => {
            let quota = valid::mb(arg(2, "quota-mb")?)?;
            let dirs = args[3..]
                .iter()
                .map(|s| valid::data_dir(s))
                .collect::<Result<Vec<_>>>()?;
            let key = read_key(d.c.stdin)?;
            if !d.exists()? {
                let quota = format!("quota={quota}M");
                let mountpoint = format!("mountpoint={}", d.mnt);
                let mut a = vec![
                    "create",
                    "-o",
                    "encryption=aes-256-gcm",
                    "-o",
                    "keyformat=hex",
                    "-o",
                    "keylocation=prompt",
                ];
                a.extend(["-o", &quota, "-o", &mountpoint]);
                if d.c.darwin {
                    a.extend(["-o", "com.apple.browse=off"]);
                }
                a.push(&d.ds);
                d.c.sys.run("zfs", &a, Input::Bytes(&key))?;
            }
            d.open(&key)?;
            for dir in dirs {
                let p = format!("{}/data/{dir}", d.mnt);
                std::fs::create_dir_all(&p).map_err(|e| Fail::new(1, format!("{p}: {e}")))?;
            }
            say(d.c.out, "open")
        }
        "open" => {
            if !d.exists()? {
                return exit_with(d.c.out, "missing", 3);
            }
            let key = read_key(d.c.stdin)?;
            d.open(&key)?;
            say(d.c.out, "open")
        }
        "quota" => {
            let quota = valid::mb(arg(2, "quota-mb")?)?;
            if !d.exists()? {
                return exit_with(d.c.out, "missing", 3);
            }
            // zfs refuses a quota below the space in use; the provisioner checks first.
            let ds = d.ds.clone();
            d.zfs(&["set", &format!("quota={quota}M"), &ds])?;
            say(d.c.out, &format!("quota {quota}"))
        }
        "close" => {
            let ds = d.ds.clone();
            if d.exists()? {
                if d.mounted()? {
                    d.zfs(&["unmount", &ds])?;
                }
                if d.unlocked()? {
                    d.zfs(&["unload-key", &ds])?;
                }
            }
            say(d.c.out, "closed")
        }
        "destroy" => {
            let ds = d.ds.clone();
            if d.exists()? {
                if d.mounted()? {
                    d.zfs(&["unmount", "-f", &ds])?;
                }
                d.zfs(&["destroy", "-r", &ds])?;
            }
            let _ = std::fs::remove_dir(&d.mnt);
            say(d.c.out, "destroyed")
        }
        "status" => {
            if !d.exists()? {
                say(d.c.out, "missing")
            } else if d.mounted()? {
                let used = d.mb_prop("used")?;
                let quota = d.mb_prop("quota")?;
                say(d.c.out, &format!("open {used} {quota} {quota}"))
            } else {
                say(d.c.out, "closed")
            }
        }
        "usage" => {
            if !(d.exists()? && d.mounted()?) {
                return say(d.c.out, "closed");
            }
            let mnt = d.mnt.clone();
            usage(d.c.sys, d.c.out, &Path::new(&mnt).join("data"))
        }
        _ => Err(Fail::invalid(format!("unknown command: {cmd}"))),
    }
}

/// Quota of a stacks dataset: 0 would mean "no quota" in ZFS.
fn quota(s: &str) -> Result<u64> {
    match valid::mb(s)? {
        0 => Err(Fail::invalid("quota must be above 0")),
        q => Ok(q),
    }
}

/// A stack name, as packages/stacks allows it: `[a-z][a-z0-9]*`.
fn stack_name(s: &str) -> Result<&str> {
    let ok = s.starts_with(|c: char| c.is_ascii_lowercase())
        && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    if !ok {
        return Err(Fail::invalid("invalid stack"));
    }
    Ok(s)
}

/// `<dir>=<uid>.<gid>`, never a host account: containers are mapped far
/// above (userns-remap), so anything below 65536 is refused.
fn owner(s: &str) -> Result<(&str, u32, u32)> {
    let bad = || Fail::invalid(format!("invalid owner: {s}"));
    let (dir, ids) = s.split_once('=').ok_or_else(bad)?;
    let dir = valid::data_dir(dir)?;
    let (uid, gid) = ids.split_once('.').ok_or_else(bad)?;
    let id = |v: &str| -> Result<u32> {
        if v.is_empty() || !v.bytes().all(|b| b.is_ascii_digit()) {
            return Err(bad());
        }
        v.parse().map_err(|_| bad())
    };
    let (uid, gid) = (id(uid)?, id(gid)?);
    if uid < 65536 || gid < 65536 {
        return Err(Fail::invalid(format!("owner below the userns range: {ids}")));
    }
    Ok((dir, uid, gid))
}

fn children(d: &mut Dataset<'_, '_>) -> Result<Vec<String>> {
    let ds = d.ds.clone();
    let list =
        d.c.sys
            .run("zfs", &["list", "-H", "-o", "name", "-r", "-d", "1", &ds], Input::Null)?;
    Ok(list
        .lines()
        .filter(|l| *l != ds && !l.is_empty())
        .map(str::to_owned)
        .collect())
}

fn child_mounted(d: &mut Dataset<'_, '_>, child: &str) -> Result<bool> {
    Ok(d.c
        .sys
        .run("zfs", &["get", "-Hp", "-o", "value", "mounted", child], Input::Null)?
        .trim()
        == "yes")
}

fn stacks(d: &mut Dataset<'_, '_>, root: &str, cmd: &str, rest: &[&str]) -> Result {
    let arg = |i: usize, what: &str| {
        rest.get(i)
            .copied()
            .ok_or_else(|| Fail::invalid(format!("missing {what}")))
    };
    match cmd {
        "stacks-open" => {
            let quota = quota(arg(0, "quota-mb")?)?;
            let key = read_key(d.c.stdin)?;
            if !d.c.sys.ok("zfs", &["list", "-H", "-o", "name", root])? {
                d.zfs(&["create", "-p", root])?;
            }
            if !d.exists()? {
                let q = format!("quota={quota}M");
                let mountpoint = format!("mountpoint={}", d.mnt);
                let mut a = vec![
                    "create",
                    "-o",
                    "encryption=aes-256-gcm",
                    "-o",
                    "keyformat=hex",
                    "-o",
                    "keylocation=prompt",
                ];
                a.extend(["-o", &q, "-o", &mountpoint]);
                if d.c.darwin {
                    a.extend(["-o", "com.apple.browse=off"]);
                }
                a.push(&d.ds);
                d.c.sys.run("zfs", &a, Input::Bytes(&key))?;
            }
            d.open(&key)?;
            // Children inherit the key, but not the mount after a load-key.
            for child in children(d)? {
                if !child_mounted(d, &child)? {
                    d.zfs(&["mount", &child])?;
                }
            }
            // zfs refuses a quota below what is used; the provisioner checks first.
            if d.prop("quota")? != (quota * MB).to_string() {
                let ds = d.ds.clone();
                d.zfs(&["set", &format!("quota={quota}M"), &ds])?;
            }
            say(d.c.out, "open")
        }
        "stacks-close" => {
            if d.exists()? {
                let mut all = children(d)?;
                all.sort();
                all.reverse();
                all.push(d.ds.clone());
                for child in all {
                    if child_mounted(d, &child)? {
                        d.zfs(&["unmount", &child])?;
                    }
                }
                if d.unlocked()? {
                    let ds = d.ds.clone();
                    d.zfs(&["unload-key", &ds])?;
                }
            }
            say(d.c.out, "closed")
        }
        "stacks-destroy" => {
            if d.exists()? {
                let ds = d.ds.clone();
                let _ = d.c.sys.output("zfs", &["unmount", "-f", &ds], Input::Null);
                d.zfs(&["destroy", "-r", &ds])?;
            }
            // Only empty mountpoint directories are left.
            remove_empty_dirs(Path::new(&d.mnt));
            say(d.c.out, "destroyed")
        }
        "stacks-status" => {
            if !d.exists()? {
                say(d.c.out, "missing")
            } else if d.mounted()? {
                let used = d.mb_prop("used")?;
                let quota = d.mb_prop("quota")?;
                say(d.c.out, &format!("open {used} {quota} {quota}"))
            } else {
                say(d.c.out, "closed")
            }
        }
        "stack-create" => {
            let stack = stack_name(arg(0, "stack")?)?;
            let quota = quota(arg(1, "quota-mb")?)?;
            let owners = rest[2..].iter().map(|s| owner(s)).collect::<Result<Vec<_>>>()?;
            if !(d.exists()? && d.mounted()?) {
                return Err(Fail::new(3, "stacks dataset not open"));
            }
            let child = format!("{}/{stack}", d.ds);
            let q = format!("quota={quota}M");
            if d.c.sys.ok("zfs", &["list", "-H", "-o", "name", &child])? {
                d.zfs(&["set", &q, &child])?;
            } else {
                // Mountpoint and key are inherited.
                d.zfs(&["create", "-o", &q, &child])?;
            }
            if !child_mounted(d, &child)? {
                d.zfs(&["mount", &child])?;
            }
            // Owned by root, not writable by the containers: only data/<dir> is.
            let base = Path::new(&d.mnt).join(stack);
            let data = base.join("data");
            mkdir(&data)?;
            for p in [&base, &data] {
                own(d.c.sys, p, 0, 0, 0o755)?;
            }
            for (dir, uid, gid) in owners {
                let p = data.join(dir);
                // Never follow a link a container may have put there.
                if std::fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_symlink()) {
                    return Err(Fail::invalid(format!("refusing symlink: {dir}")));
                }
                mkdir(&p)?;
                own(d.c.sys, &p, uid, gid, 0o750)?;
            }
            say(d.c.out, "open")
        }
        "stack-destroy" => {
            let stack = stack_name(arg(0, "stack")?)?;
            let child = format!("{}/{stack}", d.ds);
            if d.c.sys.ok("zfs", &["list", "-H", "-o", "name", &child])? {
                let _ = d.c.sys.output("zfs", &["unmount", "-f", &child], Input::Null);
                d.zfs(&["destroy", "-r", &child])?;
            }
            let _ = std::fs::remove_dir(Path::new(&d.mnt).join(stack));
            say(d.c.out, "destroyed")
        }
        "stack-usage" => {
            if !(d.exists()? && d.mounted()?) {
                return say(d.c.out, "closed");
            }
            let ds = d.ds.clone();
            let list = d.c.sys.run(
                "zfs",
                &["list", "-Hp", "-o", "name,used", "-r", "-d", "1", &ds],
                Input::Null,
            )?;
            for line in list.lines() {
                let mut f = line.split_whitespace();
                let (Some(name), Some(used)) = (f.next(), f.next()) else {
                    continue;
                };
                if name == ds {
                    continue;
                }
                let used: u64 = used.parse().map_err(|_| Fail::new(1, format!("zfs list: {line}")))?;
                let stack = name.rsplit('/').next().unwrap_or(name);
                say(d.c.out, &format!("{stack} {}", used / MB))?;
            }
            Ok(())
        }
        _ => Err(Fail::invalid(format!("unknown command: {cmd}"))),
    }
}

fn mkdir(p: &Path) -> Result {
    std::fs::create_dir_all(p).map_err(|e| Fail::new(1, format!("{}: {e}", p.display())))
}

/// chown -h, then the mode (the path is a real directory: checked or ours).
fn own(sys: &mut dyn Sys, p: &Path, uid: u32, gid: u32, mode: u32) -> Result {
    use std::os::unix::fs::PermissionsExt as _;
    let path = p.to_string_lossy();
    sys.run("chown", &["-h", &format!("{uid}:{gid}"), &path], Input::Null)?;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).map_err(|e| Fail::new(1, format!("{path}: {e}")))
}

/// Removes `p` and the empty directories below it; anything else stays.
fn remove_empty_dirs(p: &Path) {
    let Ok(meta) = std::fs::symlink_metadata(p) else { return };
    if !meta.is_dir() {
        return;
    }
    if let Ok(entries) = std::fs::read_dir(p) {
        for e in entries.flatten() {
            remove_empty_dirs(&e.path());
        }
    }
    let _ = std::fs::remove_dir(p);
}

/// "<dir> <used-mb>" for each data directory, by name. Only real directories:
/// a symlink a tenant container placed under data/ is skipped, and du stays
/// on the dataset's filesystem (-x), so usage never measures anything else.
pub fn usage(sys: &mut dyn Sys, out: &mut dyn Write, data: &Path) -> Result {
    let mut dirs = Vec::new();
    if let Ok(entries) = std::fs::read_dir(data) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.starts_with('.') && e.file_type().is_ok_and(|t| t.is_dir()) {
                dirs.push(name);
            }
        }
    }
    dirs.sort();
    for name in dirs {
        let p = data.join(&name);
        let p = p.to_string_lossy();
        let line = sys.run("du", &["-smx", &p], Input::Null)?;
        let mb = line.split_whitespace().next().unwrap_or("0");
        say(out, &format!("{name} {mb}"))?;
    }
    Ok(())
}
