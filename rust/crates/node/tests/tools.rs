// The tools against a fake zfs that keeps datasets, snapshots and bookmarks
// in memory: same command lines and output the shell scripts produced.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use castor_node::conf::{Conf, GateConf, Layout};
use castor_node::sys::{Input, Sys};
use castor_node::{Fail, Result, gate, snap, zfs};

const KEY: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

#[derive(Default, Clone)]
struct Ds {
    props: BTreeMap<String, String>,
}

#[derive(Default)]
struct FakeZfs {
    /// datasets, `ds@snap` and `ds#bookmark`, each with its properties
    objects: BTreeMap<String, Ds>,
    calls: Vec<String>,
    keys: Vec<String>,
    execs: Vec<String>,
}

impl FakeZfs {
    fn add(&mut self, name: &str, props: &[(&str, &str)]) {
        let mut d = Ds::default();
        for (k, v) in props {
            d.props.insert((*k).into(), (*v).into());
        }
        self.objects.insert(name.into(), d);
    }

    fn dataset(&mut self, name: &str, open: bool) {
        let s = if open {
            ("available", "yes")
        } else {
            ("unavailable", "no")
        };
        self.add(
            name,
            &[
                ("keystatus", s.0),
                ("mounted", s.1),
                ("used", "2097152"),
                ("quota", "10737418240"),
                ("mountpoint", "/mnt/x"),
            ],
        );
    }

    fn set(&mut self, name: &str, k: &str, v: &str) {
        self.objects.get_mut(name).unwrap().props.insert(k.into(), v.into());
    }

    fn children(&self, ds: &str, sep: char) -> Vec<String> {
        self.objects
            .keys()
            .filter(|n| n.strip_prefix(ds).is_some_and(|r| r.starts_with(sep)))
            .cloned()
            .collect()
    }

