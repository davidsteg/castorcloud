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
