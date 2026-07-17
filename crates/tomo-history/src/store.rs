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
const SCHEMA_VERSION: i64 = 1;

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
        Ok(Self { conn })
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
        Ok(Some(Self { conn }))
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

    /// Create the schema idempotently and stamp `user_version`.
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
             \x20 origin       INTEGER NOT NULL\n\
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
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(())
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
        let clock_blob = postcard::to_allocvec(clock)?;
        let tx = self.conn.transaction()?;

        let prepared = match state {
            EntryState::Present(sig) => {
                let bytes =
                    bytes.ok_or_else(|| HistoryError::MissingContent { path: path.clone() })?;
                let stored = store_chunks(&tx, bytes)?;
                verify_sig(path, sig, stored.hash, len_u64(bytes.len()))?;
                PreparedVersion {
                    state_int: 1,
                    content_hash: Some(stored.hash.0),
                    size: Some(sig.size),
                    manifest: Some(stored.manifest),
                }
            }
            EntryState::Tombstone => PreparedVersion {
                state_int: 0,
                content_hash: None,
                size: None,
                manifest: None,
            },
        };

        tx.execute(
            "INSERT INTO versions \
             (path, state, content_hash, size, manifest, clock, replica, wall_ms, origin) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
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
            ],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(VersionId(id))
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

    /// All recorded versions of `path`, newest first.
    ///
    /// # Errors
    /// [`HistoryError::Sqlite`] on a query failure, or [`HistoryError::Clock`]
    /// / [`HistoryError::Malformed`] if a stored row cannot be decoded.
    pub fn log(&self, path: &RelPath) -> Result<Vec<VersionMeta>, HistoryError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, state, content_hash, size, clock, replica, wall_ms, origin \
             FROM versions WHERE path = ?1 ORDER BY id DESC",
        )?;
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
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?.into_meta()?);
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
            EntryState::Present(ContentSig { hash, size })
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
