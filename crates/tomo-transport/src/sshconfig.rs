//! A minimal, **pure** `ssh_config(5)` reader.
//!
//! Tomo authenticates over SSH like a user's system `ssh` already does — but
//! [`SshOpts`](crate::SshOpts) only knows the two default key names and the
//! agent. A user with a differently-named key selected via `~/.ssh/config`
//! (e.g. a global `IdentityFile ~/.ssh/id_work`, or a `Host` alias) would have
//! `ssh host` work while `tomo connect host` fails auth. This module closes that
//! gap by resolving the `IdentityFile` directives that apply to a host.
//!
//! It is deliberately **not** a full `ssh_config` implementation: it covers the
//! parts that decide which private keys to try — global (pre-`Host`) directives,
//! `Host` pattern blocks (whitespace-separated globs with `*`/`?`, `!` negation),
//! and `IdentityFile` accumulation with `~`/`%d` home expansion. `Match` blocks
//! and every other keyword are ignored. Keeping it pure (content in, paths out)
//! preserves this crate's rule of reading no environment for policy — the CLI
//! does the file I/O and passes the text in.

use std::path::{Path, PathBuf};

/// The `IdentityFile` paths that apply to `host` in the given `ssh_config`
/// `content`, in file order (first declared first), tilde/`%d`-expanded against
/// `home`. Duplicates are removed, preserving first-seen order.
///
/// Global directives that appear before any `Host` line apply to every host
/// (matching `ssh`'s own semantics), so a top-of-file `IdentityFile` is always
/// included. Returns an empty vec when the config names no identity for `host`.
///
/// # Examples
/// ```
/// use std::path::Path;
/// use tomo_transport::identity_files_for;
///
/// let cfg = "IdentityFile ~/.ssh/id_global\n\
///            Host build\n  IdentityFile ~/.ssh/id_build\n";
/// let home = Path::new("/home/u");
/// // The global key applies everywhere; the Host-scoped one only to `build`.
/// assert_eq!(
///     identity_files_for(cfg, "example.com", home),
///     vec![Path::new("/home/u/.ssh/id_global").to_path_buf()]
/// );
/// assert_eq!(
///     identity_files_for(cfg, "build", home),
///     vec![
///         Path::new("/home/u/.ssh/id_global").to_path_buf(),
///         Path::new("/home/u/.ssh/id_build").to_path_buf(),
///     ]
/// );
/// ```
#[must_use]
pub fn identity_files_for(content: &str, host: &str, home: &Path) -> Vec<PathBuf> {
    // A block applies until the next `Host`/`Match`. The section before the
    // first `Host` is global, so we start active.
    let mut active = true;
    let mut out: Vec<PathBuf> = Vec::new();

    for raw in content.lines() {
        let Some((keyword, arg)) = split_directive(raw) else {
            continue;
        };
        let lower = keyword.to_ascii_lowercase();
        match lower.as_str() {
            "host" => active = host_matches(arg, host),
            // We do not evaluate Match conditions; conservatively treat a Match
            // block as not-applicable so we never attribute its keys to `host`.
            "match" => active = false,
            "identityfile" if active => {
                let path = expand_path(unquote(arg), home);
                if !out.contains(&path) {
                    out.push(path);
                }
            }
            _ => {}
        }
    }
    out
}

/// Split one config line into `(keyword, argument)`, or `None` for blank/comment
/// lines. ssh accepts either `Keyword value` or `Keyword=value`, with arbitrary
/// surrounding whitespace; a `#` at the first non-blank position is a comment.
fn split_directive(line: &str) -> Option<(&str, &str)> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    // The keyword ends at the first whitespace or '='; the argument is the rest,
    // with a leading '=' and surrounding whitespace stripped.
    let split_at = trimmed.find(|c: char| c.is_whitespace() || c == '=')?;
    let keyword = &trimmed[..split_at];
    let rest = trimmed[split_at..].trim_start();
    let rest = rest.strip_prefix('=').map_or(rest, str::trim_start);
    if keyword.is_empty() || rest.is_empty() {
        return None;
    }
    Some((keyword, rest))
}

