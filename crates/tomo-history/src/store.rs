//! The content-addressed history store: `FastCDC` chunking + BLAKE3 identity +
//! zstd compression, with all metadata and chunk BLOBs in a single `SQLite`
//! database (docs/SPEC.md §6.1).
//!
//! # Why one database file
//! Chunks live as compressed BLOBs in a `chunks` table rather than as many
//! small files: a single file is transactional (invariant #8 — a `kill -9`
//! mid-write cannot leave a torn tree or a dangling chunk), avoids the
//! many-small-files problem, and keeps all of Tomo's state under
//! `<root>/.tomo/` (invariant #2). This is revisited only if measured as a
//! bottleneck.
//!
//! # Ordering authority
//! The `wall_ms` recorded on every version is **display only** (invariant #7);
//! ordering is decided by the vector clock, stored as a postcard blob. The
//! store never consults wall time for any decision.

use std::collections::HashSet;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{params, Connection, OptionalExtension};
use tomo_engine::{ContentHash, ContentSig, EntryState, RelPath, ReplicaId, VectorClock};

use crate::error::{hex, HistoryError};

/// `FastCDC` minimum chunk size: 16 KiB (docs/SPEC.md §6.1).
const CDC_MIN: u32 = 16 * 1024;
/// `FastCDC` average (target) chunk size: 64 KiB.
const CDC_AVG: u32 = 64 * 1024;
/// `FastCDC` maximum chunk size: 256 KiB.
const CDC_MAX: u32 = 256 * 1024;

/// zstd compression level for chunk BLOBs. Level 3 is zstd's default: a good
/// ratio/speed balance for a store on the sync hot path.
const ZSTD_LEVEL: i32 = 3;

/// The schema version stamped into `PRAGMA user_version`. Bump when the schema
/// changes so future migrations can branch on it.
///
/// - v1: initial schema (chunks / versions / conflicts).
/// - v2: `versions.exec` — the Unix executable bit per present version (git's
///   model). An older v1 database is migrated in place on open by adding the
///   column with a `0` default (docs/SPEC.md §12).
/// - v3: the `versions_identity` UNIQUE index on `(path, state, clock)` — makes
///   [`HistoryStore::record_version`] idempotent at the STORE level (SEED-PERF
///   Phase 2, bug B2). Version-row dedup was previously a distributed caller
///   contract (the session's `find_or_record` `log()` check); a crash-retry that
///   re-recorded the same version therefore duplicated the row. The unique index
///   turns a duplicate ingest into a no-op (`INSERT OR IGNORE`) so the batch
///   ingest and any crash replay stay idempotent. An older v1/v2 database is
///   migrated in place on open: pre-existing duplicate rows are collapsed to
///   their lowest id (conflicts repointed) before the index is created.
const SCHEMA_VERSION: i64 = 3;

/// A monotonically increasing version identifier (the `versions.id` rowid).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VersionId(pub i64);

/// A conflict-record identifier (the `conflicts.id` rowid).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConflictId(pub i64);

/// Where a recorded version came from: authored locally or received from the
/// peer. Recorded for provenance/display; never used for ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// The version was authored on this replica.
    Local,
    /// The version was received from the remote peer.
    Remote,
}

impl Origin {
    /// The integer stored in the `origin` column.
    fn as_i64(self) -> i64 {
        match self {
            Origin::Local => 0,
            Origin::Remote => 1,
        }
    }

    /// Reconstruct from the stored integer (anything but `0` reads as remote).
    fn from_i64(v: i64) -> Origin {
        if v == 0 {
            Origin::Local
        } else {
            Origin::Remote
        }
    }
}

/// Metadata for one version, as returned by [`HistoryStore::log`].
///
/// `content_hash`/`size` are `Some` exactly when `state` is
/// [`EntryState::Present`]; they mirror the present state's
/// [`ContentSig`] for callers that want the identity without matching on the
/// enum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionMeta {
    /// The version's identifier.
    pub id: VersionId,
    /// Present (with content) or tombstone.
    pub state: EntryState,
    /// The whole-file content hash, if present.
    pub content_hash: Option<ContentHash>,
    /// The file size in bytes, if present.
    pub size: Option<u64>,
    /// The vector clock at which this version was recorded (ordering authority).
    pub clock: VectorClock,
    /// The replica that authored this version.
    pub replica: ReplicaId,
    /// Wall-clock milliseconds since the Unix epoch — **display only**.
    pub wall_ms: u64,
    /// Whether the version was authored locally or received from the peer.
    pub origin: Origin,
}

/// One version to record via the batch API [`HistoryStore::record_versions`].
///
/// Mirrors the argument list of [`HistoryStore::record_version`] as a borrowing
/// struct so a caller can build a slice of them (from staged captures already in
/// hand) and ingest the whole batch in one transaction.
#[derive(Debug, Clone, Copy)]
pub struct VersionRecord<'a> {
    /// The path this version belongs to.
    pub path: &'a RelPath,
    /// Present (with content) or tombstone.
    pub state: &'a EntryState,
    /// The vector clock at which this version was recorded (identity + ordering).
    pub clock: &'a VectorClock,
    /// The authoring replica (provenance/display only).
    pub replica: ReplicaId,
    /// Whether authored locally or received from the peer.
    pub origin: Origin,
    /// Wall-clock milliseconds since the Unix epoch — display only (invariant #7).
    pub wall_ms: u64,
    /// The content bytes (required for `Present`, ignored for `Tombstone`).
    pub bytes: Option<&'a [u8]>,
}

/// A recorded conflict, as returned by [`HistoryStore::conflicts`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictRecord {
    /// The conflict record's identifier.
    pub id: ConflictId,
    /// The path the conflict occurred on.
    pub path: RelPath,
    /// The version chosen as the deterministic winner.
    pub winner: VersionId,
    /// The version preserved as the loser.
    pub loser: VersionId,
    /// Wall-clock milliseconds when the conflict was recorded — display only.
    pub wall_ms: u64,
    /// Whether the conflict has been marked resolved.
    pub resolved: bool,
}

/// A one-line summary of one path's recorded history, as returned by
/// [`HistoryStore::history_paths`]: the path, how many versions it has, and the
/// id and display-only wall time of its newest version. Backs the control
/// channel's `history_paths` command (the TUI history browser's path picker).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathHistory {
    /// The repo-relative path.
    pub path: RelPath,
    /// How many versions of this path are recorded.
    pub versions: u64,
    /// The id of the newest recorded version of this path.
    pub last_version: VersionId,
    /// Wall-clock milliseconds of the newest version — **display only**.
    pub last_wall_ms: u64,
}

/// The result of [`HistoryStore::check`]: an integrity report over the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckReport {
    /// Number of version rows examined.
    pub versions_checked: u64,
    /// Number of chunk rows examined.
    pub chunks_checked: u64,
    /// Whether the store is healthy (no issues found).
    pub ok: bool,
    /// Human-readable descriptions of every integrity problem found. Empty iff
    /// [`CheckReport::ok`].
    pub issues: Vec<String>,
}

/// Convert a `u64` domain value to the `i64` `SQLite` stores. Sizes, counters,
/// and wall-clock milliseconds all fit `i64` for any real input; the top bit is
/// never set in practice, so this round-trips with [`from_sql_int`].
#[allow(clippy::cast_possible_wrap)] // storage bridge; see doc comment above
fn to_sql_int(v: u64) -> i64 {
    v as i64
}

/// Convert an `i64` read back from `SQLite` to our `u64` domain. Inverse of
/// [`to_sql_int`] for any value it produced.
#[allow(clippy::cast_sign_loss)] // storage bridge; see doc comment above
fn from_sql_int(v: i64) -> u64 {
    v as u64
}

/// Length of a byte slice as `u64`, saturating (a slice cannot realistically
/// exceed `u64::MAX`; saturating avoids an infallible-in-practice error path).
fn len_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// The content-addressed history store over a single `SQLite` database at
/// `<project_root>/.tomo/db/history.sqlite`.
///
/// The API is synchronous: the CLI's session thread calls it directly.
#[derive(Debug)]
pub struct HistoryStore {
    conn: Connection,
    /// Whether the `versions` table has the `exec` column (schema v2). Always
    /// true after a read-write [`HistoryStore::open`] (which migrates), but a
    /// read-only open of a not-yet-migrated v1 database sees `false` — the
    /// query builders then substitute a constant `0`, so `log`/`recent` work on
    /// an old database without taking a write lock to migrate it.
    has_exec: bool,
}

impl HistoryStore {
    /// Open (creating if absent) the history store for `project_root`.
    ///
    /// Creates `<project_root>/.tomo/db/`, opens the database, applies the
    /// schema idempotently, and configures WAL journaling with `NORMAL`
    /// synchronous and foreign keys enabled.
    ///
    /// # Errors
    /// [`HistoryError::Io`] if the state directory cannot be created, or
    /// [`HistoryError::Sqlite`] if the database cannot be opened or migrated.
    pub fn open(project_root: &Path) -> Result<Self, HistoryError> {
        let db_dir = project_root.join(".tomo").join("db");
        std::fs::create_dir_all(&db_dir).map_err(|source| HistoryError::Io {
            path: db_dir.clone(),
            source,
        })?;
        let db_path: PathBuf = db_dir.join("history.sqlite");
        let conn = Connection::open(&db_path)?;
        // Let interleaved writers (two handles under WAL) wait out a brief lock
        // rather than failing immediately.
        conn.busy_timeout(Duration::from_secs(5))?;
        Self::configure(&conn)?;
        Self::migrate(&conn)?;
        // Migration guarantees the column exists; compute rather than assume so
        // the invariant is checked, not trusted.
        let has_exec = Self::has_column(&conn, "versions", "exec")?;
        Ok(Self { conn, has_exec })
    }

