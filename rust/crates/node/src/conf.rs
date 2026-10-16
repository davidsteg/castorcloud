//! castor.conf: the node's settings, next to the binary, written by the
//! node's setup script. The shell scripts sourced it (ran it as code as
//! root); here it is parsed as `KEY=VALUE` lines and only trusted when root
//! owns it and nobody else can write it. Without it the macOS defaults apply.

use std::collections::HashMap;
use std::path::Path;

use crate::{Fail, Result};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Conf {
    vars: HashMap<String, String>,
}

/// Where tenant datasets live and are mounted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub pool: String,
    pub dataset_root: String,
    pub mount_root: String,
}

/// What the gate needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateConf {
    /// Templates and tenant config files.
    pub root: String,
    /// How to run the Docker CLI, e.g. `["sudo", "-n", "/usr/bin/docker"]`.
    pub docker: Vec<String>,
    /// DOCKER_HOST for the Docker CLI; None leaves it unset.
    pub docker_host: Option<String>,
}

impl Conf {
    /// Reads `path` if it exists. `require_root`: refuse a file that isn't
    /// root-owned or that group or others can write.
    pub fn load(path: &Path, require_root: bool) -> Result<Self> {
        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(Fail::new(1, format!("{}: {e}", path.display()))),
        };
        if require_root {
            use std::os::unix::fs::MetadataExt as _;
            if meta.uid() != 0 || meta.mode() & 0o022 != 0 {
                return Err(Fail::new(
                    1,
                    format!("{}: must be owned by root and not writable by others", path.display()),
                ));
            }
        }
        let text = std::fs::read_to_string(path).map_err(|e| Fail::new(1, format!("{}: {e}", path.display())))?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Self> {
        let mut vars = HashMap::new();
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let bad = || Fail::new(1, format!("castor.conf line {}: expected KEY=VALUE", n + 1));
            let (key, value) = line.split_once('=').ok_or_else(bad)?;
            if key.is_empty()
                || !key
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
            {
                return Err(bad());
            }
            let value = match value.strip_prefix('"') {
                Some(rest) => rest.strip_suffix('"').ok_or_else(bad)?,
                None => value,
            };
            // Values are taken literally; anything that would have meant
            // something to the shell is refused rather than misread.
            if value.bytes().any(|b| b"\"'`$\\;&|<>(){}".contains(&b)) {
                return Err(bad());
            }
            vars.insert(key.to_owned(), value.to_owned());
        }
        Ok(Self { vars })
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.vars.get(key).map(String::as_str).filter(|v| !v.is_empty())
    }

    pub fn layout(&self) -> Result<Layout> {
        let pool = self.get("POOL").unwrap_or("castor").to_owned();
        let dataset_root = self
            .get("DATASET_ROOT")
            .map_or_else(|| format!("{pool}/t"), str::to_owned);
        let mount_root = self
            .get("MOUNT_ROOT")
            .map_or_else(|| format!("/Volumes/{pool}/t"), str::to_owned);
        for name in [&pool, &dataset_root] {
            if name.starts_with('-') || name.starts_with('/') || name.contains(char::is_whitespace) {
                return Err(Fail::new(1, format!("castor.conf: bad dataset name {name}")));
            }
        }
        absolute(&mount_root)?;
        Ok(Layout {
            pool,
            dataset_root,
            mount_root,
        })
    }

    /// `home`: the gate user's HOME, for Colima's default socket.
    pub fn gate(&self, home: Option<&str>) -> Result<GateConf> {
        let root = self.get("ROOT").unwrap_or("/Users/Shared/castor").to_owned();
        absolute(&root)?;
        let docker: Vec<String> = self
            .get("DOCKER")
            .unwrap_or("docker")
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        // Set (even empty) in castor.conf wins: TrueNAS clears it to use the
        // system engine. Otherwise Colima's engine on a Mac.
        let docker_host = match self.vars.get("DOCKER_HOST") {
            Some(v) if v.is_empty() => None,
            Some(v) => Some(v.clone()),
            None => home.map(|h| format!("unix://{h}/.colima/default/docker.sock")),
        };
        Ok(GateConf {
            root,
            docker,
            docker_host,
        })
    }
}

fn absolute(p: &str) -> Result {
    if !p.starts_with('/') || p.split('/').any(|c| c == "..") {
        return Err(Fail::new(1, format!("castor.conf: {p} must be an absolute path")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    // What infra/node/truenas/setup.sh writes.
    const TRUENAS: &str = "# Written by infra/node/truenas/setup.sh. Read by castor-ssh and castor-zfs.
POOL=tank
DATASET_ROOT=tank/castor/t
MOUNT_ROOT=/mnt/tank/castor/t
ROOT=/mnt/tank/castor/node
DOCKER=\"sudo -n /usr/bin/docker\"
DOCKER_HOST=
";

    #[test]
    fn truenas() {
        let c = Conf::parse(TRUENAS).unwrap();
        assert_eq!(
            c.layout().unwrap(),
            Layout {
                pool: "tank".into(),
                dataset_root: "tank/castor/t".into(),
                mount_root: "/mnt/tank/castor/t".into()
            }
        );
        let g = c.gate(Some("/home/castor")).unwrap();
        assert_eq!(g.root, "/mnt/tank/castor/node");
        assert_eq!(g.docker, ["sudo", "-n", "/usr/bin/docker"]);
        assert_eq!(g.docker_host, None);
    }

    #[test]
    fn macos_defaults() {
        let c = Conf::default();
        assert_eq!(
            c.layout().unwrap(),
            Layout {
                pool: "castor".into(),
                dataset_root: "castor/t".into(),
                mount_root: "/Volumes/castor/t".into()
            }
        );
        let g = c.gate(Some("/Users/castor")).unwrap();
        assert_eq!(g.root, "/Users/Shared/castor");
        assert_eq!(g.docker, ["docker"]);
        assert_eq!(
            g.docker_host.as_deref(),
            Some("unix:///Users/castor/.colima/default/docker.sock")
        );
    }

    #[test]
    fn refuses_shell() {
        for bad in [
            "POOL=$(id)",
            "POOL=`id`",
            "POOL=a;b",
            "export POOL=a",
            "POOL=\"a",
            "rm -rf /",
            "lower=x",
        ] {
            assert!(Conf::parse(bad).is_err(), "{bad}");
        }
        assert!(Conf::parse("MOUNT_ROOT=relative").unwrap().layout().is_err());
        assert!(Conf::parse("POOL=-o").unwrap().layout().is_err());
        assert!(Conf::parse("ROOT=/a/../b").unwrap().gate(None).is_err());
    }

    #[test]
    fn refuses_a_file_others_can_write() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("castor.conf");
        std::fs::write(&path, "POOL=tank\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(Conf::load(&path, true).is_err());
        assert_eq!(Conf::load(&path, false).unwrap().layout().unwrap().pool, "tank");
        assert_eq!(Conf::load(&dir.path().join("missing"), true).unwrap(), Conf::default());
    }
}