    fn zfs(&mut self, a: &[&str], stdin: &Input<'_>) -> (i32, String) {
        match a {
            ["list", "-H", "-o", "name", name] => (if self.objects.contains_key(*name) { 0 } else { 1 }, String::new()),
            ["list", "-H", "-o", "name", "-t", kind, rest @ ..] => {
                let ds = rest.last().unwrap();
                let sep = if *kind == "snapshot" { '@' } else { '#' };
                (0, self.children(ds, sep).iter().map(|n| format!("{n}\n")).collect())
            }
            ["list", "-H", "-o", cols, "-s", "creation", "-d", "1", parent] => {
                let prop = cols.strip_prefix("name,").unwrap();
                let out = self
                    .objects
                    .iter()
                    .filter(|(n, _)| n.starts_with(&format!("{parent}/")) && !n.contains(['@', '#']))
                    .map(|(n, d)| format!("{n}\t{}\n", d.props.get(prop).map_or("-", String::as_str)))
                    .collect();
                (0, out)
            }
            ["get", _, "-o", field, prop, ds] => match self.objects.get(*ds) {
                None => (1, String::new()),
                Some(d) if *field == "source" => (
                    0,
                    format!(
                        "{}\n",
                        d.props.get("mountpoint-source").map_or("default", String::as_str)
                    ),
                ),
                Some(d) => (0, format!("{}\n", d.props.get(*prop).map_or("-", String::as_str))),
            },
            ["list", "-H", "-o", "name", "-r", "-d", "1", ds] => (
                0,
                self.objects
                    .keys()
                    .filter(|n| {
                        *n == ds
                            || n.strip_prefix(&format!("{ds}/"))
                                .is_some_and(|r| !r.contains(['/', '@', '#']))
                    })
                    .map(|n| format!("{n}\n"))
                    .collect(),
            ),
            ["list", "-Hp", "-o", "name,used", "-r", "-d", "1", ds] => (
                0,
                self.objects
                    .iter()
                    .filter(|(n, _)| {
                        *n == ds
                            || n.strip_prefix(&format!("{ds}/"))
                                .is_some_and(|r| !r.contains(['/', '@', '#']))
                    })
                    .map(|(n, d)| format!("{n}\t{}\n", d.props.get("used").map_or("0", String::as_str)))
                    .collect(),
            ),
            ["create", "-p", ds] => {
                self.add(ds, &[]);
                (0, String::new())
            }
            // A child: key and mountpoint inherited, mounted by a separate call.
            ["create", "-o", q, ds] if !matches!(stdin, Input::Bytes(_)) => {
                self.dataset(ds, true);
                self.set(ds, "mounted", "no");
                let (k, v) = q.split_once('=').unwrap();
                self.set(ds, k, v);
                (0, String::new())
            }
            ["create", opts @ .., ds] => {
                let Input::Bytes(key) = stdin else {
                    panic!("create without key")
                };
                self.keys.push(String::from_utf8(key.to_vec()).unwrap());
                assert!(opts.contains(&"encryption=aes-256-gcm") && opts.contains(&"keylocation=prompt"));
                self.dataset(ds, true);
                (0, String::new())
            }
            ["load-key", ds] => {
                let Input::Bytes(key) = stdin else {
                    panic!("load-key without key")
                };
                self.keys.push(String::from_utf8(key.to_vec()).unwrap());
                self.set(ds, "keystatus", "available");
                (0, String::new())
            }
            ["unload-key", ds] => {
                self.set(ds, "keystatus", "unavailable");
                (0, String::new())
            }
            ["mount", ds] => {
                self.set(ds, "mounted", "yes");
                (0, String::new())
            }
            ["unmount", .., ds] => {
                self.set(ds, "mounted", "no");
                (0, String::new())
            }
            ["set", kv, ds] => {
                let (k, v) = kv.split_once('=').unwrap();
                self.set(ds, k, v);
                (0, String::new())
            }
            ["inherit", k, ds] => {
                self.objects.get_mut(*ds).unwrap().props.remove(*k);
                (0, String::new())
            }
            ["rename", from, to] => {
                let d = self.objects.remove(*from).unwrap();
                self.objects.insert((*to).into(), d);
                (0, String::new())
            }
            ["destroy", rest @ ..] => {
                let name = rest.last().unwrap();
                let gone: Vec<String> = self
                    .objects
                    .keys()
                    .filter(|n| *n == name || n.starts_with(&format!("{name}@")) || n.starts_with(&format!("{name}#")))
                    .cloned()
                    .collect();
                for n in gone {
                    self.objects.remove(&n);
                }
                (0, String::new())
            }
            ["snapshot", at] => {
                self.add(at, &[]);
                (0, String::new())
            }
            ["bookmark", _, b] => {
                self.add(b, &[]);
                (0, String::new())
            }
            ["receive", "-u", opts @ .., ds] => {
                assert!(matches!(stdin, Input::Inherit));
                self.dataset(ds, false);
                for o in opts.iter().filter(|o| **o != "-o") {
                    let (k, v) = o.split_once('=').unwrap();
                    self.set(ds, k, v);
                }
                self.add(&format!("{ds}@from-stream"), &[]);
                (0, String::new())
            }
            _ => panic!("unexpected zfs {a:?}"),
        }
    }
}

impl Sys for FakeZfs {
    fn output(&mut self, prog: &str, args: &[&str], stdin: Input<'_>) -> Result<(i32, String)> {
        self.calls.push(format!("{prog} {}", args.join(" ")));
        Ok(match prog {
            "zfs" => self.zfs(args, &stdin),
            "zpool" => (0, "2000398934016\t1073741824\tONLINE\n".into()),
            "du" => (0, format!("7\t{}\n", args[1])),
            "chown" => (0, String::new()),
            _ => panic!("unexpected {prog}"),
        })
    }

    fn exec(&mut self, prog: &str, args: &[&str], env: &[(&str, &str)]) -> Fail {
        let env: Vec<String> = env.iter().map(|(k, v)| format!("{k}={v}")).collect();
        self.execs
            .push(format!("{} {prog} {}", env.join(" "), args.join(" ")).trim().to_owned());
        Fail::new(0, "exec")
    }
}

fn layout(mount_root: &str) -> Layout {
    Layout {
        pool: "tank".into(),
        dataset_root: "tank/castor/t".into(),
        mount_root: mount_root.into(),
        stacks: Some(("tank/castor/s".into(), format!("{mount_root}/s"))),
    }
}

