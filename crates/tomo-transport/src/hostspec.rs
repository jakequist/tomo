//! Parsing of `user@host[:port]` SSH targets.
//!
//! A pure module: no I/O, exhaustively unit-tested (including hostile inputs)
//! per the deliverable. The default port is 22 and the default user is the
//! local login name (resolved by the caller, not here, so this stays pure).

use crate::error::TransportError;

/// The SSH default port.
pub const DEFAULT_SSH_PORT: u16 = 22;

/// A parsed SSH target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostSpec {
    /// The login user, if the target specified one (`user@…`). `None` means the
    /// caller should substitute the local login name.
    pub user: Option<String>,
    /// The hostname or IP literal.
    pub host: String,
    /// The TCP port (defaults to [`DEFAULT_SSH_PORT`]).
    pub port: u16,
}

impl HostSpec {
    /// Parse `user@host[:port]`.
    ///
    /// Rules:
    /// - At most one `@`; the part before it is the user (must be non-empty if
    ///   present), the part after is `host[:port]`.
    /// - A trailing `:<port>` sets the port; the port must be a valid `u16` > 0.
    /// - IPv6 literals must be bracketed (`[::1]` or `[::1]:22`) so the colons
    ///   inside the address are not mistaken for a port separator.
    /// - The host must be non-empty.
    ///
    /// # Errors
    /// [`TransportError::HostSpec`] describing the specific problem.
    pub fn parse(target: &str) -> Result<HostSpec, TransportError> {
        let bad = |reason: &str| TransportError::HostSpec {
            target: target.to_owned(),
            reason: reason.to_owned(),
        };

        let trimmed = target.trim();
        if trimmed.is_empty() {
            return Err(bad("empty target"));
        }

        // Split the (optional) user off the front. `rsplit` is wrong here — a
        // username cannot contain `@`, and neither can the host, so a single
        // `@` is expected; more than one is an error.
        let (user, hostport) = match trimmed.split_once('@') {
            Some((u, rest)) => {
                if u.is_empty() {
                    return Err(bad("empty user before '@'"));
                }
                if rest.contains('@') {
                    return Err(bad("more than one '@'"));
                }
                (Some(u.to_owned()), rest)
            }
            None => (None, trimmed),
        };

        let (host, port) = Self::split_host_port(hostport, &bad)?;
        if host.is_empty() {
            return Err(bad("empty host"));
        }
        Ok(HostSpec { user, host, port })
    }

    /// Split `host[:port]`, honoring `[ipv6]` bracketing.
    fn split_host_port<F>(hostport: &str, bad: &F) -> Result<(String, u16), TransportError>
    where
        F: Fn(&str) -> TransportError,
    {
        if let Some(rest) = hostport.strip_prefix('[') {
            // Bracketed IPv6 literal: `[addr]` or `[addr]:port`.
            let Some(close) = rest.find(']') else {
                return Err(bad("unterminated '[' in IPv6 literal"));
            };
            let addr = &rest[..close];
            let after = &rest[close + 1..];
            let port = if after.is_empty() {
                DEFAULT_SSH_PORT
            } else if let Some(p) = after.strip_prefix(':') {
                Self::parse_port(p, bad)?
            } else {
                return Err(bad("trailing characters after ']'"));
            };
            return Ok((addr.to_owned(), port));
        }

        // Unbracketed. A single trailing `:port` is a port; multiple colons
        // mean a bare IPv6 literal, which must be bracketed to carry a port —
        // treat the whole thing as the host with the default port.
        match hostport.rsplit_once(':') {
            Some((h, p)) if !h.contains(':') => Ok((h.to_owned(), Self::parse_port(p, bad)?)),
            _ => Ok((hostport.to_owned(), DEFAULT_SSH_PORT)),
        }
    }

    fn parse_port<F>(text: &str, bad: &F) -> Result<u16, TransportError>
    where
        F: Fn(&str) -> TransportError,
    {
        match text.parse::<u16>() {
            Ok(0) => Err(bad("port 0 is not valid")),
            Ok(p) => Ok(p),
            Err(_) => Err(bad("port is not a valid number in 1..=65535")),
        }
    }

    /// The user to log in as, given a fallback (the local login name).
    pub fn user_or<'a>(&'a self, fallback: &'a str) -> &'a str {
        self.user.as_deref().unwrap_or(fallback)
    }

