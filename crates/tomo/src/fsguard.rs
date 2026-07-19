//! The pure case-collision detector behind the case-insensitive ingress guard
//! (macOS↔Linux filename semantics, Tier-1 edge case 3a).
//!
//! On a **case-insensitive** local filesystem (APFS default, exFAT, NTFS) two
//! names that differ only in case — `Foo.txt` and `foo.txt` — address the *same*
//! file. A Linux peer can legitimately hold both as distinct files and ship both
//! down. Applying the second would silently overwrite the first on disk, and the
//! index/echo bookkeeping (which still tracks two distinct [`RelPath`]s) would
//! thrash. So on such a filesystem the session refuses an inbound apply for a
//! path `P` when a *different* already-known path `Q` case-folds equal to it,
//! preserving the incoming bytes in history rather than clobbering `Q`
//! (see [`crate::session`]).
//!
//! This module is the pure predicate for that decision — no I/O, no index type —
//! so the folding rule and the collision logic are exhaustively unit-tested. The
//! session supplies the candidate existing paths and performs the preservation.
//!
//! # The fold
//! We compare with Rust's [`str::to_lowercase`], i.e. the Unicode **simple**
//! lowercase mapping (full `Default_Case_Folding` minus the handful of special
//! multi-char foldings). This covers ASCII (`A`↔`a`) and the common Latin/Greek/
//! Cyrillic letters that collide on real case-insensitive volumes. It is a
//! deliberately conservative approximation of a filesystem's own case-folding
//! (APFS folds per a frozen Unicode table); a rare unfoldable pair would at
//! worst not be flagged here and be caught by the filesystem's own overwrite —
//! but the guard never *falsely* refuses distinct-cased ASCII names, which is
//! the case that actually occurs.

/// Case-fold a path string for collision comparison (Unicode simple lowercase).
#[must_use]
pub fn casefold(s: &str) -> String {
    s.to_lowercase()
}

/// Whether `incoming` and `existing` are *different* paths that collide under
/// case folding (i.e. they would be the same file on a case-insensitive FS).
///
/// Byte-identical paths never "collide" — that is the normal same-file update,
/// not a collision.
#[must_use]
pub fn collides(incoming: &str, existing: &str) -> bool {
    incoming != existing && casefold(incoming) == casefold(existing)
}

/// The first path in `existing` that case-collides with `incoming`, if any.
///
/// `existing` should enumerate the paths that currently occupy a real name on
/// disk (present index entries); a tombstoned path holds no file and cannot
/// collide. Returns the colliding existing path so the caller can name it in the
/// operator-facing note.
#[must_use]
pub fn first_collision<'a, I>(incoming: &str, existing: I) -> Option<&'a str>
where
    I: IntoIterator<Item = &'a str>,
{
    existing.into_iter().find(|e| collides(incoming, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_paths_do_not_collide() {
        assert!(!collides("Foo.txt", "Foo.txt"));
        assert!(!collides("a/b/c.rs", "a/b/c.rs"));
    }

    #[test]
    fn ascii_case_variants_collide() {
        assert!(collides("Foo.txt", "foo.txt"));
        assert!(collides("foo.txt", "FOO.TXT"));
        assert!(collides("src/Main.rs", "src/main.rs"));
    }

    #[test]
    fn distinct_names_do_not_collide() {
        assert!(!collides("foo.txt", "bar.txt"));
        assert!(!collides("a/foo.txt", "b/foo.txt")); // different directory
    }

    #[test]
    fn unicode_case_folds() {
        // Latin-1 supplement and Greek fold under simple lowercase.
        assert!(collides("R\u{c9}SUM\u{c9}.txt", "r\u{e9}sum\u{e9}.txt")); // ÉÉ vs éé
        assert!(collides("\u{3a9}.txt", "\u{3c9}.txt")); // Ω vs ω
    }

    #[test]
    fn first_collision_finds_and_names_the_existing_path() {
        let existing = ["bar.txt", "Foo.txt", "baz.txt"];
        assert_eq!(
            first_collision("foo.txt", existing.iter().copied()),
            Some("Foo.txt")
        );
    }

    #[test]
    fn first_collision_none_when_no_case_match() {
        let existing = ["bar.txt", "baz.txt"];
        assert_eq!(first_collision("foo.txt", existing.iter().copied()), None);
    }

    #[test]
    fn first_collision_ignores_the_exact_same_path() {
        // An existing path byte-equal to the incoming one is the normal update,
        // not a collision — even on a case-insensitive FS.
        let existing = ["foo.txt"];
        assert_eq!(first_collision("foo.txt", existing.iter().copied()), None);
    }

    #[test]
    fn casefold_is_lowercase() {
        assert_eq!(casefold("Foo.TXT"), "foo.txt");
        assert_eq!(casefold("already/lower"), "already/lower");
    }
}