fn zfs_run(fake: &mut FakeZfs, mount_root: &str, args: &[&str], stdin: &str) -> (Result, String) {
    let mut out = Vec::new();
    let mut input = stdin.as_bytes();
    let mut c = zfs::Ctx {
        sys: fake,
        layout: layout(mount_root),
        stdin: &mut input,
        out: &mut out,
        darwin: false,
    };
    let r = zfs::main(&mut c, args);
    (r, String::from_utf8(out).unwrap())
}

fn snap_run(fake: &mut FakeZfs, args: &[&str]) -> (Result, String) {
    let mut out = Vec::new();
    let mut c = snap::Ctx {
        sys: fake,
        layout: layout("/mnt/tank/castor/t"),
        out: &mut out,
    };
    let r = snap::main(&mut c, args);
    (r, String::from_utf8(out).unwrap())
}

#[test]
fn zfs_create_open_status_close_destroy() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_str().unwrap();
    let mut f = FakeZfs::default();
    let key = format!("{KEY}\n");

    let (r, out) = zfs_run(&mut f, root, &["create", "anna", "10240", "photos", "db"], &key);
    assert_eq!((r, out.as_str()), (Ok(()), "open\n"));
    assert_eq!(f.keys, std::slice::from_ref(&key));
    assert!(f.calls[1].contains("quota=10240M") && f.calls[1].contains(&format!("mountpoint={root}/anna")));
    assert!(dir.path().join("anna/data/photos").is_dir() && dir.path().join("anna/data/db").is_dir());

    assert_eq!(
        zfs_run(&mut f, root, &["status", "anna"], ""),
        (Ok(()), "open 2 10240 10240\n".into())
    );
    assert_eq!(
        zfs_run(&mut f, root, &["close", "anna"], ""),
        (Ok(()), "closed\n".into())
    );
    assert_eq!(
        zfs_run(&mut f, root, &["status", "anna"], ""),
        (Ok(()), "closed\n".into())
    );
    assert_eq!(
        zfs_run(&mut f, root, &["usage", "anna"], ""),
        (Ok(()), "closed\n".into())
    );

    // Opening a locked dataset hands the key to load-key.
    assert_eq!(
        zfs_run(&mut f, root, &["open", "anna"], &key),
        (Ok(()), "open\n".into())
    );
    assert_eq!(f.keys.len(), 2);

    assert_eq!(
        zfs_run(&mut f, root, &["quota", "anna", "20480"], ""),
        (Ok(()), "quota 20480\n".into())
    );
    assert_eq!(
        zfs_run(&mut f, root, &["destroy", "anna"], ""),
        (Ok(()), "destroyed\n".into())
    );
    assert_eq!(
        zfs_run(&mut f, root, &["status", "anna"], ""),
        (Ok(()), "missing\n".into())
    );
    let (r, out) = zfs_run(&mut f, root, &["open", "anna"], &key);
    assert_eq!((r.unwrap_err().code, out.as_str()), (3, "missing\n"));
}

#[test]
fn zfs_refuses_bad_input_before_touching_anything() {
    let mut f = FakeZfs::default();
    let key = format!("{KEY}\n");
    for (args, stdin) in [
        (vec!["create", "-o", "1"], key.as_str()),
        (vec!["create", "Anna", "1"], &key),
        (vec!["create", "anna", "10G"], &key),
        (vec!["create", "anna", "1", "../etc"], &key),
        (vec!["create", "anna", "1", ".ssh"], &key),
        (vec!["create", "anna", "1"], "not-a-key\n"),
        (vec!["create", "anna", "1"], ""),
        (vec!["quota", "anna", "-1"], ""),
        (vec!["status", "anna/../b"], ""),
        (vec!["format", "anna"], ""),
    ] {
        let (r, _) = zfs_run(&mut f, "/mnt/t", &args, stdin);
        assert_eq!(r.unwrap_err().code, 2, "{args:?}");
    }
    assert!(f.calls.is_empty(), "{:?}", f.calls);
}