    /// Open the store read-only, with no directory creation, no schema work,
    /// and no journal-mode change.
    ///
    /// This is what informational commands (`status`, `log`, `conflicts list`)
    /// MUST use: the read-write [`HistoryStore::open`] applies pragmas and
    /// migrations that take write locks, and a status poll racing a starting
    /// sync session once made the session's own open fail with "database is
    /// locked" (found by scenario 02 flaking at "reports connected").
    ///
    /// Returns `Ok(None)` when the database does not exist yet — callers
    /// render that as "no history recorded", never an error.
    ///
    /// # Errors
    /// [`HistoryError::Sqlite`] if the database exists but cannot be opened.
    pub fn open_readonly(project_root: &Path) -> Result<Option<Self>, HistoryError> {
        let db_path = project_root.join(".tomo").join("db").join("history.sqlite");
        if !db_path.exists() {
            return Ok(None);
        }
        let conn = Connection::open_with_flags(
            &db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(Duration::from_secs(5))?;
        // A read-only handle never migrates, so a v1 database (not yet upgraded
        // by a read-write open) still lacks the exec column; detect it so the
        // query builders substitute a constant instead of naming a missing
        // column (which would fail at prepare time).
        let has_exec = Self::has_column(&conn, "versions", "exec")?;
        Ok(Some(Self { conn, has_exec }))
    }

    /// Apply per-connection pragmas: WAL journaling, `NORMAL` synchronous, and
    /// foreign-key enforcement.
    fn configure(conn: &Connection) -> Result<(), HistoryError> {
        // `journal_mode=WAL` returns a row; `execute_batch` steps and discards
        // it. synchronous/foreign_keys are per-connection and set on every open.
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;\n\
             PRAGMA synchronous=NORMAL;\n\
             PRAGMA foreign_keys=ON;",
        )?;
        Ok(())
    }

