//! Checks on names that end up in command lines and paths. The same rules as
//! the TypeScript side; values come from the database, but a bad row must
//! never turn into an option, a path outside its directory or a shell word.

fn all(s: &str, ok: impl Fn(u8) -> bool) -> bool {
    !s.is_empty() && s.bytes().all(ok)
}

/// user@host as node.ts accepts it.
pub fn ssh_target(s: &str) -> bool {
    let Some((user, host)) = s.split_once('@') else {
        return false;
    };
    all(user, |b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        && all(host, |b| b.is_ascii_alphanumeric() || b".-".contains(&b))
}

pub fn pool(s: &str) -> bool {
    all(s, |b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}

/// What castor-ssh passes on (SSH_WORD in node.ts).
pub fn ssh_word(s: &str) -> bool {
    all(s, |b| b.is_ascii_alphanumeric() || b"._/=-".contains(&b))
}

/// A tenant slug (SLUG_RE in apps/portal/src/lib/slug.ts): 3 to 30 of
/// a-z, 0-9 and "-", not at either end, no "--".
pub fn slug(s: &str) -> bool {
    let b = s.as_bytes();
    (3..=30).contains(&b.len())
        && all(s, |c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        && b.first() != Some(&b'-')
        && b.last() != Some(&b'-')
        && !s.contains("--")
}

/// A custom stack's name (STACK_NAME_RE in packages/stacks/src/spec.ts).
pub fn stack_name(s: &str) -> bool {
    let b = s.as_bytes();
    (2..=20).contains(&b.len()) && b[0].is_ascii_lowercase() && all(s, |c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

/// A pinned image reference as stored on a Stack (repo[:tag][@sha256:...]):
/// never an option, never a shell word.
pub fn image(s: &str) -> bool {
    s.len() <= 512 && !s.starts_with('-') && all(s, |b| b.is_ascii_alphanumeric() || b"._/:@-".contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules() {
        assert!(ssh_target("deploy@10.0.0.5") && ssh_target("castor_1@nas.lan"));
        assert!(!ssh_target("deploy@") && !ssh_target("@nas") && !ssh_target("a@b@c") && !ssh_target("a@b;c"));
        assert!(pool("tank") && !pool("tank/x") && !pool(""));
        assert!(ssh_word("tenants/anna/x.yml") && ssh_word("a=1") && !ssh_word("a b") && !ssh_word("$(x)"));
        assert!(slug("anna") && slug("a-1b") && slug("abc"));
        for bad in ["ab", "-ann", "ann-", "an--na", "Anna", "an.na", &"a".repeat(31)] {
            assert!(!slug(bad), "{bad}");
        }
        assert!(image("ghcr.io/sissbruecker/linkding@sha256:0a1b") && image("nginx:1.27"));
        assert!(!image("--privileged") && !image("a b") && !image(""));
        assert!(stack_name("linkding") && stack_name("ab1"));
        assert!(!stack_name("a") && !stack_name("1ab") && !stack_name("ab-c") && !stack_name(&"a".repeat(21)));
    }
}