#[test]
fn zfs_pool_and_usage() {
    let mut f = FakeZfs::default();
    assert_eq!(
        zfs_run(&mut f, "/x", &["pool"], ""),
        (Ok(()), "pool 1907729 1024 ONLINE\n".into())
    );

    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("anna/data");
    std::fs::create_dir_all(data.join("photos")).unwrap();
    std::fs::create_dir_all(data.join("db")).unwrap();
    std::fs::create_dir_all(data.join(".hidden")).unwrap();
    // A link a tenant container planted is not measured.
    std::os::unix::fs::symlink("/", data.join("escape")).unwrap();
    f.dataset("tank/castor/t/anna", true);
    let (r, out) = zfs_run(&mut f, dir.path().to_str().unwrap(), &["usage", "anna"], "");
    assert_eq!((r, out.as_str()), (Ok(()), "db 7\nphotos 7\n"));
    assert!(
        f.calls
            .iter()
            .filter(|c| c.starts_with("du"))
            .all(|c| c.starts_with("du -smx "))
    );
}

#[test]
fn snap_backup_cycle() {
    let mut f = FakeZfs::default();
    let ds = "tank/castor/t/anna";
    f.dataset(ds, true);
    f.add(&format!("{ds}#cb-1"), &[]);
    f.add(&format!("{ds}@manual"), &[]);

    assert_eq!(
        snap_run(&mut f, &["snapshot", "anna", "cb-2"]),
        (Ok(()), "snapshot cb-2\n".into())
    );
    let (r, _) = snap_run(&mut f, &["send", "anna", "cb-2", "cb-1"]);
    assert_eq!(r.unwrap_err().msg, "exec");
    assert_eq!(f.execs, [format!("zfs send -w -i {ds}#cb-1 {ds}@cb-2")]);

    assert_eq!(
        snap_run(&mut f, &["sent", "anna", "cb-2"]),
        (Ok(()), "sent cb-2\n".into())
    );
    // Backup snapshot and the older bookmark are gone; other snapshots stay.
    let names: Vec<&String> = f.objects.keys().collect();
    assert_eq!(names, [&ds.to_string(), &format!("{ds}#cb-2"), &format!("{ds}@manual")]);
    assert_eq!(snap_run(&mut f, &["bookmarks", "anna"]), (Ok(()), "cb-2\n".into()));
    assert_eq!(
        snap_run(&mut f, &["drop", "anna", "cb-9"]),
        (Ok(()), "dropped cb-9\n".into())
    );

    let (r, out) = snap_run(&mut f, &["bookmarks", "bob"]);
    assert_eq!((r.unwrap_err().code, out.as_str()), (3, "missing\n"));
    for bad in [
        vec!["send", "anna", "cb-2", "x;rm"],
        vec!["snapshot", "anna", "manual"],
        vec!["sent", "-r", "cb-2"],
    ] {
        assert_eq!(snap_run(&mut f, &bad).0.unwrap_err().code, 2, "{bad:?}");
    }
}

#[test]
fn snap_restore_and_put_back() {
    let mut f = FakeZfs::default();
    let ds = "tank/castor/t/anna";
    f.dataset(ds, true);
    f.set(ds, "mountpoint-source", "local");
    f.set(ds, "mountpoint", "/mnt/tank/castor/t/anna");

    assert_eq!(
        snap_run(&mut f, &["set-aside", "anna", "r1"]),
        (Ok(()), "set aside anna.r1\n".into())
    );
    let aside = &f.objects[&format!("{ds}.r1")].props;
    assert_eq!(aside["keystatus"], "unavailable");
    assert_eq!(aside["mounted"], "no");
    assert_eq!(aside["canmount"], "noauto");
    assert_eq!(aside["ch.castorcloud:mountpoint"], "/mnt/tank/castor/t/anna");
    assert_eq!(
        snap_run(&mut f, &["set-aside", "anna", "r1"]),
        (Ok(()), "missing\n".into())
    );

    assert_eq!(
        snap_run(&mut f, &["receive", "anna", "10240"]),
        (Ok(()), "received\n".into())
    );
    let got = &f.objects[ds].props;
    assert_eq!(got["quota"], "10240M");
    assert_eq!(got["mountpoint"], "/mnt/tank/castor/t/anna");
    assert_eq!(got["keylocation"], "prompt");
    assert!(!f.objects.contains_key(&format!("{ds}@from-stream")));
    assert_eq!(snap_run(&mut f, &["receive", "anna", "10240"]).0.unwrap_err().code, 2);

    // A failed restore: drop what was received, put the old one back.
    f.objects.remove(ds);
    assert_eq!(
        snap_run(&mut f, &["put-back", "anna", "r1"]),
        (Ok(()), "put back\n".into())
    );
    assert_eq!(f.objects[ds].props["canmount"], "on");
    assert!(!f.objects[ds].props.contains_key("ch.castorcloud:mountpoint"));
    assert_eq!(snap_run(&mut f, &["put-back", "anna", "r1"]).0.unwrap_err().code, 3);
    assert_eq!(
        snap_run(&mut f, &["discard", "anna", "r1"]),
        (Ok(()), "discarded anna.r1\n".into())
    );
}