    /// Create the schema idempotently, migrate an older database in place, and
    /// stamp `user_version`.
    ///
    /// Fresh databases get the current (v2) shape from the `CREATE TABLE`s
    /// below, including `versions.exec`. A pre-existing v1 database still has a
    /// `versions` table without that column (the `IF NOT EXISTS` create is a
    /// no-op for it), so we add the column with an `ALTER TABLE` guarded by a
    /// column-existence check. The default `0` reads back as "not executable",
    /// which is the correct assumption for every version recorded before the
    /// bit was tracked.
    fn migrate(conn: &Connection) -> Result<(), HistoryError> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS chunks (\n\
             \x20 hash  BLOB PRIMARY KEY,\n\
             \x20 size  INTEGER NOT NULL,\n\
             \x20 zdata BLOB NOT NULL\n\
             );\n\
             CREATE TABLE IF NOT EXISTS versions (\n\
             \x20 id           INTEGER PRIMARY KEY AUTOINCREMENT,\n\
             \x20 path         TEXT NOT NULL,\n\
             \x20 state        INTEGER NOT NULL,\n\
             \x20 content_hash BLOB,\n\
             \x20 size         INTEGER,\n\
             \x20 manifest     BLOB,\n\
             \x20 clock        BLOB NOT NULL,\n\
             \x20 replica      INTEGER NOT NULL,\n\
             \x20 wall_ms      INTEGER NOT NULL,\n\
             \x20 origin       INTEGER NOT NULL,\n\
             \x20 exec         INTEGER NOT NULL DEFAULT 0\n\
             );\n\
             CREATE INDEX IF NOT EXISTS versions_path_id ON versions (path, id);\n\
             CREATE TABLE IF NOT EXISTS conflicts (\n\
             \x20 id             INTEGER PRIMARY KEY AUTOINCREMENT,\n\
             \x20 path           TEXT NOT NULL,\n\
             \x20 winner_version INTEGER NOT NULL REFERENCES versions(id),\n\
             \x20 loser_version  INTEGER NOT NULL REFERENCES versions(id),\n\
             \x20 wall_ms        INTEGER NOT NULL,\n\
             \x20 resolved       INTEGER NOT NULL DEFAULT 0\n\
             );",
        )?;
        // v1 → v2: an existing versions table predates the exec column; add it.
        if !Self::has_column(conn, "versions", "exec")? {
            conn.execute_batch("ALTER TABLE versions ADD COLUMN exec INTEGER NOT NULL DEFAULT 0;")?;
        }
        // v2 → v3: the store-level idempotency index (bug B2). A fresh database
        // has no `versions` rows, so the dedup below is a no-op and the index is
        // created cleanly. An older database that accrued duplicate version rows
        // (the pre-fix non-idempotent crash-retry) must have them collapsed
        // first, or `CREATE UNIQUE INDEX` would fail on the existing duplicates.
        // The clock blob is postcard over a `BTreeMap`, so two logically-equal
        // clocks serialize identically — the index dedups exactly the
        // `(path, state, clock)` identity the caller contract used.
        if !Self::has_index(conn, "versions_identity")? {
            Self::dedup_versions(conn)?;
            conn.execute_batch(
                "CREATE UNIQUE INDEX IF NOT EXISTS versions_identity \
                 ON versions (path, state, clock);",
            )?;
        }
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(())
    }

    /// Collapse any duplicate `(path, state, clock)` version rows to their lowest
    /// id, repointing conflict references first so the unique index can be built.
    /// A no-op on a database with no duplicates (the fresh and already-migrated
    /// cases). Wrapped in one transaction so a crash mid-dedup leaves the store
    /// consistent (invariant #8).
    fn dedup_versions(conn: &Connection) -> Result<(), HistoryError> {
        conn.execute_batch(
            // Repoint conflict rows onto the surviving (min-id) member of each
            // identity group, so deleting the duplicates cannot orphan a foreign
            // key. `w` is the currently-referenced version; `v` is its group's
            // canonical (lowest-id) member.
            "BEGIN;\n\
             UPDATE conflicts SET winner_version = (\n\
             \x20 SELECT MIN(v.id) FROM versions v JOIN versions w ON w.id = conflicts.winner_version\n\
             \x20 WHERE v.path = w.path AND v.state = w.state AND v.clock = w.clock);\n\
             UPDATE conflicts SET loser_version = (\n\
             \x20 SELECT MIN(v.id) FROM versions v JOIN versions w ON w.id = conflicts.loser_version\n\
             \x20 WHERE v.path = w.path AND v.state = w.state AND v.clock = w.clock);\n\
             DELETE FROM versions WHERE id NOT IN (\n\
             \x20 SELECT MIN(id) FROM versions GROUP BY path, state, clock);\n\
             COMMIT;",
        )?;
        Ok(())
    }

    /// Whether `table` has a column named `column` (via `PRAGMA table_info`).
    /// Used to make the v1 → v2 `ALTER TABLE` idempotent and re-run-safe.
    fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool, HistoryError> {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let name: String = row.get(1)?;
            if name == column {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Whether an index named `name` exists (via `sqlite_master`). Used to make
    /// the v2 → v3 unique-index migration idempotent and re-run-safe.
    fn has_index(conn: &Connection, name: &str) -> Result<bool, HistoryError> {
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = ?1",
            params![name],
            |row| row.get(0),
        )?;
        Ok(n > 0)
    }

    /// Chunk, hash, compress, and store `bytes`, deduplicating against chunks
    /// already present.
    ///
    /// Returns the whole-file [`ContentHash`] (BLAKE3 of all bytes) and the
    /// number of **new** uncompressed chunk-bytes actually written — chunks
    /// already in the store contribute zero. Storing identical content twice
    /// therefore reports `0` the second time; this is the dedup metric the
    /// scenarios assert on.
    ///
    /// # Errors
    /// [`HistoryError::Sqlite`] on a database failure.
    pub fn store_content(&mut self, bytes: &[u8]) -> Result<(ContentHash, u64), HistoryError> {
        let tx = self.conn.transaction()?;
        let stored = store_chunks(&tx, bytes)?;
        tx.commit()?;
        Ok((stored.hash, stored.new_bytes))
    }

    /// Record a version of `path` in one transaction.
    ///
    /// For [`EntryState::Present`], `bytes` are required and are stored first
    /// (content-addressed, deduplicated); the version row references the
    /// resulting chunk manifest. For [`EntryState::Tombstone`], `bytes` must be
    /// `None`-or-ignored and no content is stored.
    ///
    /// `replica` is the authoring replica (taken from the clock tick by the
    /// caller); `wall_ms` is recorded for display only (invariant #7).
    ///
    /// # Errors
    /// - [`HistoryError::MissingContent`] if `state` is present but `bytes` is
    ///   `None`.
    /// - [`HistoryError::SigMismatch`] if `bytes` do not hash/size to the
    ///   present state's declared [`ContentSig`].
    /// - [`HistoryError::Sqlite`] / [`HistoryError::Clock`] on storage failure.
    // The signature mirrors the version-row shape the store persists (path,
    // state, clock, replica, origin, wall time, content). Bundling these into a
    // struct would only relocate the same fields, so the arity is intrinsic.
    #[allow(clippy::too_many_arguments)]
    pub fn record_version(
        &mut self,
        path: &RelPath,
        state: &EntryState,
        clock: &VectorClock,
        replica: ReplicaId,
        origin: Origin,
        wall_ms: u64,
        bytes: Option<&[u8]>,
    ) -> Result<VersionId, HistoryError> {
        let tx = self.conn.transaction()?;
        let id = insert_version(&tx, path, state, clock, replica, origin, wall_ms, bytes)?;
        tx.commit()?;
        Ok(id)
    }

    /// Record a **batch** of versions in a SINGLE transaction (SEED-PERF Phase 2).
    ///
    /// A bulk seed captures thousands of versions; recording each in its own
    /// transaction pays a per-version commit cost (WAL frame flush, lock cycle)
    /// that dominates the receiver's history-ingest time. Grouping a batch into
    /// one transaction amortizes that to one commit. **A batch is exactly
    /// equivalent to N single [`record_version`] calls** (H8): the same
    /// content-addressed chunks, the same dedup, and — thanks to the v3
    /// `versions_identity` index — the same store-level idempotency, so a
    /// re-ingested (crash-replayed) version is a no-op whether it arrives singly
    /// or in a batch. The returned ids are in input order; a version already
    /// present yields its existing id (no duplicate row).
    ///
    /// The whole batch commits atomically: a `kill -9` mid-batch (before the
    /// commit) leaves the store exactly as it was, and the restarted session
    /// re-ingests idempotently (invariant #8, pairs with scenario 31/H2).
    ///
    /// # Errors
    /// The same per-version errors as [`record_version`] (missing content,
    /// signature mismatch, storage failure); the transaction is rolled back so a
    /// failure records none of the batch.
    pub fn record_versions(
        &mut self,
        records: &[VersionRecord<'_>],
    ) -> Result<Vec<VersionId>, HistoryError> {
        let tx = self.conn.transaction()?;
        let mut ids = Vec::with_capacity(records.len());
        for r in records {
            ids.push(insert_version(
                &tx, r.path, r.state, r.clock, r.replica, r.origin, r.wall_ms, r.bytes,
            )?);
        }
        tx.commit()?;
        Ok(ids)
    }

    /// The stored `(path, clock-blob, state-int)` identity of every version, as a
    /// set — the cheapest way for the session's startup history-completeness
    /// reconcile (SEED-PERF Phase 2, bug B1) to detect index-present-but-
    /// history-absent paths in one pass instead of a `log()` query per file.
    ///
    /// The clock blob is postcard over the `VectorClock`'s `BTreeMap`, exactly
    /// what a caller gets from `postcard::to_allocvec(clock)`, so membership can
    /// be tested against a freshly serialized index-head clock.
    ///
    /// # Errors
    /// [`HistoryError::Sqlite`] on a query failure.
    pub fn version_identities(&self) -> Result<HashSet<(String, Vec<u8>, i64)>, HistoryError> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, clock, state FROM versions")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;
        let mut set = HashSet::new();
        for row in rows {
            set.insert(row?);
        }
        Ok(set)
    }

    /// Record a conflict between two already-stored versions of `path`.
    ///
    /// # Errors
    /// [`HistoryError::Sqlite`] on failure (including a foreign-key violation if
    /// either version id does not exist).
    pub fn record_conflict(
        &mut self,
        path: &RelPath,
        winner: VersionId,
        loser: VersionId,
        wall_ms: u64,
    ) -> Result<ConflictId, HistoryError> {
        self.conn.execute(
            "INSERT INTO conflicts (path, winner_version, loser_version, wall_ms) \
             VALUES (?1, ?2, ?3, ?4)",
            params![path.as_str(), winner.0, loser.0, to_sql_int(wall_ms)],
        )?;
        Ok(ConflictId(self.conn.last_insert_rowid()))
    }

    /// Mark the conflict `id` resolved (acknowledged), clearing it from the
    /// unresolved set surfaced by [`HistoryStore::conflicts`].
    ///
    /// Returns `true` if a row was flipped from unresolved to resolved, and
    /// `false` if the id is unknown or was already resolved — so callers can
    /// report "already acknowledged" without a second query. Idempotent.
    ///
    /// # Errors
    /// [`HistoryError::Sqlite`] on a database failure.
    pub fn mark_conflict_resolved(&mut self, id: ConflictId) -> Result<bool, HistoryError> {
        let changed = self.conn.execute(
            "UPDATE conflicts SET resolved = 1 WHERE id = ?1 AND resolved = 0",
            params![id.0],
        )?;
        Ok(changed == 1)
    }

    /// Mark the conflict `id` unresolved again, returning it to the unresolved
    /// set surfaced by [`HistoryStore::conflicts`]. The exact inverse of
    /// [`mark_conflict_resolved`](HistoryStore::mark_conflict_resolved), it backs
    /// the control channel's `conflict_unresolve` command (the TUI's real undo).
    ///
    /// Returns `true` if a row was flipped from resolved to unresolved, and
    /// `false` if the id is unknown or was already unresolved — so callers can
    /// report "already unresolved" without a second query. Idempotent.
    ///
    /// # Errors
    /// [`HistoryError::Sqlite`] on a database failure.
    pub fn mark_conflict_unresolved(&mut self, id: ConflictId) -> Result<bool, HistoryError> {
        let changed = self.conn.execute(
            "UPDATE conflicts SET resolved = 0 WHERE id = ?1 AND resolved = 1",
            params![id.0],
        )?;
        Ok(changed == 1)
    }

    /// All recorded versions of `path`, newest first.
    ///
    /// # Errors
    /// [`HistoryError::Sqlite`] on a query failure, or [`HistoryError::Clock`]
    /// / [`HistoryError::Malformed`] if a stored row cannot be decoded.
    pub fn log(&self, path: &RelPath) -> Result<Vec<VersionMeta>, HistoryError> {
        // A v1 (unmigrated, read-only) database lacks the exec column; select a
        // constant `0` in its place so old versions read back as non-executable.
        let exec_col = if self.has_exec { "exec" } else { "0" };
        let mut stmt = self.conn.prepare(&format!(
            "SELECT id, state, content_hash, size, clock, replica, wall_ms, origin, {exec_col} \
             FROM versions WHERE path = ?1 ORDER BY id DESC"
        ))?;
        let rows = stmt.query_map(params![path.as_str()], |row| {
            Ok(RawVersion {
                id: row.get(0)?,
                state: row.get(1)?,
                content_hash: row.get(2)?,
                size: row.get(3)?,
                clock: row.get(4)?,
                replica: row.get(5)?,
                wall_ms: row.get(6)?,
                origin: row.get(7)?,
                exec: row.get(8)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?.into_meta()?);
        }
        Ok(out)
    }

    /// The most recently recorded versions across **all** paths, newest first,
    /// capped at `limit`. Each entry pairs the version's path with its metadata.
    ///
    /// Read-only; this backs the repo-wide `tomo log` (no path argument), which
    /// surfaces recent activity anywhere in the tree.
    ///
    /// # Errors
    /// [`HistoryError::Sqlite`] on a query failure, [`HistoryError::Clock`] /
    /// [`HistoryError::Malformed`] if a stored row cannot be decoded.
    pub fn recent(&self, limit: usize) -> Result<Vec<(RelPath, VersionMeta)>, HistoryError> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        // See `log`: a constant stands in for the exec column on a v1 database.
        let exec_col = if self.has_exec { "exec" } else { "0" };
        let mut stmt = self.conn.prepare(&format!(
            "SELECT id, path, state, content_hash, size, clock, replica, wall_ms, origin, {exec_col} \
             FROM versions ORDER BY id DESC LIMIT ?1"
        ))?;
        let rows = stmt.query_map(params![limit], |row| {
            Ok((
                row.get::<_, String>(1)?,
                RawVersion {
                    id: row.get(0)?,
                    state: row.get(2)?,
                    content_hash: row.get(3)?,
                    size: row.get(4)?,
                    clock: row.get(5)?,
                    replica: row.get(6)?,
                    wall_ms: row.get(7)?,
                    origin: row.get(8)?,
                    exec: row.get(9)?,
                },
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (path, raw) = row?;
            let path = RelPath::new(&path).map_err(|e| {
                HistoryError::Malformed(format!("stored version path {path:?}: {e}"))
            })?;
            out.push((path, raw.into_meta()?));
        }
        Ok(out)
    }

    /// The most recently-versioned distinct paths, newest version first, capped
    /// at `limit`. Each entry carries the path, its version count, and the id and
    /// display-only wall time of its newest version.
    ///
    /// Read-only; this backs the control channel's `history_paths` command and
    /// the TUI history browser's path picker (UX-V2 §3, TUI v2). Ordering is by
    /// the newest version's rowid (a version-arrival stand-in), never by wall
    /// time (invariant #7 — wall time is display only).
    ///
    /// # Errors
    /// [`HistoryError::Sqlite`] on a query failure, or [`HistoryError::Malformed`]
    /// if a stored path cannot be decoded.
    pub fn history_paths(&self, limit: usize) -> Result<Vec<PathHistory>, HistoryError> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        // Group by path for the count + newest rowid, then read that version's
        // wall time back. Ordering on the newest rowid keeps this clock-free.
        let mut stmt = self.conn.prepare(
            "SELECT g.path, g.versions, g.last_id, v.wall_ms \
             FROM (SELECT path, COUNT(*) AS versions, MAX(id) AS last_id \
                   FROM versions GROUP BY path) g \
             JOIN versions v ON v.id = g.last_id \
             ORDER BY g.last_id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (path, versions, last_id, wall_ms) = row?;
            let path = RelPath::new(&path).map_err(|e| {
                HistoryError::Malformed(format!("stored version path {path:?}: {e}"))
            })?;
            out.push(PathHistory {
                path,
                versions: from_sql_int(versions),
                last_version: VersionId(last_id),
                last_wall_ms: from_sql_int(wall_ms),
            });
        }
        Ok(out)
    }

    /// The id of the most recently recorded version of `path`, if any.
    ///
    /// # Errors
    /// [`HistoryError::Sqlite`] on a query failure.
    pub fn latest_version_id(&self, path: &RelPath) -> Result<Option<VersionId>, HistoryError> {
        let id: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM versions WHERE path = ?1 ORDER BY id DESC LIMIT 1",
                params![path.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(id.map(VersionId))
    }

    /// Reassemble, decompress, and integrity-check the content of a present
    /// version.
    ///
    /// Every chunk is verified (decompresses cleanly, correct length, BLAKE3
    /// matches its key) and the reassembled whole-file BLAKE3 is verified
    /// against the version's recorded content hash — so a corrupt store yields
    /// an error, never silently wrong bytes.
    ///
    /// # Errors
    /// - [`HistoryError::NoSuchVersion`] if the id is unknown.
    /// - [`HistoryError::NotPresent`] if the version is a tombstone.
    /// - [`HistoryError::MissingChunk`] / [`HistoryError::CorruptChunk`] /
    ///   [`HistoryError::ContentMismatch`] on any integrity failure.
    pub fn get_content(&self, version: VersionId) -> Result<Vec<u8>, HistoryError> {
        let row: Option<ContentRow> = self
            .conn
            .query_row(
                "SELECT state, content_hash, manifest FROM versions WHERE id = ?1",
                params![version.0],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let (state, content_hash, manifest) = row.ok_or(HistoryError::NoSuchVersion(version.0))?;
        if state == 0 {
            return Err(HistoryError::NotPresent(version.0));
        }
        let content_hash = content_hash.ok_or_else(|| {
            HistoryError::Malformed(format!("present version {} has no hash", version.0))
        })?;
        let manifest = manifest.ok_or_else(|| {
            HistoryError::Malformed(format!("present version {} has no manifest", version.0))
        })?;
        if manifest.len() % 32 != 0 {
            return Err(HistoryError::Malformed(format!(
                "version {} manifest length {} is not a multiple of 32",
                version.0,
                manifest.len()
            )));
        }

        let mut stmt = self
            .conn
            .prepare("SELECT size, zdata FROM chunks WHERE hash = ?1")?;
        let mut out = Vec::new();
        for chunk_hash in manifest.chunks_exact(32) {
            let found: Option<(i64, Vec<u8>)> = stmt
                .query_row(params![chunk_hash], |row| Ok((row.get(0)?, row.get(1)?)))
                .optional()?;
            let (size, zdata) =
                found.ok_or_else(|| HistoryError::missing_chunk(version.0, chunk_hash))?;
            let plain = decompress_and_verify(chunk_hash, from_sql_int(size), &zdata)?;
            out.extend_from_slice(&plain);
        }

        let actual = ContentHash(*blake3::hash(&out).as_bytes());
        let expected_hash = to_hash32(&content_hash).ok_or_else(|| {
            HistoryError::Malformed(format!(
                "version {} content_hash is {} bytes, expected 32",
                version.0,
                content_hash.len()
            ))
        })?;
        if actual.0 != expected_hash {
            return Err(HistoryError::ContentMismatch {
                version: version.0,
                expected: hex(&expected_hash),
                actual: actual.to_string(),
            });
        }
        Ok(out)
    }

    /// Reassemble the content of the most recent present version whose
    /// whole-file hash equals `hash`, if any is stored.
    ///
    /// The history store is content-addressed, so any version that ever held
    /// this exact content shares its chunks — this lets the sync session source
    /// apply bytes "by signature" from the CAS when neither the triggering
    /// frame nor the current disk content carries them (the multi-head apply
    /// case in docs/NOTES.md). Returns `Ok(None)` when no present version has
    /// that content.
    ///
    /// # Errors
    /// [`HistoryError::Sqlite`] on a query failure, or any [`get_content`]
    /// integrity error if the located version is corrupt.
    ///
    /// [`get_content`]: HistoryStore::get_content
    pub fn content_by_hash(&self, hash: &ContentHash) -> Result<Option<Vec<u8>>, HistoryError> {
        let id: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM versions WHERE state = 1 AND content_hash = ?1 \
                 ORDER BY id DESC LIMIT 1",
                params![hash.0.as_slice()],
                |row| row.get(0),
            )
            .optional()?;
        match id {
            Some(id) => Ok(Some(self.get_content(VersionId(id))?)),
            None => Ok(None),
        }
    }

    /// Recorded conflicts, oldest first. With `unresolved_only`, only rows whose
    /// `resolved` flag is unset are returned.
    ///
    /// # Errors
    /// [`HistoryError::Sqlite`] on a query failure, or
    /// [`HistoryError::Malformed`] if a stored path cannot be decoded.
    pub fn conflicts(&self, unresolved_only: bool) -> Result<Vec<ConflictRecord>, HistoryError> {
        let sql = if unresolved_only {
            "SELECT id, path, winner_version, loser_version, wall_ms, resolved \
             FROM conflicts WHERE resolved = 0 ORDER BY id"
        } else {
            "SELECT id, path, winner_version, loser_version, wall_ms, resolved \
             FROM conflicts ORDER BY id"
        };
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, path, winner, loser, wall_ms, resolved) = row?;
            let path = RelPath::new(&path).map_err(|e| {
                HistoryError::Malformed(format!("stored conflict path {path:?}: {e}"))
            })?;
            out.push(ConflictRecord {
                id: ConflictId(id),
                path,
                winner: VersionId(winner),
                loser: VersionId(loser),
                wall_ms: from_sql_int(wall_ms),
                resolved: resolved != 0,
            });
        }
        Ok(out)
    }

    /// Verify the store's integrity: `SQLite`'s own `quick_check`, that every
    /// present version's manifest chunks exist and sum to its recorded size, and
    /// that every chunk decompresses to content whose BLAKE3 matches its key.
    ///
    /// Problems are collected into [`CheckReport::issues`] rather than aborting,
    /// so one bad chunk does not hide others. The report is `ok` iff no issues
    /// were found.
    ///
    /// # Errors
    /// [`HistoryError::Sqlite`] only if the database cannot be queried at all;
    /// data-level problems are reported, not raised.
    pub fn check(&self) -> Result<CheckReport, HistoryError> {
        let mut issues = Vec::new();

        let quick: String = self
            .conn
            .query_row("PRAGMA quick_check", [], |row| row.get(0))?;
        if quick != "ok" {
            issues.push(format!("sqlite quick_check: {quick}"));
        }

        // Every present version: manifest chunks exist and sizes are consistent.
        let mut versions_checked: u64 = 0;
        {
            let mut stmt = self
                .conn
                .prepare("SELECT id, size, manifest FROM versions WHERE state = 1")?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, Option<Vec<u8>>>(2)?,
                ))
            })?;
            for row in rows {
                versions_checked += 1;
                let (id, size, manifest) = row?;
                let Some(manifest) = manifest else {
                    issues.push(format!("version {id}: present but has no manifest"));
                    continue;
                };
                if manifest.len() % 32 != 0 {
                    issues.push(format!(
                        "version {id}: manifest length {} not a multiple of 32",
                        manifest.len()
                    ));
                    continue;
                }
                let mut summed: u64 = 0;
                for chunk_hash in manifest.chunks_exact(32) {
                    let chunk_size: Option<i64> = self
                        .conn
                        .query_row(
                            "SELECT size FROM chunks WHERE hash = ?1",
                            params![chunk_hash],
                            |r| r.get(0),
                        )
                        .optional()?;
                    match chunk_size {
                        None => {
                            issues.push(format!("version {id}: missing chunk {}", hex(chunk_hash)));
                        }
                        Some(cs) => summed = summed.saturating_add(from_sql_int(cs)),
                    }
                }
                if let Some(sz) = size {
                    if from_sql_int(sz) != summed {
                        issues.push(format!(
                            "version {id}: recorded size {sz} != sum of chunk sizes {summed}"
                        ));
                    }
                }
            }
        }

        // Every chunk: decompresses cleanly, correct length, BLAKE3 matches key.
        let mut chunks_checked: u64 = 0;
        {
            let mut stmt = self.conn.prepare("SELECT hash, size, zdata FROM chunks")?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                ))
            })?;
            for row in rows {
                chunks_checked += 1;
                let (hash, size, zdata) = row?;
                if let Err(e) = decompress_and_verify(&hash, from_sql_int(size), &zdata) {
                    issues.push(e.to_string());
                }
            }
        }

        Ok(CheckReport {
            versions_checked,
            chunks_checked,
            ok: issues.is_empty(),
            issues,
        })
    }
}

