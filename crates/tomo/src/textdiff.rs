//! The shared hand-rolled textual line diff used by `tomo diff` and
//! `tomo conflicts show`.
//!
//! We deliberately avoid a diff *dependency* (the SPEC's minimal-deps policy):
//! this is not a minimal-edit (LCS) diff, just a common-prefix/suffix trim that
//! brackets the changed region with a few context lines. It is exact for the
//! common single-region edit and merely verbose (the whole middle shown as a
//! delete+add block) for scattered edits — good enough to eyeball a change,
//! with `tomo restore --stdout` available for the full bytes.
//!
//! Only the CLI crate renders to humans (rust-hygiene); this module returns the
//! rendered lines and its callers print them.

/// The largest content, in bytes, either side may have for an inline textual
/// diff. Above this we decline (the terminal is the wrong place for a megabyte
/// of text) and point at `tomo restore --stdout`.
pub(crate) const DIFF_MAX_BYTES: usize = 1024 * 1024;

/// How many lines of the rendered diff a command prints before truncating.
pub(crate) const DIFF_MAX_LINES: usize = 20;

/// Context lines kept on each side of a changed region in [`line_diff`].
const DIFF_CONTEXT: usize = 3;

/// Whether two byte blobs can be shown as an inline textual diff: both valid
/// UTF-8 and each under [`DIFF_MAX_BYTES`]. Binary or oversized content is
/// declined in favour of `tomo restore --stdout`.
pub(crate) fn diffable(a: &[u8], b: &[u8]) -> bool {
    a.len() < DIFF_MAX_BYTES
        && b.len() < DIFF_MAX_BYTES
        && std::str::from_utf8(a).is_ok()
        && std::str::from_utf8(b).is_ok()
}

/// A trivial hand-rolled line diff from `old` to `new`, unified-style.
///
/// Removed (`old`) lines are prefixed `- `, added (`new`) lines `+ `, context
/// ` `. Truncated to `max_lines` with a trailing hint.
pub(crate) fn line_diff(old: &str, new: &str, max_lines: usize) -> Vec<String> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();

    // Longest common prefix.
    let mut pre = 0;
    while pre < a.len() && pre < b.len() && a[pre] == b[pre] {
        pre += 1;
    }
    // Longest common suffix that does not overlap the prefix on either side.
    let mut suf = 0;
    while suf < a.len() - pre && suf < b.len() - pre && a[a.len() - 1 - suf] == b[b.len() - 1 - suf]
    {
        suf += 1;
    }

    let mut out = Vec::new();
    // Leading context (drawn from the common prefix).
    for line in &a[pre.saturating_sub(DIFF_CONTEXT)..pre] {
        out.push(format!("  {line}"));
    }
    // The changed region: old lines removed, new lines added.
    for line in &a[pre..a.len() - suf] {
        out.push(format!("- {line}"));
    }
    for line in &b[pre..b.len() - suf] {
        out.push(format!("+ {line}"));
    }
    // Trailing context (drawn from the common suffix).
    let suffix_start = a.len() - suf;
    let suffix_end = std::cmp::min(a.len(), suffix_start + DIFF_CONTEXT);
    for line in &a[suffix_start..suffix_end] {
        out.push(format!("  {line}"));
    }

    if out.len() > max_lines {
        out.truncate(max_lines);
        out.push("  … (diff truncated; use `tomo restore --stdout` for full content)".to_owned());
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn diffable_accepts_small_utf8() {
        assert!(diffable(b"hello\nworld", b"hello\nthere"));
        assert!(diffable(b"", b""));
    }

    #[test]
    fn diffable_rejects_non_utf8() {
        // 0xff is never a valid UTF-8 byte.
        assert!(!diffable(&[0xff, 0xfe], b"ok"));
        assert!(!diffable(b"ok", &[0xff]));
    }

    #[test]
    fn diffable_rejects_oversized() {
        let big = vec![b'a'; DIFF_MAX_BYTES];
        assert!(!diffable(&big, b"small"));
        assert!(!diffable(b"small", &big));
    }

    #[test]
    fn line_diff_brackets_a_single_change() {
        let old = "one\ntwo\nthree";
        let new = "one\nTWO\nthree";
        let diff = line_diff(old, new, DIFF_MAX_LINES);
        assert!(diff.contains(&"  one".to_owned()), "context kept: {diff:?}");
        assert!(diff.contains(&"- two".to_owned()), "old line: {diff:?}");
        assert!(diff.contains(&"+ TWO".to_owned()), "new line: {diff:?}");
        assert!(
            diff.contains(&"  three".to_owned()),
            "trailing ctx: {diff:?}"
        );
    }

    #[test]
    fn line_diff_all_additions_when_old_empty() {
        let diff = line_diff("", "a\nb", DIFF_MAX_LINES);
        assert_eq!(diff, vec!["+ a".to_owned(), "+ b".to_owned()]);
    }

    #[test]
    fn line_diff_truncates_at_max_lines() {
        let mut old = String::new();
        let mut new = String::new();
        for i in 0..100 {
            use std::fmt::Write as _;
            let _ = writeln!(old, "l{i}");
            let _ = writeln!(new, "w{i}");
        }
        let diff = line_diff(&old, &new, 20);
        // 20 kept + 1 truncation notice.
        assert_eq!(diff.len(), 21);
        assert!(diff.last().unwrap().contains("truncated"));
    }
}