fn gate_conf(root: &str) -> GateConf {
    GateConf {
        root: root.into(),
        docker: vec!["sudo".into(), "-n".into(), "/usr/bin/docker".into()],
        docker_host: None,
    }
}

fn gate_run(f: &mut FakeZfs, root: &str, command: &str, stdin: &[u8]) -> (Result, String) {
    let mut out = Vec::new();
    let mut input = stdin;
    let mut c = gate::Ctx {
        sys: f,
        conf: gate_conf(root),
        bin: PathBuf::from("/mnt/tank/castor/bin"),
        stdin: &mut input,
        out: &mut out,
    };
    let r = gate::main(&mut c, command);
    (r, String::from_utf8(out).unwrap())
}

#[test]
fn gate_execs_only_what_it_allows() {
    let mut f = FakeZfs::default();
    for ok in [
        "docker system dial-stdio",
        "'docker' 'system' 'dial-stdio'",
        "castor-zfs create anna 1 photos",
        "castor-snap send anna cb-1",
    ] {
        assert_eq!(gate_run(&mut f, "/r", ok, b"").0.unwrap_err().msg, "exec", "{ok}");
    }
    let want = [
        "sudo -n /usr/bin/docker system dial-stdio",
        "sudo -n /usr/bin/docker system dial-stdio",
        "sudo -n /mnt/tank/castor/bin/castor-zfs create anna 1 photos",
        "sudo -n /mnt/tank/castor/bin/castor-snap send anna cb-1",
    ];
    assert_eq!(f.execs.len(), want.len());
    for (got, want) in f.execs.iter().zip(want) {
        assert!(got.ends_with(want), "{got}");
        assert!(!got.contains("DOCKER_HOST"), "{got}");
    }
    for bad in [
        "",
        "bash",
        "sh -c id",
        "docker run --privileged alpine",
        "docker system dial-stdio; id",
        "castor-zfs $(id)",
        "castor-zfs `id`",
        "castor-zfs a|b",
        "scp -t /etc",
        "castor-put /etc/sudoers",
        "castor-put tenants/../../etc/passwd",
        "castor-put tenants/anna/.env 777",
        "castor-rm tenants",
        "castor-rm tenants/",
        "castor-rm tenants/../x",
        "castor-rm files/templates",
        "castor-untar ../x",
    ] {
        let n = f.execs.len();
        assert_eq!(gate_run(&mut f, "/r", bad, b"").0.unwrap_err().code, 2, "{bad}");
        assert_eq!(f.execs.len(), n, "{bad}");
    }
}

#[test]
fn gate_put_and_rm() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_str().unwrap();
    let mut f = FakeZfs::default();

    assert_eq!(
        gate_run(&mut f, root, "castor-put tenants/anna/.env", b"A=1\n").0,
        Ok(())
    );
    assert_eq!(
        gate_run(&mut f, root, "castor-put tenants/anna/skin.conf 644", b"x").0,
        Ok(())
    );
    let env = dir.path().join("tenants/anna/.env");
    assert_eq!(std::fs::read_to_string(&env).unwrap(), "A=1\n");
    assert_eq!(std::fs::metadata(&env).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(
        std::fs::metadata(dir.path().join("tenants/anna/skin.conf"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o644
    );
    assert!(!dir.path().join("tenants/anna/.env.tmp").exists());

    // A symlink at the temp path is replaced, not followed.
    let outside = dir.path().join("outside");
    std::fs::write(&outside, "keep").unwrap();
    std::os::unix::fs::symlink(&outside, dir.path().join("tenants/anna/.env.tmp")).unwrap();
    assert_eq!(
        gate_run(&mut f, root, "castor-put tenants/anna/.env", b"B=2\n").0,
        Ok(())
    );
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "keep");

    assert_eq!(gate_run(&mut f, root, "castor-rm tenants/anna", b"").0, Ok(()));
    assert!(!dir.path().join("tenants/anna").exists());
    assert_eq!(gate_run(&mut f, root, "castor-rm tenants/anna", b"").0, Ok(()));
}

fn tar_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    for (path, data) in entries {
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        b.append_data(&mut h, path, *data).unwrap();
    }
    b.into_inner().unwrap()
}