/// A raw `versions` row, decoded field-by-field before conversion to
/// [`VersionMeta`].
struct RawVersion {
    id: i64,
    state: i64,
    content_hash: Option<Vec<u8>>,
    size: Option<i64>,
    clock: Vec<u8>,
    replica: i64,
    wall_ms: i64,
    origin: i64,
    exec: i64,
}

impl RawVersion {
    /// Decode into a [`VersionMeta`], reconstructing the [`EntryState`] and the
    /// vector clock.
    fn into_meta(self) -> Result<VersionMeta, HistoryError> {
        let clock: VectorClock = postcard::from_bytes(&self.clock)?;
        let content_hash = match &self.content_hash {
            Some(bytes) => Some(ContentHash(to_hash32(bytes).ok_or_else(|| {
                HistoryError::Malformed(format!(
                    "version {} content_hash is {} bytes, expected 32",
                    self.id,
                    bytes.len()
                ))
            })?)),
            None => None,
        };
        let size = self.size.map(from_sql_int);
        let state = if self.state == 0 {
            EntryState::Tombstone
        } else {
            let hash = content_hash.ok_or_else(|| {
                HistoryError::Malformed(format!("present version {} has no hash", self.id))
            })?;
            let size = size.ok_or_else(|| {
                HistoryError::Malformed(format!("present version {} has no size", self.id))
            })?;
            EntryState::Present(ContentSig {
                hash,
                size,
                exec: self.exec != 0,
                // History does not persist mtime: it is carried metadata, not
                // part of a version's identity (restore rewrites bytes with a
                // fresh mtime, and the genesis adoption tiebreak only ever reads
                // live index sigs, never historical ones). `0` is harmless here.
                mtime_ms: 0,
            })
        };
        Ok(VersionMeta {
            id: VersionId(self.id),
            state,
            content_hash,
            size,
            clock,
            replica: ReplicaId(from_sql_int(self.replica)),
            wall_ms: from_sql_int(self.wall_ms),
            origin: Origin::from_i64(self.origin),
        })
    }
}

/// The `(state, content_hash, manifest)` projection [`HistoryStore::get_content`]
/// reads for one version.
type ContentRow = (i64, Option<Vec<u8>>, Option<Vec<u8>>);

