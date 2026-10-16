//! castor-snap: the backup side of the tenant datasets on a node. Runs as
//! root via sudo, next to castor-zfs. It never sees a key: raw sends
//! (zfs send -w) carry the data still encrypted, and a received dataset
//! stays locked until castor-zfs opens it with the tenant's key.
//!
//!   castor-snap snapshot  <slug> <snap>           creates <ds>@<snap>
//!   castor-snap send      <slug> <snap> [<base>]  raw stream of @<snap> on stdout,
//!                                                 incremental from bookmark #<base>
//!   castor-snap sent      <slug> <snap>           bookmarks @<snap> as #<snap>, then drops
//!                                                 every backup snapshot and older bookmark
//!   castor-snap drop      <slug> <snap>           destroys @<snap> (a failed backup)
//!   castor-snap bookmarks <slug>                  backup bookmarks, oldest first
//!   castor-snap set-aside <slug> <tag>            locks <ds> and renames it to <ds>.<tag>
//!   castor-snap receive   <slug> <quota-mb>       raw stream on stdin -> <ds> (must not
//!                                                 exist), left locked and unmounted
//!   castor-snap put-back  <slug> <tag>            undoes set-aside (a failed restore)
//!   castor-snap discard   <slug> <tag>            destroys <ds>.<tag> after a restore
//!
//! Snapshots only live while they're sent: afterwards a bookmark (no space)
//! is the base of the next incremental, so backups don't eat the tenant's
//! quota. Backup snapshots and bookmarks are named cb-<UTC time>.

use std::io::Write;

use crate::conf::Layout;
use crate::sys::{Input, Sys};
use crate::{Fail, Result, exit_with, say, valid};

/// Where set-aside remembers a dataset's own mountpoint.
const MPROP: &str = "ch.castorcloud:mountpoint";

pub struct Ctx<'a> {
    pub sys: &'a mut dyn Sys,
    pub layout: Layout,
    pub out: &'a mut dyn Write,
}

impl Ctx<'_> {
    fn exists(&mut self, name: &str) -> Result<bool> {
        self.sys.ok("zfs", &["list", "-H", "-o", "name", name])
    }

    fn zfs(&mut self, args: &[&str]) -> Result<String> {
        self.sys.run("zfs", args, Input::Null)
    }

    fn get(&mut self, field: &str, prop: &str, ds: &str) -> Result<String> {
        Ok(self.zfs(&["get", "-H", "-o", field, prop, ds])?.trim().to_owned())
    }

    /// Names of `ds`'s direct snapshots or bookmarks.
    fn children(&mut self, kind: &str, ds: &str, sorted: bool) -> Result<Vec<String>> {
        let mut a = vec!["list", "-H", "-o", "name", "-t", kind];
        if sorted {
            a.extend(["-s", "createtxg"]);
        }
        a.extend(["-d", "1", ds]);
        Ok(self.zfs(&a)?.lines().map(str::to_owned).collect())
    }
}