#[test]
fn gate_untar() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_str().unwrap();
    let mut f = FakeZfs::default();
    let files = dir.path().join("files");

    let first = tar_of(&[("./compose.yml", b"v1"), ("litellm/config.yaml", b"c")]);
    assert_eq!(gate_run(&mut f, root, "castor-untar node", &first).0, Ok(()));
    assert_eq!(std::fs::read_to_string(files.join("node/compose.yml")).unwrap(), "v1");

    let second = tar_of(&[("compose.yml", b"v2")]);
    assert_eq!(gate_run(&mut f, root, "castor-untar node", &second).0, Ok(()));
    assert_eq!(std::fs::read_to_string(files.join("node/compose.yml")).unwrap(), "v2");
    assert!(
        !files.join("node/litellm").exists() && !files.join("node.old").exists() && !files.join("node.new").exists()
    );

    // A link, or a path out of the directory: refused, the old files stay.
    let mut b = tar::Builder::new(Vec::new());
    let mut h = tar::Header::new_gnu();
    h.set_entry_type(tar::EntryType::Symlink);
    h.set_size(0);
    b.append_link(&mut h, "evil", "/etc").unwrap();
    let link = b.into_inner().unwrap();
    let mut raw = tar_of(&[("x", b"x")]);
    raw[..100].fill(0);
    raw[..15].copy_from_slice(b"../../etc/evil\0");
    let mut h = tar::Header::new_old();
    h.as_mut_bytes().copy_from_slice(&raw[..512]);
    h.set_cksum();
    raw[..512].copy_from_slice(h.as_bytes());
    for bad in [link, raw] {
        assert_eq!(gate_run(&mut f, root, "castor-untar node", &bad).0.unwrap_err().code, 2);
        assert_eq!(std::fs::read_to_string(files.join("node/compose.yml")).unwrap(), "v2");
        assert!(!files.join("node.new").exists());
    }
    assert!(!dir.path().join("etc").exists());
}

#[test]
fn conf_from_truenas_setup_drives_the_layout() {
    let c = Conf::parse("POOL=tank\nDATASET_ROOT=tank/castor/t\nMOUNT_ROOT=/mnt/tank/castor/t\n").unwrap();
    let mut f = FakeZfs::default();
    let mut out = Vec::new();
    let mut input: &[u8] = b"";
    let mut ctx = zfs::Ctx {
        sys: &mut f,
        layout: c.layout().unwrap(),
        stdin: &mut input,
        out: &mut out,
        darwin: false,
    };
    zfs::main(&mut ctx, &["status", "anna"]).unwrap();
    assert_eq!(f.calls, ["zfs list -H -o name tank/castor/t/anna"]);
}