/// The column values for a `versions` INSERT, prepared from an [`EntryState`].
struct PreparedVersion {
    /// `1` present, `0` tombstone.
    state_int: i64,
    /// Whole-file hash (present only).
    content_hash: Option<[u8; 32]>,
    /// File size in bytes (present only).
    size: Option<u64>,
    /// Concatenated chunk-hash manifest (present only).
    manifest: Option<Vec<u8>>,
    /// The Unix executable bit (present only; `false` for a tombstone).
    exec: bool,
}

/// The outcome of chunking and storing one blob of content.
struct StoredContent {
    /// Whole-file BLAKE3 hash — the content identity.
    hash: ContentHash,
    /// Concatenated 32-byte chunk hashes, in file order.
    manifest: Vec<u8>,
    /// New uncompressed chunk-bytes actually written (dedup metric).
    new_bytes: u64,
}

/// Split `bytes` into `FastCDC` chunks using the store's exact parameters,
/// returning each chunk's BLAKE3 [`ContentHash`] and its byte range in `bytes`.
///
/// This is the pure, no-I/O counterpart to what [`HistoryStore::store_content`]
/// persists: because the `FastCDC` parameters (16/64/256 KiB) and the BLAKE3
/// hashing are identical, `chunk_bytes(x)` yields exactly the chunk hashes, in
/// the same order, that storing `x` records in its manifest. The wire protocol
/// uses this to chunk large content for transfer without touching the database
/// (docs/SPEC.md §8), and dedup against the peer's CAS stays coherent.
///
/// ```
/// use tomo_history::chunk_bytes;
/// // A short input is a single chunk whose hash is the whole-file BLAKE3.
/// let bytes = b"tomo";
/// let chunks = chunk_bytes(bytes);
/// assert_eq!(chunks.len(), 1);
/// assert_eq!(chunks[0].0, tomo_engine::ContentHash(*blake3::hash(bytes).as_bytes()));
/// assert_eq!(chunks[0].1, 0..bytes.len());
/// ```
#[must_use]
pub fn chunk_bytes(bytes: &[u8]) -> Vec<(ContentHash, Range<usize>)> {
    let mut out = Vec::new();
    for chunk in fastcdc::v2020::FastCDC::new(bytes, CDC_MIN, CDC_AVG, CDC_MAX) {
        let range = chunk.offset..chunk.offset + chunk.length;
        let hash = ContentHash(*blake3::hash(&bytes[range.clone()]).as_bytes());
        out.push((hash, range));
    }
    out
}

/// Insert one version row inside an existing transaction, storing its content
/// (deduplicated) first, and return the version's id.
///
/// STORE-LEVEL IDEMPOTENCY (SEED-PERF Phase 2, bug B2): the insert is
/// `INSERT OR IGNORE`, so a version whose `(path, state, clock)` identity is
/// already present — a crash-retry double-record — does NOT create a duplicate
/// row; its existing id is looked up and returned instead. This makes both
/// [`HistoryStore::record_version`] and the batch API idempotent without relying
/// on the caller's `log()` check (that check remains as belt-and-suspenders and
/// to return an existing id for conflict rows). Content chunks are stored
/// regardless (they dedup at the chunk level via `INSERT OR IGNORE`), so a
/// re-record is cheap, not free — the cost is one dedup pass, never a new row.
#[allow(clippy::too_many_arguments)]
fn insert_version(
    tx: &Connection,
    path: &RelPath,
    state: &EntryState,
    clock: &VectorClock,
    replica: ReplicaId,
    origin: Origin,
    wall_ms: u64,
    bytes: Option<&[u8]>,
) -> Result<VersionId, HistoryError> {
    let clock_blob = postcard::to_allocvec(clock)?;
    let prepared = match state {
        EntryState::Present(sig) => {
            let bytes = bytes.ok_or_else(|| HistoryError::MissingContent { path: path.clone() })?;
            let stored = store_chunks(tx, bytes)?;
            verify_sig(path, sig, stored.hash, len_u64(bytes.len()))?;
            PreparedVersion {
                state_int: 1,
                content_hash: Some(stored.hash.0),
                size: Some(sig.size),
                manifest: Some(stored.manifest),
                exec: sig.exec,
            }
        }
        EntryState::Tombstone => PreparedVersion {
            state_int: 0,
            content_hash: None,
            size: None,
            manifest: None,
            exec: false,
        },
    };

    let inserted = tx.execute(
        "INSERT OR IGNORE INTO versions \
         (path, state, content_hash, size, manifest, clock, replica, wall_ms, origin, exec) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            path.as_str(),
            prepared.state_int,
            prepared.content_hash.as_ref().map(<[u8; 32]>::as_slice),
            prepared.size.map(to_sql_int),
            prepared.manifest,
            clock_blob,
            to_sql_int(replica.0),
            to_sql_int(wall_ms),
            origin.as_i64(),
            i64::from(prepared.exec),
        ],
    )?;
    if inserted == 1 {
        return Ok(VersionId(tx.last_insert_rowid()));
    }
    // Already present (the `versions_identity` unique index fired): return the
    // existing row's id rather than a fresh one, so callers see a stable id.
    let id: i64 = tx.query_row(
        "SELECT id FROM versions WHERE path = ?1 AND state = ?2 AND clock = ?3",
        params![path.as_str(), prepared.state_int, clock_blob],
        |row| row.get(0),
    )?;
    Ok(VersionId(id))
}

/// Chunk `bytes` with `FastCDC`, hash each chunk with BLAKE3, zstd-compress, and
/// `INSERT OR IGNORE` it into `chunks`. Returns the whole-file hash, the ordered
/// chunk-hash manifest, and the count of newly written (non-deduplicated)
/// uncompressed chunk-bytes.
fn store_chunks(conn: &Connection, bytes: &[u8]) -> Result<StoredContent, HistoryError> {
    let whole = ContentHash(*blake3::hash(bytes).as_bytes());
    let mut manifest = Vec::new();
    let mut new_bytes: u64 = 0;

    for chunk in fastcdc::v2020::FastCDC::new(bytes, CDC_MIN, CDC_AVG, CDC_MAX) {
        let slice = &bytes[chunk.offset..chunk.offset + chunk.length];
        let chunk_hash = blake3::hash(slice);
        let key = chunk_hash.as_bytes();
        manifest.extend_from_slice(key);

        let zdata = zstd::encode_all(slice, ZSTD_LEVEL)
            .map_err(|e| HistoryError::Malformed(format!("zstd compression failed: {e}")))?;
        let inserted = conn.execute(
            "INSERT OR IGNORE INTO chunks (hash, size, zdata) VALUES (?1, ?2, ?3)",
            params![key.as_slice(), to_sql_int(len_u64(slice.len())), zdata],
        )?;
        if inserted == 1 {
            new_bytes = new_bytes.saturating_add(len_u64(slice.len()));
        }
    }

    Ok(StoredContent {
        hash: whole,
        manifest,
        new_bytes,
    })
}

/// Decompress a chunk's `zdata` and verify it against its declared `size` and
/// key `hash`. Returns the plaintext on success.
fn decompress_and_verify(hash: &[u8], size: u64, zdata: &[u8]) -> Result<Vec<u8>, HistoryError> {
    let plain = zstd::decode_all(zdata)
        .map_err(|e| HistoryError::corrupt_chunk(hash, format!("decompression failed: {e}")))?;
    if len_u64(plain.len()) != size {
        return Err(HistoryError::corrupt_chunk(
            hash,
            format!("size {} != recorded {size}", plain.len()),
        ));
    }
    let actual = blake3::hash(&plain);
    if actual.as_bytes().as_slice() != hash {
        return Err(HistoryError::corrupt_chunk(
            hash,
            format!("content hashes to {}", actual.to_hex()),
        ));
    }
    Ok(plain)
}

/// Verify that `bytes` (already stored, `actual_hash`) match the declared
/// [`ContentSig`] for `path`.
fn verify_sig(
    path: &RelPath,
    sig: &ContentSig,
    actual_hash: ContentHash,
    actual_size: u64,
) -> Result<(), HistoryError> {
    if actual_hash != sig.hash || actual_size != sig.size {
        return Err(HistoryError::SigMismatch {
            path: path.clone(),
            declared: sig.hash.to_string(),
            actual: actual_hash.to_string(),
        });
    }
    Ok(())
}

