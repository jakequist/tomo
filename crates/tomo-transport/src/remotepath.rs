//! Remote-path rewriting: server-side `~` (home) expansion.
//!
//! A pure module. The remote home is resolved by the caller over SFTP
//! (`realpath(".")` — the SSH user's home); this function only rewrites the
//! leading `~` against that home, so the decision is unit-tested without any
//! I/O. See [`crate::ssh::SshSession::expand_remote_path`] for the I/O wrapper.

use crate::error::TransportError;

/// Rewrite a remote path's leading `~` against the remote `home` directory.
///
/// - `~` alone → `home`.
/// - `~/rest` → `home` joined with `rest`.
/// - `~user/…` (a tilde not immediately followed by `/` or end) → an error:
///   per-user home expansion is not supported (Tomo cannot know another user's
///   home without a lookup, and it is virtually never what is meant here).
/// - Anything else (absolute or relative, no leading tilde) → returned unchanged.
///
/// `home` has any trailing slash trimmed so the join never doubles a separator.
///
/// # Errors
/// [`TransportError::RemotePath`] for the unsupported `~user/` form.
///
/// # Examples
/// ```
/// use tomo_transport::expand_remote_tilde;
/// assert_eq!(expand_remote_tilde("~/proj", "/home/dev").unwrap(), "/home/dev/proj");
/// assert_eq!(expand_remote_tilde("~", "/home/dev/").unwrap(), "/home/dev");
/// assert_eq!(expand_remote_tilde("/srv/x", "/home/dev").unwrap(), "/srv/x");
/// assert!(expand_remote_tilde("~bob/x", "/home/dev").is_err());
/// ```
pub fn expand_remote_tilde(path: &str, home: &str) -> Result<String, TransportError> {
    let home = home.trim_end_matches('/');
    if path == "~" {
        return Ok(home.to_owned());
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return Ok(format!("{home}/{rest}"));
    }
    if path.starts_with('~') {
        return Err(TransportError::RemotePath {
            path: path.to_owned(),
            reason: "~user/ home expansion is not supported; use an explicit path or ~/…"
                .to_owned(),
        });
    }
    Ok(path.to_owned())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn bare_tilde_is_home() {
        assert_eq!(expand_remote_tilde("~", "/home/dev").unwrap(), "/home/dev");
        // Trailing slash on home is trimmed.
        assert_eq!(expand_remote_tilde("~", "/home/dev/").unwrap(), "/home/dev");
    }

    #[test]
    fn tilde_slash_joins_remainder() {
        assert_eq!(
            expand_remote_tilde("~/proj", "/home/dev").unwrap(),
            "/home/dev/proj"
        );
        assert_eq!(
            expand_remote_tilde("~/a/b/c", "/home/dev/").unwrap(),
            "/home/dev/a/b/c"
        );
    }

    #[test]
    fn non_tilde_paths_are_unchanged() {
        assert_eq!(
            expand_remote_tilde("/srv/x", "/home/dev").unwrap(),
            "/srv/x"
        );
        assert_eq!(
            expand_remote_tilde("relative/dir", "/home/dev").unwrap(),
            "relative/dir"
        );
        // A tilde that is not leading is not special.
        assert_eq!(
            expand_remote_tilde("/srv/~cache", "/home/dev").unwrap(),
            "/srv/~cache"
        );
    }

    #[test]
    fn tilde_user_form_is_rejected() {
        let err = expand_remote_tilde("~bob/proj", "/home/dev").unwrap_err();
        assert!(matches!(err, TransportError::RemotePath { .. }));
    }
}