    /// The `host:port` string used for error messages and connection.
    pub fn host_port(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// Split a single-argument rsync-style `[user@]host:PATH` target into its SSH
/// target (`[user@]host`, possibly a bracketed IPv6 literal) and the remote
/// PATH.
///
/// The split is at the **first `:` outside a `[...]` group**, so an IPv6
/// literal's own colons (which must be bracketed) are never mistaken for the
/// separator: `[::1]:/srv` → (`[::1]`, `/srv`), `user@[fe80::1]:~/p` →
/// (`user@[fe80::1]`, `~/p`). Returns `None` when there is no such colon — the
/// argument is then a bare target carrying no path, which the caller reports as
/// an error (the `host:/path` form is required). An empty PATH after the colon is
/// returned as `Some((host, ""))` so the caller can reject it with a specific
/// message.
///
/// # Examples
/// ```
/// use tomo_transport::split_target_path;
/// assert_eq!(split_target_path("dev@box:~/proj"), Some(("dev@box", "~/proj")));
/// assert_eq!(split_target_path("host:/srv/app"), Some(("host", "/srv/app")));
/// assert_eq!(split_target_path("[::1]:/srv"), Some(("[::1]", "/srv")));
/// assert_eq!(split_target_path("user@host"), None); // no path → caller errors
/// ```
#[must_use]
pub fn split_target_path(arg: &str) -> Option<(&str, &str)> {
    let mut depth: usize = 0;
    for (i, c) in arg.char_indices() {
        match c {
            '[' => depth += 1,
            ']' => depth = depth.saturating_sub(1),
            ':' if depth == 0 => return Some((&arg[..i], &arg[i + 1..])),
            _ => {}
        }
    }
    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn plain_host() {
        let s = HostSpec::parse("build-server").unwrap();
        assert_eq!(s.user, None);
        assert_eq!(s.host, "build-server");
        assert_eq!(s.port, 22);
    }

    #[test]
    fn user_and_host() {
        let s = HostSpec::parse("jake@localhost").unwrap();
        assert_eq!(s.user.as_deref(), Some("jake"));
        assert_eq!(s.host, "localhost");
        assert_eq!(s.port, 22);
    }

    #[test]
    fn user_host_port() {
        let s = HostSpec::parse("root@10.0.0.5:2222").unwrap();
        assert_eq!(s.user.as_deref(), Some("root"));
        assert_eq!(s.host, "10.0.0.5");
        assert_eq!(s.port, 2222);
    }

    #[test]
    fn host_port_no_user() {
        let s = HostSpec::parse("example.com:22").unwrap();
        assert_eq!(s.user, None);
        assert_eq!(s.host, "example.com");
        assert_eq!(s.port, 22);
    }

    #[test]
    fn bracketed_ipv6_no_port() {
        let s = HostSpec::parse("user@[fe80::1]").unwrap();
        assert_eq!(s.host, "fe80::1");
        assert_eq!(s.port, 22);
    }

    #[test]
    fn bracketed_ipv6_with_port() {
        let s = HostSpec::parse("[::1]:2200").unwrap();
        assert_eq!(s.host, "::1");
        assert_eq!(s.port, 2200);
    }

    #[test]
    fn bare_ipv6_is_host_only() {
        // Unbracketed IPv6 cannot carry a port; the whole thing is the host.
        let s = HostSpec::parse("fe80::1").unwrap();
        assert_eq!(s.host, "fe80::1");
        assert_eq!(s.port, 22);
    }

    #[test]
    fn user_fallback() {
        let s = HostSpec::parse("host").unwrap();
        assert_eq!(s.user_or("me"), "me");
        let s2 = HostSpec::parse("you@host").unwrap();
        assert_eq!(s2.user_or("me"), "you");
    }

    #[test]
    fn rejects_empty() {
        assert!(HostSpec::parse("").is_err());
        assert!(HostSpec::parse("   ").is_err());
    }

    #[test]
    fn rejects_empty_user() {
        assert!(HostSpec::parse("@host").is_err());
    }

    #[test]
    fn rejects_double_at() {
        assert!(HostSpec::parse("a@b@host").is_err());
    }

    #[test]
    fn rejects_bad_port() {
        assert!(HostSpec::parse("host:0").is_err());
        assert!(HostSpec::parse("host:70000").is_err());
        assert!(HostSpec::parse("host:abc").is_err());
    }

    #[test]
    fn rejects_unterminated_bracket() {
        assert!(HostSpec::parse("[::1").is_err());
    }

    // ---- split_target_path (rsync-style host:path) -----------------------

    #[test]
    fn split_plain_host_path() {
        assert_eq!(
            split_target_path("host:/srv/app"),
            Some(("host", "/srv/app"))
        );
        assert_eq!(
            split_target_path("dev@box:~/proj"),
            Some(("dev@box", "~/proj"))
        );
    }

    #[test]
    fn split_first_colon_only() {
        // Only the FIRST outside-bracket colon splits; later colons stay in path.
        assert_eq!(
            split_target_path("host:/srv:weird"),
            Some(("host", "/srv:weird"))
        );
    }

    #[test]
    fn split_ipv6_bracket_safe() {
        assert_eq!(split_target_path("[::1]:/srv"), Some(("[::1]", "/srv")));
        assert_eq!(
            split_target_path("user@[fe80::1]:~/p"),
            Some(("user@[fe80::1]", "~/p"))
        );
        // A bracketed literal with no trailing path → no outside-bracket colon.
        assert_eq!(split_target_path("[fe80::1]"), None);
    }

    #[test]
    fn split_no_colon_is_none() {
        assert_eq!(split_target_path("user@host"), None);
        assert_eq!(split_target_path("host"), None);
    }

    #[test]
    fn split_empty_path_reported() {
        assert_eq!(split_target_path("host:"), Some(("host", "")));
    }
}