/// Interpret a stored blob as a 32-byte hash, or `None` if the length is wrong.
fn to_hash32(bytes: &[u8]) -> Option<[u8; 32]> {
    <[u8; 32]>::try_from(bytes).ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// A deterministic pseudorandom byte vector (tiny xorshift, no `rand`).
    fn pseudorandom(len: usize, seed: u64) -> Vec<u8> {
        let mut state = seed | 1;
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            out.extend_from_slice(&state.to_le_bytes());
        }
        out.truncate(len);
        out
    }

    /// Record a present version of `path` with the given `bytes` in `store`,
    /// ticking an ADVANCING clock for `replica` (one more tick than the path's
    /// existing version count) so successive versions of the same path carry
    /// distinct `(path, clock)` identities — matching how the engine bumps the
    /// clock per change, and required now that the store dedups on that identity
    /// (the v3 `versions_identity` index). A tiny fixture for query tests.
    fn record(store: &mut HistoryStore, path: &str, bytes: &[u8], replica: u64) -> VersionId {
        let rel = RelPath::new(path).unwrap();
        let n = u32::try_from(store.log(&rel).unwrap().len()).unwrap() + 1;
        let (hash, _) = store.store_content(bytes).unwrap();
        let mut clock = VectorClock::new();
        for _ in 0..n {
            clock.tick(ReplicaId(replica));
        }
        let sig = ContentSig {
            hash,
            size: bytes.len() as u64,
            exec: false,
            mtime_ms: 0,
        };
        store
            .record_version(
                &rel,
                &EntryState::Present(sig),
                &clock,
                ReplicaId(replica),
                Origin::Local,
                0,
                Some(bytes),
            )
            .unwrap()
    }

    /// Read `PRAGMA user_version` from a store's connection (migration tests).
    fn user_version(store: &HistoryStore) -> i64 {
        store
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap()
    }

    /// Count the rows in the `chunks` table (the CAS dedup metric — SEED-PERF H8).
    fn count_chunks(store: &HistoryStore) -> i64 {
        store
            .conn
            .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))
            .unwrap()
    }

    /// A present [`ContentSig`] for `bytes` (mtime is not identity, and the store
    /// never persists it — it reads back as `0`, so `0` here round-trips).
    fn present_sig(bytes: &[u8]) -> ContentSig {
        ContentSig {
            hash: ContentHash(*blake3::hash(bytes).as_bytes()),
            size: bytes.len() as u64,
            exec: false,
            mtime_ms: 0,
        }
    }

    /// A clock ticked `n` times for `replica` — a stable "version identity".
    fn clock_ticked(replica: u64, n: u32) -> VectorClock {
        let mut c = VectorClock::new();
        for _ in 0..n {
            c.tick(ReplicaId(replica));
        }
        c
    }

    /// Ingest one present version of `path` with caller-side idempotency: skip
    /// when a version with the SAME `(clock, state)` is already logged, else
    /// record it. This mirrors the session's `find_or_record` guard — the exact
    /// contract SEED-PERF Phase 2's batch-ingest API must preserve, since the
    /// store itself does not dedup version rows. Returns whether a row was added.
    fn ingest_dedup(
        store: &mut HistoryStore,
        path: &str,
        bytes: &[u8],
        clock: &VectorClock,
    ) -> bool {
        let rel = RelPath::new(path).unwrap();
        let sig = present_sig(bytes);
        let state = EntryState::Present(sig);
        for m in store.log(&rel).unwrap() {
            if m.clock == *clock && m.state == state {
                return false; // identical version already stored — idempotent skip
            }
        }
        store
            .record_version(
                &rel,
                &state,
                clock,
                ReplicaId(1),
                Origin::Local,
                0,
                Some(bytes),
            )
            .unwrap();
        true
    }

    #[test]
    fn exec_bit_round_trips_through_record_and_log() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = HistoryStore::open(dir.path()).unwrap();
        let rel = RelPath::new("build.sh").unwrap();
        let bytes = b"#!/bin/sh\necho hi\n";
        let (hash, _) = store.store_content(bytes).unwrap();
        let mut clock = VectorClock::new();
        clock.tick(ReplicaId(1));
        // Record an EXECUTABLE version.
        let exec_sig = ContentSig {
            hash,
            size: bytes.len() as u64,
            exec: true,
            mtime_ms: 0,
        };
        store
            .record_version(
                &rel,
                &EntryState::Present(exec_sig),
                &clock,
                ReplicaId(1),
                Origin::Local,
                0,
                Some(bytes),
            )
            .unwrap();
        // …then a non-executable version of the SAME bytes (a chmod -x).
        clock.tick(ReplicaId(1));
        let plain_sig = ContentSig {
            exec: false,
            ..exec_sig
        };
        store
            .record_version(
                &rel,
                &EntryState::Present(plain_sig),
                &clock,
                ReplicaId(1),
                Origin::Local,
                1,
                Some(bytes),
            )
            .unwrap();

        let versions = store.log(&rel).unwrap();
        assert_eq!(versions.len(), 2);
        // Newest first: the chmod -x (non-exec), then the executable one.
        assert_eq!(versions[0].state, EntryState::Present(plain_sig));
        assert_eq!(versions[1].state, EntryState::Present(exec_sig));
        assert!(matches!(versions[1].state, EntryState::Present(s) if s.exec));
        assert!(matches!(versions[0].state, EntryState::Present(s) if !s.exec));
    }

    #[test]
    fn migrates_a_v1_database_by_adding_the_exec_column() {
        // Build a raw v1 database (no `exec` column) by hand, exactly as an
        // older Tomo would have written it, then open it through HistoryStore
        // and confirm the migration adds the column, bumps user_version to 2,
        // and old rows read back as non-executable.
        let dir = tempfile::tempdir().unwrap();
        let db_dir = dir.path().join(".tomo").join("db");
        std::fs::create_dir_all(&db_dir).unwrap();
        let db_path = db_dir.join("history.sqlite");
        {
            let conn = Connection::open(&db_path).unwrap();
            // The exact pre-v2 `versions` DDL (no exec column).
            conn.execute_batch(
                "CREATE TABLE versions (\n\
                 id INTEGER PRIMARY KEY AUTOINCREMENT, path TEXT NOT NULL, state INTEGER NOT NULL,\n\
                 content_hash BLOB, size INTEGER, manifest BLOB, clock BLOB NOT NULL,\n\
                 replica INTEGER NOT NULL, wall_ms INTEGER NOT NULL, origin INTEGER NOT NULL);",
            )
            .unwrap();
            let clock = postcard::to_allocvec(&{
                let mut c = VectorClock::new();
                c.tick(ReplicaId(1));
                c
            })
            .unwrap();
            // A tombstone row (no content needed) written under the v1 schema.
            conn.execute(
                "INSERT INTO versions (path, state, content_hash, size, manifest, clock, replica, wall_ms, origin) \
                 VALUES ('legacy.txt', 0, NULL, NULL, NULL, ?1, 1, 0, 0)",
                params![clock],
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 1i64).unwrap();
        }

        // Open through the store: migration runs on open (v1 → v2 → v3).
        let store = HistoryStore::open(dir.path()).unwrap();
        assert_eq!(user_version(&store), 3, "user_version bumped to 3");
        assert!(
            HistoryStore::has_column(&store.conn, "versions", "exec").unwrap(),
            "exec column added by migration"
        );
        assert!(
            HistoryStore::has_index(&store.conn, "versions_identity").unwrap(),
            "v3 identity index added by migration"
        );
        // The pre-existing v1 row still reads (as a non-executable tombstone).
        let versions = store.log(&RelPath::new("legacy.txt").unwrap()).unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].state, EntryState::Tombstone);
    }

    #[test]
    fn read_only_open_of_a_v1_database_still_logs() {
        // A read-only handle never migrates, so it must tolerate a v1 database
        // that still lacks the exec column: `log` substitutes a constant and
        // reads old rows back as non-executable rather than failing at prepare.
        let dir = tempfile::tempdir().unwrap();
        let db_dir = dir.path().join(".tomo").join("db");
        std::fs::create_dir_all(&db_dir).unwrap();
        {
            let conn = Connection::open(db_dir.join("history.sqlite")).unwrap();
            conn.execute_batch(
                "CREATE TABLE versions (\n\
                 id INTEGER PRIMARY KEY AUTOINCREMENT, path TEXT NOT NULL, state INTEGER NOT NULL,\n\
                 content_hash BLOB, size INTEGER, manifest BLOB, clock BLOB NOT NULL,\n\
                 replica INTEGER NOT NULL, wall_ms INTEGER NOT NULL, origin INTEGER NOT NULL);",
            )
            .unwrap();
            let clock = postcard::to_allocvec(&{
                let mut c = VectorClock::new();
                c.tick(ReplicaId(1));
                c
            })
            .unwrap();
            conn.execute(
                "INSERT INTO versions (path, state, content_hash, size, manifest, clock, replica, wall_ms, origin) \
                 VALUES ('old.txt', 0, NULL, NULL, NULL, ?1, 1, 0, 0)",
                params![clock],
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 1i64).unwrap();
        }

        let store = HistoryStore::open_readonly(dir.path()).unwrap().unwrap();
        assert!(
            !store.has_exec,
            "read-only v1 open detects the missing column"
        );
        let versions = store.log(&RelPath::new("old.txt").unwrap()).unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].state, EntryState::Tombstone);
        // `recent` (also selects exec) works too.
        assert_eq!(store.recent(10).unwrap().len(), 1);
    }

    #[test]
    fn migrate_is_idempotent_on_a_current_database() {
        // Opening a fresh (already-current) database twice must not error on the
        // ALTER (column exists) or the CREATE UNIQUE INDEX (index exists), and
        // must keep user_version at the current version.
        let dir = tempfile::tempdir().unwrap();
        {
            let store = HistoryStore::open(dir.path()).unwrap();
            assert_eq!(user_version(&store), SCHEMA_VERSION);
        }
        let store = HistoryStore::open(dir.path()).unwrap();
        assert_eq!(user_version(&store), SCHEMA_VERSION);
        assert!(HistoryStore::has_column(&store.conn, "versions", "exec").unwrap());
        assert!(HistoryStore::has_index(&store.conn, "versions_identity").unwrap());
    }

    #[test]
    fn migrates_a_v2_database_with_duplicate_rows_to_the_v3_identity_index() {
        // Build a raw v2 database (exec column present, NO identity index) holding
        // DUPLICATE (path, state, clock) version rows — exactly what the pre-fix
        // non-idempotent crash-retry (bug B2) produced — plus a conflict row that
        // references one of the soon-to-be-deleted duplicates. Opening through the
        // store must: dedup the rows, repoint the conflict onto the surviving id,
        // create the unique index, and bump user_version to 3 — an old-schema DB
        // opens cleanly and GAINS the index (the additive-migration requirement).
        let dir = tempfile::tempdir().unwrap();
        let db_dir = dir.path().join(".tomo").join("db");
        std::fs::create_dir_all(&db_dir).unwrap();
        let db_path = db_dir.join("history.sqlite");
        let clock_blob = postcard::to_allocvec(&clock_ticked(1, 1)).unwrap();
        {
            let conn = Connection::open(&db_path).unwrap();
            // The exact v2 DDL (exec column, no unique index).
            conn.execute_batch(
                "CREATE TABLE versions (\n\
                 id INTEGER PRIMARY KEY AUTOINCREMENT, path TEXT NOT NULL, state INTEGER NOT NULL,\n\
                 content_hash BLOB, size INTEGER, manifest BLOB, clock BLOB NOT NULL,\n\
                 replica INTEGER NOT NULL, wall_ms INTEGER NOT NULL, origin INTEGER NOT NULL,\n\
                 exec INTEGER NOT NULL DEFAULT 0);\n\
                 CREATE TABLE conflicts (\n\
                 id INTEGER PRIMARY KEY AUTOINCREMENT, path TEXT NOT NULL,\n\
                 winner_version INTEGER NOT NULL, loser_version INTEGER NOT NULL,\n\
                 wall_ms INTEGER NOT NULL, resolved INTEGER NOT NULL DEFAULT 0);",
            )
            .unwrap();
            // THREE identical (path, state, clock) tombstone rows: ids 1, 2, 3.
            for _ in 0..3 {
                conn.execute(
                    "INSERT INTO versions (path, state, content_hash, size, manifest, clock, replica, wall_ms, origin, exec) \
                     VALUES ('dup.txt', 0, NULL, NULL, NULL, ?1, 1, 0, 0, 0)",
                    params![clock_blob],
                )
                .unwrap();
            }
            // A distinct row (id 4) and a conflict referencing a DUPLICATE (id 3)
            // as winner and the distinct row (id 4) as loser.
            conn.execute(
                "INSERT INTO versions (path, state, content_hash, size, manifest, clock, replica, wall_ms, origin, exec) \
                 VALUES ('other.txt', 0, NULL, NULL, NULL, ?1, 2, 0, 0, 0)",
                params![postcard::to_allocvec(&clock_ticked(2, 1)).unwrap()],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO conflicts (path, winner_version, loser_version, wall_ms) VALUES ('dup.txt', 3, 4, 0)",
                [],
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 2i64).unwrap();
        }

        // Open through the store: the v2 → v3 migration runs.
        let store = HistoryStore::open(dir.path()).unwrap();
        assert_eq!(user_version(&store), 3, "user_version bumped to 3");
        assert!(HistoryStore::has_index(&store.conn, "versions_identity").unwrap());
        // The three duplicate rows collapsed to exactly one (the min id, 1).
        let log = store.log(&RelPath::new("dup.txt").unwrap()).unwrap();
        assert_eq!(log.len(), 1, "duplicates collapsed to one row");
        assert_eq!(log[0].id, VersionId(1), "the lowest id survives");
        // The conflict was repointed from the deleted duplicate (3) onto the
        // surviving canonical id (1), so it still resolves.
        let conflicts = store.conflicts(false).unwrap();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(
            conflicts[0].winner,
            VersionId(1),
            "winner repointed to survivor"
        );
        assert_eq!(conflicts[0].loser, VersionId(4));
        assert!(
            store.check().unwrap().ok,
            "db check green after v3 migration"
        );
        // A fresh re-record of the same identity is now a no-op (index enforced).
        let mut store = HistoryStore::open(dir.path()).unwrap();
        store
            .record_version(
                &RelPath::new("dup.txt").unwrap(),
                &EntryState::Tombstone,
                &clock_ticked(1, 1),
                ReplicaId(1),
                Origin::Local,
                0,
                None,
            )
            .unwrap();
        assert_eq!(
            store.log(&RelPath::new("dup.txt").unwrap()).unwrap().len(),
            1
        );
    }

    #[test]
    fn recent_returns_newest_first_across_paths_and_respects_limit() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = HistoryStore::open(dir.path()).unwrap();

        // Interleave two paths; ids increase monotonically with insertion.
        record(&mut store, "a.txt", b"a1", 1);
        record(&mut store, "b.txt", b"b1", 2);
        record(&mut store, "a.txt", b"a2", 1);
        let last = record(&mut store, "b.txt", b"b2", 2);

        let all = store.recent(10).unwrap();
        assert_eq!(all.len(), 4, "every version is listed");
        // Newest first: the last insert leads, ids strictly descending.
        assert_eq!(all[0].1.id, last);
        assert_eq!(all[0].0.as_str(), "b.txt");
        for pair in all.windows(2) {
            assert!(pair[0].1.id > pair[1].1.id, "ids must descend");
        }

        // Limit caps the result to the newest N.
        let two = store.recent(2).unwrap();
        assert_eq!(two.len(), 2);
        assert_eq!(two[0].1.id, last);
        assert!(two[0].1.id > two[1].1.id);
    }

    #[test]
    fn recent_is_empty_on_a_fresh_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = HistoryStore::open(dir.path()).unwrap();
        assert!(store.recent(20).unwrap().is_empty());
    }

    #[test]
    fn history_paths_groups_by_path_newest_first_with_counts() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = HistoryStore::open(dir.path()).unwrap();

        // a.txt gets three versions, b.txt one; the last write is to a.txt.
        record(&mut store, "a.txt", b"a1", 1);
        record(&mut store, "b.txt", b"b1", 2);
        record(&mut store, "a.txt", b"a2", 1);
        let last_a = record(&mut store, "a.txt", b"a3", 1);

        let paths = store.history_paths(10).unwrap();
        assert_eq!(paths.len(), 2, "one row per distinct path");
        // a.txt has the newest version (last insert) → it leads.
        assert_eq!(paths[0].path.as_str(), "a.txt");
        assert_eq!(paths[0].versions, 3);
        assert_eq!(paths[0].last_version, last_a);
        assert_eq!(paths[1].path.as_str(), "b.txt");
        assert_eq!(paths[1].versions, 1);
    }

    #[test]
    fn history_paths_respects_limit_and_is_empty_when_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = HistoryStore::open(dir.path()).unwrap();
        assert!(store.history_paths(5).unwrap().is_empty());
        record(&mut store, "a.txt", b"a", 1);
        record(&mut store, "b.txt", b"b", 1);
        record(&mut store, "c.txt", b"c", 1);
        let two = store.history_paths(2).unwrap();
        assert_eq!(two.len(), 2, "limit caps the distinct-path rows");
        assert_eq!(two[0].path.as_str(), "c.txt", "newest path first");
    }

    #[test]
    fn mark_conflict_unresolved_inverts_resolved() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = HistoryStore::open(dir.path()).unwrap();
        let a = record(&mut store, "clash.txt", b"aaa", 1);
        let b = record(&mut store, "clash.txt", b"bbb", 2);
        let id = store
            .record_conflict(&RelPath::new("clash.txt").unwrap(), a, b, 0)
            .unwrap();

        // Resolve, then unresolve, checking the unresolved set both times.
        assert!(store.mark_conflict_resolved(id).unwrap());
        assert!(store.conflicts(true).unwrap().is_empty());
        assert!(store.mark_conflict_unresolved(id).unwrap(), "flipped back");
        assert_eq!(store.conflicts(true).unwrap().len(), 1, "reappears");
        // Idempotent: a second unresolve is a no-op (already unresolved).
        assert!(!store.mark_conflict_unresolved(id).unwrap());
    }

    /// The public `chunk_bytes` must cut and hash exactly as `store_chunks`
    /// persists: identical `FastCDC` params and BLAKE3 ⟹ identical chunk ids in
    /// identical order. The wire protocol relies on this to stay CAS-coherent.
    #[test]
    fn chunk_bytes_ids_match_stored_manifest() {
        let conn = Connection::open_in_memory().unwrap();
        HistoryStore::migrate(&conn).unwrap();

        for (len, seed) in [(37usize, 1u64), (100 * 1024, 7), (5 * 1024 * 1024, 0x51ED)] {
            let data = pseudorandom(len, seed);

            let pure: Vec<[u8; 32]> = chunk_bytes(&data).into_iter().map(|(h, _)| h.0).collect();
            let stored = store_chunks(&conn, &data).unwrap();
            let persisted: Vec<[u8; 32]> = stored
                .manifest
                .chunks_exact(32)
                .map(|c| <[u8; 32]>::try_from(c).unwrap())
                .collect();

            assert_eq!(pure, persisted, "len {len}: chunk id streams diverged");
            // And the ranges reconstruct the input in order.
            let reassembled: Vec<u8> = chunk_bytes(&data)
                .into_iter()
                .flat_map(|(_, r)| data[r].to_vec())
                .collect();
            assert_eq!(reassembled, data, "len {len}: ranges do not tile the input");
        }
    }

    // ---- SEED-PERF H8: history ingest contract ----------------------------
    //
    // These pin today's SINGLE-ingest semantics as the contract SEED-PERF
    // Phase 2's batch-ingest API (grouping N CAS inserts into one transaction)
    // must match: identical CAS addresses, identical dedup, integrity green
    // across a mid-batch abort, and re-ingest-after-abort idempotency (pairs
    // with H2's crash-replay). Ordering authority is the rowid, never wall time.

    #[test]
    fn h8_identical_content_dedups_chunks_across_versions_and_paths() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = HistoryStore::open(dir.path()).unwrap();
        // Multi-chunk content so dedup spans several chunk rows.
        let data = pseudorandom(300 * 1024, 0xABCD);

        let (_h, new1) = store.store_content(&data).unwrap();
        assert!(new1 > 0, "first store writes new chunk bytes");
        let chunks_after_first = count_chunks(&store);
        assert!(chunks_after_first > 1, "content spans multiple chunks");

        // Re-storing identical bytes writes ZERO new bytes and no new rows.
        let (_h2, new2) = store.store_content(&data).unwrap();
        assert_eq!(new2, 0, "identical content adds no new chunk bytes");
        assert_eq!(
            count_chunks(&store),
            chunks_after_first,
            "identical content adds no new chunk rows"
        );

        // The SAME bytes recorded under two DIFFERENT paths still share chunks:
        // dedup is content-addressed and path-independent.
        record(&mut store, "p1.bin", &data, 1);
        record(&mut store, "p2.bin", &data, 2);
        assert_eq!(
            count_chunks(&store),
            chunks_after_first,
            "two paths with identical bytes share every chunk"
        );
    }

    #[test]
    fn h8_raw_record_version_is_store_level_idempotent_after_the_b2_fix() {
        // FLIPPED PIN (SEED-PERF Phase 2, bug B2): the store's `record_version`
        // is now idempotent at the STORE level. The v3 `versions_identity` unique
        // index (`INSERT OR IGNORE`) means re-ingesting an identical version
        // (same path, content, AND clock) via the raw API does NOT append a
        // second row — the duplicate is ignored and the EXISTING id is returned.
        // Version-row idempotency is no longer merely a caller contract; a
        // crash-retry double-record (H2) cannot accrue duplicate versions.
        // (The pre-fix version of this test asserted len==2; it existed to be
        // flipped once B2 landed.)
        let dir = tempfile::tempdir().unwrap();
        let mut store = HistoryStore::open(dir.path()).unwrap();
        let rel = RelPath::new("dup.txt").unwrap();
        let bytes = b"same-bytes";
        let sig = present_sig(bytes);
        let clock = clock_ticked(1, 1);

        let mut ids = Vec::new();
        for _ in 0..3 {
            ids.push(
                store
                    .record_version(
                        &rel,
                        &EntryState::Present(sig),
                        &clock,
                        ReplicaId(1),
                        Origin::Local,
                        0,
                        Some(bytes),
                    )
                    .unwrap(),
            );
        }
        assert_eq!(
            store.log(&rel).unwrap().len(),
            1,
            "raw record_version is now store-level idempotent — one row per identity"
        );
        assert!(
            ids.iter().all(|id| *id == ids[0]),
            "every re-record returns the SAME (existing) id"
        );
        // The content still deduped at the chunk level (single small chunk).
        assert_eq!(
            count_chunks(&store),
            1,
            "identical bytes share one chunk row"
        );
    }

    #[test]
    fn h8_batch_ingest_equals_n_single_ingests() {
        // SEED-PERF H8 (the batch-equivalence test the plan promised): a batched
        // `record_versions` of N versions is EXACTLY equivalent to N single
        // `record_version` calls — same content addresses, same chunk dedup, same
        // resulting rows — and, thanks to the v3 index, the same idempotency.
        let batch: Vec<(String, Vec<u8>)> = (0..8)
            .map(|i| (format!("p{i}.bin"), pseudorandom(50 * 1024, 7 * i + 1)))
            .collect();
        let clock = clock_ticked(1, 1);

        // Reference store: N single ingests.
        let ref_dir = tempfile::tempdir().unwrap();
        let mut ref_store = HistoryStore::open(ref_dir.path()).unwrap();
        for (path, bytes) in &batch {
            ref_store
                .record_version(
                    &RelPath::new(path).unwrap(),
                    &EntryState::Present(present_sig(bytes)),
                    &clock,
                    ReplicaId(1),
                    Origin::Remote,
                    0,
                    Some(bytes),
                )
                .unwrap();
        }

        // Batch store: ONE `record_versions` of the same N.
        let batch_dir = tempfile::tempdir().unwrap();
        let mut batch_store = HistoryStore::open(batch_dir.path()).unwrap();
        let rels: Vec<RelPath> = batch
            .iter()
            .map(|(p, _)| RelPath::new(p).unwrap())
            .collect();
        let states: Vec<EntryState> = batch
            .iter()
            .map(|(_, b)| EntryState::Present(present_sig(b)))
            .collect();
        let records: Vec<VersionRecord> = (0..batch.len())
            .map(|i| VersionRecord {
                path: &rels[i],
                state: &states[i],
                clock: &clock,
                replica: ReplicaId(1),
                origin: Origin::Remote,
                wall_ms: 0,
                bytes: Some(&batch[i].1),
            })
            .collect();
        batch_store.record_versions(&records).unwrap();

        // Equivalence: identical per-path version metadata and identical chunk
        // population.
        assert_eq!(count_chunks(&batch_store), count_chunks(&ref_store));
        for (path, _bytes) in &batch {
            let rel = RelPath::new(path).unwrap();
            let bv = batch_store.log(&rel).unwrap();
            let rv = ref_store.log(&rel).unwrap();
            assert_eq!(bv.len(), 1);
            assert_eq!(rv.len(), 1);
            assert_eq!(bv[0].state, rv[0].state, "same state/sig for {path}");
            assert_eq!(bv[0].content_hash, rv[0].content_hash, "same address");
        }

        // Idempotent replay of the whole batch changes nothing (store-level).
        batch_store.record_versions(&records).unwrap();
        for (path, _) in &batch {
            assert_eq!(
                batch_store.log(&RelPath::new(path).unwrap()).unwrap().len(),
                1,
                "batch replay is idempotent"
            );
        }
        assert!(
            batch_store.check().unwrap().ok,
            "db check green after replay"
        );
    }

    #[test]
    fn h8_log_check_makes_reingest_idempotent_with_stable_version_count() {
        // The contract Phase 2 must match: guarding each ingest with a `log()`
        // lookup for a matching (clock, state) makes re-ingesting an identical
        // version idempotent — no duplicate rows, stable count. The store
        // furnishes exactly the observability this needs.
        let dir = tempfile::tempdir().unwrap();
        let mut store = HistoryStore::open(dir.path()).unwrap();
        let clock = clock_ticked(1, 1);

        assert!(
            ingest_dedup(&mut store, "f.txt", b"v1", &clock),
            "first ingest records"
        );
        assert!(
            !ingest_dedup(&mut store, "f.txt", b"v1", &clock),
            "identical re-ingest is skipped"
        );
        assert!(
            !ingest_dedup(&mut store, "f.txt", b"v1", &clock),
            "still idempotent on a third pass"
        );
        assert_eq!(
            store.log(&RelPath::new("f.txt").unwrap()).unwrap().len(),
            1,
            "version count is stable across re-ingests"
        );

        // A genuinely NEW version (advanced clock) is not swallowed.
        assert!(ingest_dedup(
            &mut store,
            "f.txt",
            b"v2",
            &clock_ticked(1, 2)
        ));
        assert_eq!(store.log(&RelPath::new("f.txt").unwrap()).unwrap().len(), 2);
    }

    #[test]
    fn h8_interrupted_ingest_reopens_clean_and_reingest_is_idempotent() {
        // Simulate a crash mid-batch by DROPPING the store between inserts and
        // reopening it (a fresh Connection, as a restart would). Integrity must
        // be green across the boundary, and a guarded re-ingest of the whole
        // batch is idempotent — no duplicate versions (SEED-PERF H8, pairs with
        // H2's "a crash between batch boundaries must re-ingest idempotently").
        let dir = tempfile::tempdir().unwrap();
        let batch: Vec<(String, Vec<u8>)> = (0..6)
            .map(|i| (format!("b{i}.bin"), pseudorandom(40 * 1024, 100 + i)))
            .collect();
        let clock = clock_ticked(1, 1);

        // Ingest the first half, then DROP the store (kill -9 between batches).
        {
            let mut store = HistoryStore::open(dir.path()).unwrap();
            for (path, bytes) in &batch[..3] {
                ingest_dedup(&mut store, path, bytes, &clock);
            }
        } // <- store dropped: simulated crash

        // Reopen: integrity intact after the interrupted sequence.
        {
            let store = HistoryStore::open(dir.path()).unwrap();
            assert!(
                store.check().unwrap().ok,
                "db check green after a mid-batch drop"
            );
        }

        // Re-ingest the WHOLE batch under a fresh handle: the first three dedup
        // (already present), the rest are added — total is exactly the batch.
        {
            let mut store = HistoryStore::open(dir.path()).unwrap();
            for (path, bytes) in &batch {
                ingest_dedup(&mut store, path, bytes, &clock);
            }
            for (path, _) in &batch {
                assert_eq!(
                    store.log(&RelPath::new(path).unwrap()).unwrap().len(),
                    1,
                    "each path has exactly one version after replay"
                );
            }
            // Replaying the batch AGAIN changes nothing (fully idempotent).
            for (path, bytes) in &batch {
                assert!(!ingest_dedup(&mut store, path, bytes, &clock));
            }
            assert!(
                store.check().unwrap().ok,
                "db check green after idempotent replay"
            );
        }
    }

    #[test]
    fn h8_version_order_is_by_rowid_never_wall_time() {
        // Invariant #7: ordering authority is the rowid (insertion order), never
        // `wall_ms` (display only). Record three versions of one path with
        // strictly DECREASING wall_ms and assert `log()` still returns them
        // newest-INSERTED first (ids descending), independent of wall time.
        let dir = tempfile::tempdir().unwrap();
        let mut store = HistoryStore::open(dir.path()).unwrap();
        let rel = RelPath::new("t.txt").unwrap();

        for (n, wall) in [(1u32, 3000u64), (2, 2000), (3, 1000)] {
            let bytes = vec![u8::try_from(n).unwrap(); 128];
            store
                .record_version(
                    &rel,
                    &EntryState::Present(present_sig(&bytes)),
                    &clock_ticked(1, n),
                    ReplicaId(1),
                    Origin::Local,
                    wall,
                    Some(&bytes),
                )
                .unwrap();
        }

        let log = store.log(&rel).unwrap();
        assert_eq!(log.len(), 3);
        // Newest-first is by rowid: the LAST inserted (smallest wall_ms) leads.
        assert_eq!(
            log[0].wall_ms, 1000,
            "last inserted leads despite smallest wall"
        );
        assert_eq!(log[1].wall_ms, 2000);
        assert_eq!(
            log[2].wall_ms, 3000,
            "first inserted trails despite largest wall"
        );
        for w in log.windows(2) {
            assert!(w[0].id > w[1].id, "ordering is strictly descending rowid");
        }
    }
}
