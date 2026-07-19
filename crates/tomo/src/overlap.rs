//! Overlapping-tree guard: refuse to sync a project against a peer path that is
//! the same tree, or nests inside/around it.
//!
//! Syncing a project into itself (or a parent/child of itself) is never what the
//! user wants: the two "sides" share files, so every apply is observed as a fresh
//! local change and echoes back — an unbounded feedback loop that also risks
//! writing a peer's `.tomo/` state into the local tree. We catch the obvious,
//! locally-decidable cases at startup and refuse fast with a clear message.
//!
//! # What is decidable, and the limits
//! - **Local-peer mode** (`--local-peer <dir>`) is fully decidable: both roots are
//!   real directories on this machine, so we canonicalize both and refuse when
//!   they are equal or one contains the other ([`paths_overlap`]).
//! - **SSH mode** is only decidable when the peer is **loopback**
//!   ([`host_is_loopback`]) — then the remote path names a directory on *this*
//!   machine and the same canonicalize-and-compare works (best effort). For a
//!   genuinely remote host we cannot know the remote filesystem's layout, so no
//!   guard is possible; and even for loopback we only inspect the *literal* host
//!   string, so a `~/.ssh/config` alias that resolves to localhost is not caught.
//!   These limits are acceptable: the guard exists to stop the easy self-sync
//!   footgun, not to be a security boundary.
//!
//! The two predicates here are **pure** and unit-tested; the canonicalization I/O
//! lives in the caller (`crate::sync`).

use std::path::Path;

/// Whether two canonicalized absolute paths name overlapping trees: identical, or
/// one is an ancestor of the other.
///
/// `Path::starts_with` compares whole components, so `/a/b` overlaps `/a` but
/// `/a/bc` does not overlap `/a/b` — no false positive on a shared name prefix.
/// Callers should pass canonicalized paths so symlinks and `..` cannot disguise
/// an overlap.
#[must_use]
pub fn paths_overlap(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

/// Whether an SSH target's host component denotes the loopback interface.
///
/// Accepts a bare host or a `user@host` (an optional `:port` suffix or a
/// bracketed IPv6 literal are tolerated). Recognizes `localhost`, the IPv4
/// loopback block `127.0.0.0/8`, and the IPv6 loopback `::1`. Purely lexical: an
/// alias that only resolves to loopback via `~/.ssh/config` is intentionally not
/// treated as loopback here (documented limit).
#[must_use]
pub fn host_is_loopback(target: &str) -> bool {
    // Strip an optional `user@` prefix.
    let host = target.rsplit('@').next().unwrap_or(target);
    // Strip a bracketed IPv6 form `[::1]:port` → `::1`.
    let host = host
        .strip_prefix('[')
        .map_or(host, |rest| rest.split(']').next().unwrap_or(rest));
    // Strip a trailing `:port` (only for the non-bracketed, single-colon case, so
    // an IPv6 literal with multiple colons is left intact).
    let host = if host.matches(':').count() == 1 {
        host.split(':').next().unwrap_or(host)
    } else {
        host
    };
    let host = host.trim();
    if host.eq_ignore_ascii_case("localhost") || host == "::1" {
        return true;
    }
    // IPv4 loopback block 127.0.0.0/8.
    let mut octets = host.split('.');
    if octets.clone().count() == 4 && octets.next() == Some("127") {
        return octets.all(|o| o.parse::<u8>().is_ok());
    }
    false
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn identical_paths_overlap() {
        assert!(paths_overlap(&p("/home/u/proj"), &p("/home/u/proj")));
    }

    #[test]
    fn ancestor_and_descendant_overlap_either_order() {
        // peer inside root
        assert!(paths_overlap(&p("/home/u/proj"), &p("/home/u/proj/sub")));
        // root inside peer
        assert!(paths_overlap(&p("/home/u/proj/sub"), &p("/home/u/proj")));
        // deep nesting
        assert!(paths_overlap(&p("/a"), &p("/a/b/c/d")));
    }

    #[test]
    fn disjoint_trees_do_not_overlap() {
        assert!(!paths_overlap(&p("/home/u/proj"), &p("/home/u/other")));
        assert!(!paths_overlap(&p("/a/b"), &p("/a/c")));
    }

    #[test]
    fn shared_name_prefix_is_not_overlap() {
        // Component-wise: `projX` is not under `proj`.
        assert!(!paths_overlap(&p("/home/u/proj"), &p("/home/u/projX")));
        assert!(!paths_overlap(&p("/a/bc"), &p("/a/b")));
    }

    #[test]
    fn loopback_hosts_recognized() {
        for t in [
            "localhost",
            "LocalHost",
            "user@localhost",
            "127.0.0.1",
            "jake@127.0.0.1",
            "127.1.2.3",
            "127.0.0.1:2222",
            "user@localhost:2200",
            "::1",
            "[::1]:22",
            "user@[::1]:22",
        ] {
            assert!(host_is_loopback(t), "{t} should be loopback");
        }
    }

    #[test]
    fn non_loopback_hosts_rejected() {
        for t in [
            "example.com",
            "user@server",
            "10.0.0.71",
            "192.168.1.5",
            "128.0.0.1",  // just outside the 127/8 block
            "1270.0.0.1", // not a valid octet
            "vm1",        // a config alias — not treated as loopback (documented)
            "2001:db8::1",
        ] {
            assert!(!host_is_loopback(t), "{t} should not be loopback");
        }
    }
}