#[test]
fn zfs_stacks() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_str().unwrap();
    let mut f = FakeZfs::default();
    let key = format!("{KEY}\n");
    let ds = "tank/castor/s/anna";

    // Not open yet: no stack can be created.
    let (r, _) = zfs_run(
        &mut f,
        root,
        &["stack-create", "anna", "links", "1024", "data=100000.100000"],
        "",
    );
    assert_eq!(r.unwrap_err().code, 3);

    let (r, out) = zfs_run(&mut f, root, &["stacks-open", "anna", "20480"], &key);
    assert_eq!((r, out.as_str()), (Ok(()), "open\n"));
    assert!(f.calls.contains(&"zfs create -p tank/castor/s".to_owned()));
    let create = f
        .calls
        .iter()
        .find(|c| c.starts_with("zfs create -o encryption"))
        .unwrap();
    assert!(create.contains("encryption=aes-256-gcm") && create.contains(&format!("mountpoint={root}/s/anna")));

    let (r, out) = zfs_run(
        &mut f,
        root,
        &["stack-create", "anna", "links", "1024", "data=100000.100001"],
        "",
    );
    assert_eq!((r, out.as_str()), (Ok(()), "open\n"));
    let data = dir.path().join("s/anna/links/data");
    assert!(data.join("data").is_dir());
    assert!(f.calls.contains(&format!("chown -h 0:0 {}", data.display())));
    assert!(
        f.calls
            .contains(&format!("chown -h 100000:100001 {}", data.join("data").display()))
    );
    assert_eq!(f.objects[&format!("{ds}/links")].props["quota"], "1024M");

    f.set(&format!("{ds}/links"), "used", "5242880");
    assert_eq!(
        zfs_run(&mut f, root, &["stack-usage", "anna"], ""),
        (Ok(()), "links 5\n".into())
    );

    // After a restart: unlocking mounts the stacks again.
    assert_eq!(
        zfs_run(&mut f, root, &["stacks-close", "anna"], ""),
        (Ok(()), "closed\n".into())
    );
    assert_eq!(f.objects[&format!("{ds}/links")].props["mounted"], "no");
    assert_eq!(
        zfs_run(&mut f, root, &["stacks-status", "anna"], ""),
        (Ok(()), "closed\n".into())
    );
    assert_eq!(
        zfs_run(&mut f, root, &["stacks-open", "anna", "20480"], &key),
        (Ok(()), "open\n".into())
    );
    assert_eq!(f.objects[&format!("{ds}/links")].props["mounted"], "yes");

    assert_eq!(
        zfs_run(&mut f, root, &["stack-destroy", "anna", "links"], ""),
        (Ok(()), "destroyed\n".into())
    );
    assert!(!f.objects.contains_key(&format!("{ds}/links")));
    assert_eq!(
        zfs_run(&mut f, root, &["stacks-destroy", "anna"], ""),
        (Ok(()), "destroyed\n".into())
    );
    assert_eq!(
        zfs_run(&mut f, root, &["stacks-status", "anna"], ""),
        (Ok(()), "missing\n".into())
    );
}

#[test]
fn zfs_stacks_refuse_bad_input() {
    let mut f = FakeZfs::default();
    let key = format!("{KEY}\n");
    for (args, stdin) in [
        (vec!["stacks-open", "anna", "0"], key.as_str()),
        (vec!["stacks-open", "anna", "1"], "nope\n"),
        (vec!["stack-create", "anna", "Links", "1"], ""),
        (vec!["stack-create", "anna", "a-b", "1"], ""),
        (vec!["stack-create", "anna", "links", "1", "data=0.0"], ""),
        (vec!["stack-create", "anna", "links", "1", "data=65535.100000"], ""),
        (vec!["stack-create", "anna", "links", "1", "../x=100000.100000"], ""),
        (vec!["stack-create", "anna", "links", "1", "data=100000"], ""),
        (vec!["stack-destroy", "anna", "../t"], ""),
        (vec!["stacks-format", "anna"], ""),
    ] {
        let (r, _) = zfs_run(&mut f, "/mnt/x", &args, stdin);
        assert_eq!(r.unwrap_err().code, 2, "{args:?}");
    }
    assert!(f.calls.is_empty(), "{:?}", f.calls);
}

#[test]
fn zfs_stacks_symlinks_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_str().unwrap();
    let mut f = FakeZfs::default();
    f.dataset("tank/castor/s/anna", true);
    let data = dir.path().join("s/anna/links/data");
    std::fs::create_dir_all(&data).unwrap();
    std::os::unix::fs::symlink("/etc", data.join("data")).unwrap();
    let (r, _) = zfs_run(
        &mut f,
        root,
        &["stack-create", "anna", "links", "1024", "data=100000.100000"],
        "",
    );
    assert_eq!(r.unwrap_err().code, 2);
    assert!(!f.calls.iter().any(|c| c.contains("100000:100000")));
}