pub fn main(c: &mut Ctx<'_>, args: &[&str]) -> Result {
    let cmd = *args
        .first()
        .ok_or_else(|| Fail::invalid("usage: castor-snap <command> <slug> ..."))?;
    let slug = valid::slug(args.get(1).copied().unwrap_or(""))?;
    let parent = c.layout.dataset_root.clone();
    let ds = format!("{parent}/{slug}");
    let arg = |i: usize, what: &str| {
        args.get(i)
            .copied()
            .ok_or_else(|| Fail::invalid(format!("missing {what}")))
    };

    match cmd {
        "snapshot" => {
            let snap = valid::snap_name(arg(2, "snap")?)?;
            need(c, &ds)?;
            c.zfs(&["snapshot", &format!("{ds}@{snap}")])?;
            say(c.out, &format!("snapshot {snap}"))
        }
        "send" => {
            let snap = valid::snap_name(arg(2, "snap")?)?;
            let base = args
                .get(3)
                .copied()
                .filter(|b| !b.is_empty())
                .map(valid::snap_name)
                .transpose()?;
            need(c, &ds)?;
            let at = format!("{ds}@{snap}");
            Err(match base {
                Some(base) => c
                    .sys
                    .exec("zfs", &["send", "-w", "-i", &format!("{ds}#{base}"), &at], &[]),
                None => c.sys.exec("zfs", &["send", "-w", &at], &[]),
            })
        }
        "sent" => {
            let snap = valid::snap_name(arg(2, "snap")?)?;
            need(c, &ds)?;
            let bookmark = format!("{ds}#{snap}");
            if !c.exists(&bookmark)? {
                c.zfs(&["bookmark", &format!("{ds}@{snap}"), &bookmark])?;
            }
            for s in c.children("snapshot", &ds, false)? {
                if s.split_once('@').is_some_and(|(_, n)| n.starts_with("cb-")) {
                    c.zfs(&["destroy", &s])?;
                }
            }
            for b in c.children("bookmark", &ds, false)? {
                if b.split_once('#')
                    .is_some_and(|(_, n)| n != snap && n.starts_with("cb-"))
                {
                    c.zfs(&["destroy", &b])?;
                }
            }
            say(c.out, &format!("sent {snap}"))
        }
        "drop" => {
            let snap = valid::snap_name(arg(2, "snap")?)?;
            let at = format!("{ds}@{snap}");
            if c.exists(&at)? {
                c.zfs(&["destroy", &at])?;
            }
            say(c.out, &format!("dropped {snap}"))
        }
        "bookmarks" => {
            need(c, &ds)?;
            for b in c.children("bookmark", &ds, true)? {
                if let Some((_, n)) = b.split_once('#')
                    && n.starts_with("cb-")
                {
                    say(c.out, n)?;
                }
            }
            Ok(())
        }
        "set-aside" => {
            let tag = valid::tag(arg(2, "tag")?)?;
            if !c.exists(&ds)? {
                return say(c.out, "missing");
            }
            let aside = format!("{ds}.{tag}");
            if c.exists(&aside)? {
                return Err(Fail::invalid(format!("exists: {slug}.{tag}")));
            }
            // Remember where it was mounted: the restored dataset takes its place.
            if c.get("source", "mountpoint", &ds)? == "local" {
                let mp = c.get("value", "mountpoint", &ds)?;
                c.zfs(&["set", &format!("{MPROP}={mp}"), &ds])?;
            }
            if c.get("value", "mounted", &ds)? == "yes" {
                c.zfs(&["unmount", &ds])?;
            }
            if c.get("value", "keystatus", &ds)? == "available" {
                c.zfs(&["unload-key", &ds])?;
            }
            c.zfs(&["set", "canmount=noauto", &ds])?;
            c.zfs(&["rename", &ds, &aside])?;
            say(c.out, &format!("set aside {slug}.{tag}"))
        }
        "receive" => {
            let quota = valid::mb(arg(2, "quota-mb")?)?;
            if c.exists(&ds)? {
                return Err(Fail::invalid("exists: set it aside first"));
            }
            // A raw stream keeps the tenant's encryption; the quota and
            // mountpoint aren't in it, so they're set here (mountpoint: from
            // the most recent dataset set aside for this tenant).
            let listing = c.zfs(&[
                "list",
                "-H",
                "-o",
                &format!("name,{MPROP}"),
                "-s",
                "creation",
                "-d",
                "1",
                &parent,
            ])?;
            let prefix = format!("{ds}.");
            let mp = listing
                .lines()
                .filter_map(|l| l.split_once('\t'))
                .filter(|(name, v)| name.starts_with(&prefix) && *v != "-")
                .map(|(_, v)| v.trim().to_owned())
                .next_back();
            if let Some(mp) = &mp
                && (!mp.starts_with('/') || mp.split('/').any(|c| c == ".."))
            {
                return Err(Fail::new(
                    1,
                    format!("set-aside mountpoint is not an absolute path: {mp}"),
                ));
            }
            let quota = format!("quota={quota}M");
            let mut a = vec!["receive", "-u", "-o", &quota];
            let mountpoint = mp.map(|m| format!("mountpoint={m}"));
            if let Some(m) = &mountpoint {
                a.extend(["-o", m]);
            }
            a.push(&ds);
            c.sys.run("zfs", &a, Input::Inherit)?;
            c.zfs(&["set", "keylocation=prompt", &ds])?;
            // Snapshots that came with the stream: the next backup starts a new chain.
            for s in c.children("snapshot", &ds, false)? {
                c.zfs(&["destroy", &s])?;
            }
            say(c.out, "received")
        }
        "put-back" => {
            let tag = valid::tag(arg(2, "tag")?)?;
            let aside = format!("{ds}.{tag}");
            if !c.exists(&aside)? {
                return Err(Fail::new(3, format!("nothing set aside as {slug}.{tag}")));
            }
            if c.exists(&ds)? {
                return Err(Fail::invalid(format!("exists: {slug} (not overwriting)")));
            }
            c.zfs(&["rename", &aside, &ds])?;
            c.zfs(&["set", "canmount=on", &ds])?;
            c.zfs(&["inherit", MPROP, &ds])?;
            say(c.out, "put back")
        }
        "discard" => {
            let tag = valid::tag(arg(2, "tag")?)?;
            let aside = format!("{ds}.{tag}");
            if c.exists(&aside)? {
                c.zfs(&["destroy", "-r", &aside])?;
            }
            say(c.out, &format!("discarded {slug}.{tag}"))
        }
        _ => Err(Fail::invalid(format!("unknown command: {cmd}"))),
    }
}

fn need(c: &mut Ctx<'_>, ds: &str) -> Result {
    if c.exists(ds)? {
        Ok(())
    } else {
        exit_with(c.out, "missing", 3)
    }
}
