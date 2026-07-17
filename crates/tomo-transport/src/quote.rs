//! POSIX shell single-quote escaping for remote command construction.
//!
//! Pure module. We build remote command lines (`uname -s -m`,
//! `sha256sum '<path>'`, `cd '<path>' && exec '<bin>' serve --stdio`) and run
//! them through the remote login shell, so every interpolated value must be
//! quoted so paths with spaces, quotes, `$`, `;`, backticks, or newlines cannot
//! break out. The single-quote strategy is the only fully-general one in POSIX
//! sh: wrap in `'…'` and replace each embedded `'` with `'\''`.

/// Single-quote `arg` for safe interpolation into a POSIX shell command.
///
/// The result always parses back to exactly `arg` as one shell word (or the
/// literal empty string `''` for an empty input). This is total: there is no
/// byte a POSIX shell can misinterpret inside single quotes except `'` itself,
/// which we escape as `'\''`.
///
/// ```
/// use tomo_transport::shell_quote;
/// assert_eq!(shell_quote("plain"), "'plain'");
/// assert_eq!(shell_quote("a b"), "'a b'");
/// assert_eq!(shell_quote("it's"), r"'it'\''s'");
/// assert_eq!(shell_quote(""), "''");
/// ```
pub fn shell_quote(arg: &str) -> String {
    // Worst case every char is a quote (`'` → `'\''`, 4 bytes) plus two wrapping
    // quotes; pre-size generously to avoid reallocation.
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('\'');
    for ch in arg.chars() {
        if ch == '\'' {
            out.push_str(r"'\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Join already-quoted or literal command words with single spaces.
///
/// Words are inserted verbatim — callers pass literal argv0/flags directly and
/// wrap only the untrusted values with [`shell_quote`].
pub fn shell_line(words: &[&str]) -> String {
    words.join(" ")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn plain_word() {
        assert_eq!(shell_quote("hello"), "'hello'");
    }

    #[test]
    fn empty_is_empty_string_literal() {
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn spaces() {
        assert_eq!(shell_quote("/srv/my project"), "'/srv/my project'");
    }

    #[test]
    fn single_quote_embedded() {
        assert_eq!(shell_quote("it's a trap"), r"'it'\''s a trap'");
    }

    #[test]
    fn hostile_metacharacters() {
        // None of these may escape the quotes.
        let hostile = "$(rm -rf /); `reboot` & echo \"pwned\" | tee /etc/x #\n$HOME";
        let quoted = shell_quote(hostile);
        assert!(quoted.starts_with('\''));
        assert!(quoted.ends_with('\''));
        // The only unescaped `'` characters are the wrapping ones; there are no
        // single quotes in the hostile string, so exactly two remain.
        assert_eq!(quoted.matches('\'').count(), 2);
    }

    #[test]
    fn many_single_quotes() {
        assert_eq!(shell_quote("''"), r"''\'''\'''");
    }

    #[test]
    fn line_joins_words() {
        assert_eq!(
            shell_line(&["cd", &shell_quote("/a b"), "&&", "exec", &shell_quote("x")]),
            "cd '/a b' && exec 'x'"
        );
    }
}
