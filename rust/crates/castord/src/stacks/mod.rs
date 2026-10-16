//! Customers' own stacks (docs/stacks.md): what a definition may be, how it
//! is rendered, and the pieces of apps/provisioner/src/stacks.ts that run
//! on the stacks node.

pub mod edge;
pub mod image;
pub mod js;
pub mod render;
pub mod run;
pub mod spec;

use spec::Units;

/// Where customers' stacks are served (StacksPlatform in stacks.ts).
#[derive(Debug, Clone)]
pub struct Platform {
    /// Registrable domain of the stacks, e.g. castorapps.ch; never the portal's in production.
    pub domain: String,
    pub scheme: String,
    /// Production: the ACME resolver of the stacks domain's wildcard certificate.
    pub cert_resolver: Option<String>,
}

/// Container uids above this are outside the userns mapping (65536 ids).
pub const MAX_CONTAINER_ID: u32 = 65535;

/// Which stacks the booking covers, in the order they were added; the rest are stopped.
pub fn covered<T>(stacks: Vec<(T, Units)>, booked: Units) -> (Vec<T>, Vec<T>) {
    let mut used = Units::default();
    let (mut ok, mut over) = (Vec::new(), Vec::new());
    for (s, units) in stacks {
        let next = used + units;
        if next.fits(booked) {
            ok.push(s);
            used = next;
        } else {
            over.push(s);
        }
    }
    (ok, over)
}

/// name -> id from /etc/passwd or /etc/group, in file order (a later line wins).
pub fn parse_ids(text: &str) -> Vec<(String, u64)> {
    let mut ids: Vec<(String, u64)> = Vec::new();
    for line in text.split('\n') {
        let f: Vec<&str> = line.split(':').collect();
        if f.len() >= 3 && !f[2].is_empty() && f[2].bytes().all(|b| b.is_ascii_digit()) {
            let Ok(id) = f[2].parse::<u64>() else { continue };
            match ids.iter_mut().find(|(n, _)| n == f[0]) {
                Some(e) => e.1 = id,
                None => ids.push((f[0].to_owned(), id)),
            }
        }
    }
    ids
}

pub struct IdFiles<'a> {
    pub passwd: &'a str,
    pub group: &'a str,
}

fn number(s: &str) -> Option<u64> {
    (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse::<u64>().unwrap_or(u64::MAX))
}

/// The container uid/gid a service runs as: "1000", "1000:1000", "node",
/// "node:node", or "" (root); names from the image's own passwd and group.
pub fn resolve_user(user: &str, files: Option<&IdFiles>) -> Result<(u32, u32), String> {
    let mut parts = user.split(':');
    let u = parts.next().unwrap_or("");
    let g = parts.next();
    let passwd = files.map(|f| parse_ids(f.passwd)).unwrap_or_default();
    let groups = files.map(|f| parse_ids(f.group)).unwrap_or_default();
    let lookup = |ids: &[(String, u64)], n: &str| ids.iter().find(|(k, _)| k == n).map(|(_, v)| *v);
    let uid = if u.is_empty() {
        Some(0)
    } else {
        number(u).or_else(|| lookup(&passwd, u))
    };
    let Some(uid) = uid else {
        return Err(format!(
            "user \"{u}\" is not in the image's /etc/passwd; set user: to a number"
        ));
    };
    let gid = match g {
        // The user's primary group from passwd (4th field), else the same number.
        None => {
            let line = files.and_then(|f| {
                f.passwd
                    .split('\n')
                    .find(|l| l.split(':').nth(2) == Some(&uid.to_string()))
            });
            line.and_then(|l| number(l.split(':').nth(3).unwrap_or("")))
                .or(Some(uid))
        }
        Some(g) => number(g).or_else(|| lookup(&groups, g)),
    };
    let Some(gid) = gid else {
        return Err(format!(
            "group \"{}\" is not in the image's /etc/group; set user: to numbers",
            g.unwrap_or("")
        ));
    };
    let max = u64::from(MAX_CONTAINER_ID);
    if uid > max || gid > max {
        return Err(format!(
            "user {uid}:{gid} is outside the allowed range (0-{MAX_CONTAINER_ID})"
        ));
    }
    Ok((u32::try_from(uid).unwrap_or(0), u32::try_from(gid).unwrap_or(0)))
}

