//! Resolving the `(ssh-target, remote-path)` pair from the `sync`/`connect` CLI
//! argument.
//!
//! Only one shape is accepted: the rsync-style single-argument form
//! (`[user@]host:PATH`), split at the first `:` outside a `[...]` IPv6 group
//! (see [`tomo_transport::split_target_path`]). The classic two-argument
//! `<host> <path>` form was removed — its unquoted-`~` footgun (the *local*
//! shell expanding `~` before tomo ever saw it) was too easy to trip. A stray
//! second argument is still caught here and turned into a helpful "combine them
//! into `host:/path`" message rather than a bare clap usage error.
//!
//! It also runs the fast **local-`~` guard**: if the remote path is an absolute
//! path that lies under the *local* user's home, the shell almost certainly
//! expanded an unquoted `~` before tomo saw it, producing a path that is
//! meaningless on the remote. That is caught here — before any SSH — with a
//! copy-pasteable fix.

use crate::error::CliError;

/// Resolve the effective `(ssh_target, remote_path)` from the raw CLI argument.
///
/// `target` is the single combined `host:path` positional. `legacy_remote_path`
/// is the removed second positional: when present it means the caller typed the
/// old two-argument form, which is rejected with a message showing the combined
/// `host:/path` (or `host:~/path`) equivalent. When absent, `target` is parsed
/// as the `host:path` form; a missing colon, or an empty path after a trailing
/// colon, is a specific error.
///
/// # Errors
/// [`CliError::Message`] for the removed two-argument form, a target with no
/// colon, a missing/empty remote path, or when the local-`~` guard
/// ([`local_home_hint`]) fires.
pub(crate) fn resolve(
    target: &str,
    legacy_remote_path: Option<&str>,
) -> Result<(String, String), CliError> {
    // The two-argument `<host> <path>` form was removed. If a second positional
    // slipped through, guide the user to the combined single-argument target
    // (with a `host:~/path` hint when the second arg looks home-relative or was
    // locally tilde-expanded) rather than emitting a bare clap error.
    if let Some(path) = legacy_remote_path {
        return Err(CliError::msg(two_arg_removed_message(
            std::env::var_os("HOME").as_deref(),
            target,
            path,
        )));
    }

    let (host, path) = match tomo_transport::split_target_path(target) {
        Some((_, "")) => {
            return Err(CliError::msg(format!(
                "remote path is empty in '{target}' — write it as 'host:/path' or \
                 'host:~/path' (the part after the colon is the peer's project path)"
            )));
        }
        Some((host, path)) => (host.to_owned(), path.to_owned()),
        None => {
            return Err(CliError::msg(format!(
                "'{target}' has no ':' separating the host from the path — name the peer \
                 as a single 'host:/path' target (e.g. `tomo sync user@host:/srv/app`, or \
                 `host:~/proj` for the remote home)"
            )));
        }
    };

    if let Some(msg) = local_home_hint(std::env::var_os("HOME").as_deref(), &host, &path) {
        return Err(CliError::msg(msg));
    }
    Ok((host, path))
}

/// If `path` is an absolute path that lies under `home`, return the remainder
/// after the home prefix (leading `/` trimmed; `Some("")` when it equals home);
/// otherwise `None`.
///
/// Pure over its inputs (the local home is threaded in) so the shell-`~`
/// detection and message shaping are unit-tested without touching the
/// environment. The returned slice borrows from `path`, not `home`.
fn under_home<'a>(home: Option<&std::ffi::OsStr>, path: &'a str) -> Option<&'a str> {
    let home = home?;
    let home = home.to_string_lossy();
    let home = home.trim_end_matches('/');
    if home.is_empty() {
        return None;
    }
    // Under-home iff the path equals home or has home as a `/`-terminated prefix.
    match path.strip_prefix(home) {
        Some("") => Some(""),
        Some(rest) if rest.starts_with('/') => Some(rest.trim_start_matches('/')),
        _ => None,
    }
}

/// If `remote_path` is an absolute path that lies under the local `home`, return
/// the friendly "your shell expanded ~" message; otherwise `None`.
///
/// `<rest>` is the remote path with the local-home prefix stripped, giving a
/// copy-pasteable `host:~/<rest>` suggestion.
fn local_home_hint(
    home: Option<&std::ffi::OsStr>,
    host: &str,
    remote_path: &str,
) -> Option<String> {
    let rest = under_home(home, remote_path)?;
    Some(format!(
        "'{remote_path}' is under YOUR home directory — your shell expanded ~ before \
         tomo saw it. Use '{host}:~/{rest}' or quote it: \"~/{rest}\""
    ))
}