/// Does any whitespace-separated pattern in a `Host` line match `host`? A `!`
/// prefix negates: if any negated pattern matches, the host is excluded outright,
/// mirroring `ssh`'s rule that a single negative match wins.
fn host_matches(patterns: &str, host: &str) -> bool {
    let mut positive = false;
    for pat in patterns.split_whitespace() {
        if let Some(neg) = pat.strip_prefix('!') {
            if glob_match(neg, host) {
                return false;
            }
        } else if glob_match(pat, host) {
            positive = true;
        }
    }
    positive
}

/// Case-insensitive glob match supporting `*` (any run) and `?` (one char) —
/// the only wildcards `ssh_config` `Host` patterns use. Iterative backtracking so
/// it stays linear-ish without recursion or regex.
fn glob_match(pattern: &str, text: &str) -> bool {
    let needle: Vec<char> = pattern.to_ascii_lowercase().chars().collect();
    let hay: Vec<char> = text.to_ascii_lowercase().chars().collect();
    let (mut p, mut t) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while t < hay.len() {
        if p < needle.len() && (needle[p] == '?' || needle[p] == hay[t]) {
            p += 1;
            t += 1;
        } else if p < needle.len() && needle[p] == '*' {
            star = Some(p);
            mark = t;
            p += 1;
        } else if let Some(sp) = star {
            p = sp + 1;
            mark += 1;
            t = mark;
        } else {
            return false;
        }
    }
    while p < needle.len() && needle[p] == '*' {
        p += 1;
    }
    p == needle.len()
}

/// Strip one layer of surrounding double quotes (`ssh_config` allows quoting paths
/// that contain spaces). Unquoted input is returned unchanged.
fn unquote(s: &str) -> &str {
    s.strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .unwrap_or(s)
}

