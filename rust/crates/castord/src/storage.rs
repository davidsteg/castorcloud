//! A tenant's encrypted storage on its node (storage.ts, volume.ts, zfs.ts):
//! a LUKS file through the pc-cryptd image on the local node, an encrypted
//! ZFS dataset through castor-zfs on TrueNAS and Mac nodes, and the tenant's
//! custom-stack dataset on a STACKS node. Only what needs no key so far:
//! status and close.

use crate::node::{CRYPTD_IMAGE, Node, Role};

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Missing,
    Closed,
    Open { used_mb: u64, size_mb: u64, volume_mb: u64 },
}

/// "open <used> <size> <volume>" / "closed" / "missing" (parseStatus).
pub fn parse_status(text: &str) -> Status {
    let w: Vec<&str> = text.split_whitespace().collect();
    match w.first() {
        Some(&"open") => {
            let n = |i: usize| w.get(i).and_then(|s| s.parse().ok()).unwrap_or(0);
            Status::Open {
                used_mb: n(1),
                size_mb: n(2),
                volume_mb: n(3),
            }
        }
        Some(&"closed") => Status::Closed,
        _ => Status::Missing,
    }
}

pub struct TenantStorage<'a> {
    node: &'a Node,
    slug: &'a str,
}

impl<'a> TenantStorage<'a> {
    pub fn new(node: &'a Node, slug: &'a str) -> Result<Self, String> {
        if node.role == Role::Stacks {
            return Err(format!("{} is a stacks node: no tenant data here", node.name));
        }
        Ok(Self { node, slug })
    }

    async fn run(&self, args: &[&str]) -> Result<String, String> {
        let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
        let out = if self.node.is_local() {
            let mut all: Vec<String> = [
                "run",
                "--rm",
                "-i",
                "--privileged",
                "-v",
                "/srv/pc:/srv/pc:rshared",
                CRYPTD_IMAGE,
            ]
            .iter()
            .map(|a| (*a).to_owned())
            .collect();
            all.extend(args);
            self.node.docker(all, false).await?
        } else {
            let mut all = vec!["castor-zfs".to_owned()];
            all.extend(args);
            self.node.host(all, None).await?
        };
        Ok(out.trim().to_owned())
    }

    pub async fn status(&self) -> Result<Status, String> {
        Ok(parse_status(&self.run(&["status", self.slug]).await?))
    }

    /// Unmounts and locks (suspend).
    pub async fn close(&self) -> Result<String, String> {
        self.run(&["close", self.slug]).await
    }
}

/// The tenant's custom-stack data on its STACKS node (castor-zfs stacks-*).
pub struct StacksStorage<'a> {
    node: &'a Node,
    slug: &'a str,
}

impl<'a> StacksStorage<'a> {
    pub fn new(node: &'a Node, slug: &'a str) -> Result<Self, String> {
        if node.role != Role::Stacks {
            return Err(format!("{} is not a stacks node", node.name));
        }
        Ok(Self { node, slug })
    }

    async fn run(&self, cmd: &str) -> Result<String, String> {
        let out = self
            .node
            .host(vec!["castor-zfs".into(), cmd.into(), self.slug.into()], None)
            .await?;
        Ok(out.trim().to_owned())
    }

    pub async fn status(&self) -> Result<Status, String> {
        Ok(parse_status(&self.run("stacks-status").await?))
    }

    pub async fn close(&self) -> Result<String, String> {
        self.run("stacks-close").await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses() {
        assert_eq!(
            parse_status("open 10 100 102\n"),
            Status::Open {
                used_mb: 10,
                size_mb: 100,
                volume_mb: 102
            }
        );
        assert_eq!(parse_status("closed"), Status::Closed);
        assert_eq!(parse_status(""), Status::Missing);
        assert_eq!(parse_status("missing"), Status::Missing);
    }
}