/// The message shown when the removed two-argument `<host> <path>` form is used.
///
/// Reconstructs what the caller most likely meant as a single `host:path`
/// target. When the second argument is an unquoted `~/…` that the local shell
/// already expanded to a home-absolute path, the tilde form (`host:~/rest`,
/// expanded on the *remote*) is suggested instead. Pure over `home` so the
/// reconstruction is unit-tested without the environment.
fn two_arg_removed_message(home: Option<&std::ffi::OsStr>, host: &str, path: &str) -> String {
    // A path already carrying a leading `~` cannot be under the literal home
    // string; only an unquoted, shell-expanded `~/…` (now home-absolute) needs
    // the tilde-form hint.
    if !path.starts_with('~') {
        if let Some(rest) = under_home(home, path) {
            return format!(
                "the two-argument `<host> <path>` form was removed, and '{path}' is under \
                 YOUR home directory — your shell expanded ~ before tomo saw it. Write the \
                 peer as a single target against the peer's home instead: '{host}:~/{rest}'"
            );
        }
    }
    format!(
        "the two-argument `<host> <path>` form was removed — write the peer as a single \
         'host:/path' target instead: '{host}:{path}'"
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    // ---- single-argument `host:path` form (the only accepted shape) ------

    #[test]
    fn colon_form_is_split() {
        let (h, p) = resolve("dev@box:/srv/app", None).unwrap();
        assert_eq!((h.as_str(), p.as_str()), ("dev@box", "/srv/app"));
        // Tilde path survives to be expanded server-side later.
        let (h, p) = resolve("dev@box:~/proj", None).unwrap();
        assert_eq!((h.as_str(), p.as_str()), ("dev@box", "~/proj"));
    }

    #[test]
    fn colon_form_accepts_user_host_and_relative_and_ipv6() {
        assert_eq!(
            resolve("user@host:/srv", None).unwrap(),
            ("user@host".to_owned(), "/srv".to_owned())
        );
        // A relative remote path (no leading slash) is passed through verbatim.
        assert_eq!(
            resolve("host:relative/path", None).unwrap(),
            ("host".to_owned(), "relative/path".to_owned())
        );
        // Bracketed IPv6 literal: the split ignores the address's inner colons.
        assert_eq!(
            resolve("[::1]:/srv", None).unwrap(),
            ("[::1]".to_owned(), "/srv".to_owned())
        );
        assert_eq!(
            resolve("user@[fe80::1]:~/p", None).unwrap(),
            ("user@[fe80::1]".to_owned(), "~/p".to_owned())
        );
        // Only the first outside-bracket colon splits; later colons stay in path.
        assert_eq!(
            resolve("host:/srv:weird", None).unwrap(),
            ("host".to_owned(), "/srv:weird".to_owned())
        );
    }

    #[test]
    fn colon_form_empty_path_errors() {
        let err = resolve("host:", None).unwrap_err();
        assert!(err.to_string().contains("remote path is empty"));
    }

    #[test]
    fn no_colon_target_errors_with_hostpath_hint() {
        // A bare target (no colon) is the common "forgot the path" mistake — and
        // must NOT be silently treated as the removed two-argument form.
        let err = resolve("user@host", None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no ':'"), "unexpected message: {msg}");
        assert!(msg.contains("host:/path"), "unexpected message: {msg}");
    }

    // ---- removed two-argument form ---------------------------------------

    #[test]
    fn two_arg_form_is_rejected_with_combined_suggestion() {
        let err = resolve("myhost", Some("/remote/path")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("was removed"), "unexpected message: {msg}");
        assert!(
            msg.contains("'myhost:/remote/path'"),
            "unexpected message: {msg}"
        );
    }

    #[test]
    fn two_arg_message_plain_path() {
        let msg = two_arg_removed_message(Some(OsStr::new("/home/jake")), "myhost", "/remote/path");
        assert!(msg.contains("was removed"));
        assert!(msg.contains("'myhost:/remote/path'"));
    }

    #[test]
    fn two_arg_message_literal_tilde_is_joined() {
        // A quoted `~/proj` reaches us intact → just needs the colon.
        let msg = two_arg_removed_message(Some(OsStr::new("/home/jake")), "box", "~/proj");
        assert!(msg.contains("'box:~/proj'"), "unexpected message: {msg}");
    }

    #[test]
    fn two_arg_message_shell_expanded_tilde_suggests_remote_home() {
        // An unquoted `~/proj` the local shell expanded to a home-absolute path.
        let msg = two_arg_removed_message(Some(OsStr::new("/home/jake")), "box", "/home/jake/proj");
        assert!(msg.contains("expanded ~"), "unexpected message: {msg}");
        assert!(msg.contains("'box:~/proj'"), "unexpected message: {msg}");
    }

    #[test]
    fn two_arg_message_home_expanded_without_env_falls_back_to_plain() {
        // No HOME known → cannot detect expansion; still offers the colon form.
        let msg = two_arg_removed_message(None, "box", "/home/jake/proj");
        assert!(msg.contains("'box:/home/jake/proj'"), "unexpected: {msg}");
    }

    // ---- local-home guard (single-argument form) -------------------------

    #[test]
    fn under_home_detects_and_ignores() {
        assert_eq!(
            under_home(Some(OsStr::new("/home/jake")), "/home/jake/proj"),
            Some("proj")
        );
        assert_eq!(
            under_home(Some(OsStr::new("/home/jake")), "/home/jake"),
            Some("")
        );
        // Sibling prefix must not match.
        assert_eq!(
            under_home(Some(OsStr::new("/home/jake")), "/home/jakeanne/proj"),
            None
        );
        // Unrelated absolute path.
        assert_eq!(under_home(Some(OsStr::new("/home/jake")), "/srv/app"), None);
        // A proper `~/…` is not under the literal home string.
        assert_eq!(under_home(Some(OsStr::new("/home/jake")), "~/proj"), None);
        // No HOME env is silent.
        assert_eq!(under_home(None, "/home/jake/proj"), None);
    }

    #[test]
    fn local_home_hint_fires_for_expanded_path() {
        let msg = local_home_hint(Some(OsStr::new("/home/jake")), "dev@box", "/home/jake/proj")
            .expect("should fire");
        assert!(msg.contains("under YOUR home directory"));
        assert!(msg.contains("dev@box:~/proj"));
        assert!(msg.contains("\"~/proj\""));
    }

    #[test]
    fn local_home_hint_silent_for_normal_paths() {
        assert!(local_home_hint(Some(OsStr::new("/home/jake")), "box", "/srv/app").is_none());
        assert!(local_home_hint(Some(OsStr::new("/home/jake")), "box", "~/proj").is_none());
        assert!(local_home_hint(None, "box", "/home/jake/proj").is_none());
    }
}