/// Whether a user needs the image's passwd/group to resolve.
pub fn needs_names(user: &str) -> bool {
    user.split(':')
        .any(|p| !p.is_empty() && !p.bytes().all(|b| b.is_ascii_digit()))
}

fn norm(p: &str) -> &str {
    match p.trim_end_matches('/') {
        "" => "/",
        t => t,
    }
}

/// Image VOLUME paths no mount covers (a mount at or above the path covers it).
pub fn uncovered_volumes(declared: &[String], mounts: &[&str]) -> Vec<String> {
    let within = |path: &str, mount: &str| {
        path == mount || mount == "/" || path.strip_prefix(mount).is_some_and(|r| r.starts_with('/'))
    };
    declared
        .iter()
        .map(|d| norm(d))
        .filter(|v| !mounts.iter().any(|m| within(v, norm(m))))
        .map(str::to_owned)
        .collect()
}

const BLOCK: usize = 512;

/// A one-file tar archive (what `docker cp -` reads).
pub fn tar_file(name: &str, content: &[u8], mode: u32, mtime: u64) -> Result<Vec<u8>, String> {
    if name.len() > 99 || !name.is_ascii() {
        return Err(format!("tar: bad name {name}"));
    }
    let mut h = [0u8; BLOCK];
    let mut field = |off: usize, s: &str| h[off..off + s.len()].copy_from_slice(s.as_bytes());
    field(0, name);
    field(100, &format!("{mode:07o}\0"));
    field(108, "0000000\0");
    field(116, "0000000\0");
    field(124, &format!("{:011o}\0", content.len()));
    field(136, &format!("{mtime:011o}\0"));
    field(148, "        ");
    field(156, "0");
    field(257, "ustar\0");
    field(263, "00");
    let sum: u32 = h.iter().map(|b| u32::from(*b)).sum();
    h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    let mut out = h.to_vec();
    out.extend_from_slice(content);
    out.resize(out.len() + (BLOCK - content.len() % BLOCK) % BLOCK, 0);
    out.resize(out.len() + 2 * BLOCK, 0);
    Ok(out)
}

/// The contents of the single file in a tar archive (`docker cp <c>:<file> -`).
pub fn untar_file(tar: &[u8]) -> Result<String, String> {
    if tar.len() < BLOCK {
        return Err("empty archive".into());
    }
    let raw = String::from_utf8_lossy(&tar[124..136]);
    let size_field = raw.split('\0').next().unwrap_or("").trim();
    let size = u64::from_str_radix(size_field, 8).map_err(|_| "unexpected archive")?;
    let size = usize::try_from(size).map_err(|_| "unexpected archive")?;
    if size > 1024 * 1024 || BLOCK + size > tar.len() {
        return Err("unexpected archive".into());
    }
    Ok(String::from_utf8_lossy(&tar[BLOCK..BLOCK + size]).into_owned())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn tar_round_trip() {
        let t = tar_file("passwd", b"root:x:0:0\n", 0o644, 1_700_000_000).unwrap();
        assert_eq!(t.len(), 512 * 4);
        assert_eq!(untar_file(&t).unwrap(), "root:x:0:0\n");
        assert!(untar_file(&[0; 10]).is_err());
    }

    #[test]
    fn real_tar_reads_it() {
        let dir = std::env::temp_dir().join(format!("castord-tar-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.tar");
        std::fs::write(&file, tar_file("routes.yml", b"http:\n", 0o644, 1).unwrap()).unwrap();
        let out = std::process::Command::new("tar")
            .arg("-xOf")
            .arg(&file)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(out.stdout, b"http:\n");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