/// Expand an `IdentityFile` argument to an absolute path. `~` and the `%d` token
/// both stand for the home directory; an already-absolute path is kept as-is;
/// any other relative path resolves under `~/.ssh` (ssh's default base).
fn expand_path(arg: &str, home: &Path) -> PathBuf {
    if let Some(rest) = arg.strip_prefix("~/").or_else(|| arg.strip_prefix("%d/")) {
        return home.join(rest);
    }
    if arg == "~" || arg == "%d" {
        return home.to_path_buf();
    }
    let path = Path::new(arg);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        home.join(".ssh").join(arg)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // panics are fine in tests
mod tests {
    use super::*;

    fn home() -> PathBuf {
        PathBuf::from("/home/jake")
    }
    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn global_identity_applies_to_every_host() {
        // Jake's real case: a top-of-file `IdentityFile ~/.ssh/id_tokyo` before
        // any Host block must be offered for `localhost`.
        let cfg =
            "IdentityFile ~/.ssh/id_tokyo\n\nHost github.com\n  IdentityFile ~/.ssh/id_github\n";
        assert_eq!(
            identity_files_for(cfg, "localhost", &home()),
            vec![p("/home/jake/.ssh/id_tokyo")]
        );
    }

    #[test]
    fn host_block_adds_to_global() {
        let cfg =
            "IdentityFile ~/.ssh/id_tokyo\nHost github.com\n  IdentityFile ~/.ssh/id_github\n";
        assert_eq!(
            identity_files_for(cfg, "github.com", &home()),
            vec![
                p("/home/jake/.ssh/id_tokyo"),
                p("/home/jake/.ssh/id_github"),
            ]
        );
    }

    #[test]
    fn non_matching_host_gets_only_global() {
        let cfg = "Host vm1\n  IdentityFile ~/.ssh/id_m\n";
        assert!(identity_files_for(cfg, "elsewhere", &home()).is_empty());
    }

    #[test]
    fn glob_star_and_question_match() {
        let cfg = "Host 10.0.0.*\n  IdentityFile ~/.ssh/id_lan\nHost web?\n  IdentityFile ~/.ssh/id_web\n";
        assert_eq!(
            identity_files_for(cfg, "10.0.0.71", &home()),
            vec![p("/home/jake/.ssh/id_lan")]
        );
        assert_eq!(
            identity_files_for(cfg, "web3", &home()),
            vec![p("/home/jake/.ssh/id_web")]
        );
        assert!(identity_files_for(cfg, "web42", &home()).is_empty());
    }

    #[test]
    fn multiple_patterns_on_one_host_line() {
        let cfg = "Host alpha beta gamma\n  IdentityFile ~/.ssh/id_greek\n";
        for h in ["alpha", "beta", "gamma"] {
            assert_eq!(
                identity_files_for(cfg, h, &home()),
                vec![p("/home/jake/.ssh/id_greek")],
                "host {h}"
            );
        }
        assert!(identity_files_for(cfg, "delta", &home()).is_empty());
    }

    #[test]
    fn negation_excludes() {
        let cfg = "Host *.internal !secret.internal\n  IdentityFile ~/.ssh/id_int\n";
        assert_eq!(
            identity_files_for(cfg, "box.internal", &home()),
            vec![p("/home/jake/.ssh/id_int")]
        );
        assert!(identity_files_for(cfg, "secret.internal", &home()).is_empty());
    }

    #[test]
    fn equals_form_and_case_insensitive_keyword() {
        let cfg = "identityfile=~/.ssh/id_lower\nHOST=build\n  IDENTITYFILE = ~/.ssh/id_build\n";
        assert_eq!(
            identity_files_for(cfg, "build", &home()),
            vec![p("/home/jake/.ssh/id_lower"), p("/home/jake/.ssh/id_build"),]
        );
    }

    #[test]
    fn comments_and_blanks_ignored() {
        let cfg = "# a comment\n\n  # indented comment\nIdentityFile ~/.ssh/id_ok\n";
        assert_eq!(
            identity_files_for(cfg, "any", &home()),
            vec![p("/home/jake/.ssh/id_ok")]
        );
    }

    #[test]
    fn absolute_and_tilde_and_relative_paths() {
        let cfg = "IdentityFile /abs/key\nIdentityFile ~/tkey\nIdentityFile relkey\n";
        assert_eq!(
            identity_files_for(cfg, "any", &home()),
            vec![
                p("/abs/key"),
                p("/home/jake/tkey"),
                p("/home/jake/.ssh/relkey"),
            ]
        );
    }

    #[test]
    fn quoted_path_is_unquoted() {
        let cfg = "IdentityFile \"~/.ssh/id with space\"\n";
        assert_eq!(
            identity_files_for(cfg, "any", &home()),
            vec![p("/home/jake/.ssh/id with space")]
        );
    }

    #[test]
    fn duplicates_removed_preserving_order() {
        let cfg = "IdentityFile ~/.ssh/id_a\nHost x\n  IdentityFile ~/.ssh/id_a\n  IdentityFile ~/.ssh/id_b\n";
        assert_eq!(
            identity_files_for(cfg, "x", &home()),
            vec![p("/home/jake/.ssh/id_a"), p("/home/jake/.ssh/id_b")]
        );
    }

    #[test]
    fn match_block_keys_are_not_attributed() {
        let cfg =
            "IdentityFile ~/.ssh/id_global\nMatch host secret\n  IdentityFile ~/.ssh/id_secret\n";
        // The Match block is ignored; only the global key survives.
        assert_eq!(
            identity_files_for(cfg, "secret", &home()),
            vec![p("/home/jake/.ssh/id_global")]
        );
    }

    #[test]
    fn empty_config_yields_nothing() {
        assert!(identity_files_for("", "any", &home()).is_empty());
    }
}
