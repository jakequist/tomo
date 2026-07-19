//! Resolving the `(ssh-target, remote-path)` pair from the `sync`/`connect` CLI
//! arguments.
//!
//! Two shapes are accepted:
//! - the classic two-argument form (`<target> <remote-path>`), and
//! - the rsync-style single-argument form (`[user@]host:PATH`), split at the
//!   first `:` outside a `[...]` IPv6 group (see
//!   [`tomo_transport::split_target_path`]).
//!
//! It also runs the fast **local-`~` guard**: if the remote path is an absolute
//! path that lies under the *local* user's home, the shell almost certainly
//! expanded an unquoted `~` before tomo saw it, producing a path that is
//! meaningless on the remote. That is caught here — before any SSH — with a
//! copy-pasteable fix.

use crate::error::CliError;

/// Resolve the effective `(ssh_target, remote_path)` from the raw CLI arguments.
///
/// `remote_path` is the optional second positional argument. When it is present
/// the two-argument form is used verbatim; when it is absent, `target` is parsed
/// as the single-argument `host:path` form. An empty path (after a trailing
/// colon) or a bare target with no path at all is a specific error.
///
/// # Errors
/// [`CliError::Message`] for a missing/empty remote path, or when the local-`~`
/// guard ([`local_home_hint`]) fires.
pub(crate) fn resolve(
    target: &str,
    remote_path: Option<&str>,
) -> Result<(String, String), CliError> {
    let (host, path) = match remote_path {
        Some(path) => (target.to_owned(), path.to_owned()),
        None => match tomo_transport::split_target_path(target) {
            Some((_, "")) => {
                return Err(CliError::msg(format!(
                    "remote path is empty in '{target}' — write it as 'host:/path' or \
                     'host:~/path' (the part after the colon is the peer's project path)"
                )));
            }
            Some((host, path)) => (host.to_owned(), path.to_owned()),
            None => {
                return Err(CliError::msg(
                    "provide both an <ssh-target> and a <remote-path> (e.g. `tomo sync \
                     user@host /path`), or the single-argument form `tomo sync \
                     user@host:/path`",
                ));
            }
        },
    };

    if let Some(msg) = local_home_hint(std::env::var_os("HOME").as_deref(), &host, &path) {
        return Err(CliError::msg(msg));
    }
    Ok((host, path))
}

/// If `remote_path` is an absolute path that lies under the local `home`, return
/// the friendly "your shell expanded ~" message; otherwise `None`.
///
/// Pure over its inputs (the local home is threaded in) so the detection and the
/// message shaping are unit-tested without touching the environment. `<rest>` is
/// the remote path with the local-home prefix stripped, giving a copy-pasteable
/// `host:~/<rest>` suggestion.
fn local_home_hint(
    home: Option<&std::ffi::OsStr>,
    host: &str,
    remote_path: &str,
) -> Option<String> {
    let home = home?;
    let home = home.to_string_lossy();
    let home = home.trim_end_matches('/');
    if home.is_empty() {
        return None;
    }
    // Under-home iff the path equals home or has home as a `/`-terminated prefix.
    let rest = match remote_path.strip_prefix(home) {
        Some("") => "",
        Some(r) if r.starts_with('/') => r.trim_start_matches('/'),
        _ => return None,
    };
    Some(format!(
        "'{remote_path}' is under YOUR home directory — your shell expanded ~ before \
         tomo saw it. Use '{host}:~/{rest}' or quote it: \"~/{rest}\""
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn two_arg_form_used_verbatim() {
        let (h, p) = resolve("user@host", Some("/srv/app")).unwrap();
        assert_eq!((h.as_str(), p.as_str()), ("user@host", "/srv/app"));
    }

    #[test]
    fn colon_form_is_split() {
        let (h, p) = resolve("dev@box:/srv/app", None).unwrap();
        assert_eq!((h.as_str(), p.as_str()), ("dev@box", "/srv/app"));
        // Tilde path survives to be expanded server-side later.
        let (h, p) = resolve("dev@box:~/proj", None).unwrap();
        assert_eq!((h.as_str(), p.as_str()), ("dev@box", "~/proj"));
    }

    #[test]
    fn colon_form_empty_path_errors() {
        assert!(resolve("host:", None).is_err());
    }

    #[test]
    fn bare_target_without_path_errors() {
        assert!(resolve("user@host", None).is_err());
    }

    // ---- local-home guard (Item 4) --------------------------------------

    #[test]
    fn detects_path_under_local_home() {
        let msg = local_home_hint(Some(OsStr::new("/home/jake")), "dev@box", "/home/jake/proj")
            .expect("should fire");
        assert!(msg.contains("under YOUR home directory"));
        assert!(msg.contains("dev@box:~/proj"));
        assert!(msg.contains("\"~/proj\""));
    }

    #[test]
    fn detects_exact_home() {
        let msg = local_home_hint(Some(OsStr::new("/home/jake")), "box", "/home/jake")
            .expect("should fire");
        // rest is empty → host:~/ and "~/"
        assert!(msg.contains("box:~/"));
    }

    #[test]
    fn ignores_sibling_prefix() {
        // /home/jakeanne must NOT match a /home/jake home.
        assert!(
            local_home_hint(Some(OsStr::new("/home/jake")), "box", "/home/jakeanne/proj").is_none()
        );
    }

    #[test]
    fn ignores_unrelated_absolute_path() {
        assert!(local_home_hint(Some(OsStr::new("/home/jake")), "box", "/srv/app").is_none());
    }

    #[test]
    fn ignores_tilde_path() {
        // A proper `~/…` (not yet expanded) is not under the literal home string.
        assert!(local_home_hint(Some(OsStr::new("/home/jake")), "box", "~/proj").is_none());
    }

    #[test]
    fn no_home_env_is_silent() {
        assert!(local_home_hint(None, "box", "/home/jake/proj").is_none());
    }
}
