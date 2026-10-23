//! Data model of the status database: entries, conflicts, and statistics.

/// Lifecycle state of a synchronized path, stored in `entries.state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryState {
    /// Local and remote are in sync (last known state matches both sides).
    Synced,
    /// An upload to the server is in progress or queued.
    Uploading,
    /// A download from the server is in progress or queued.
    Downloading,
    /// Both sides changed; waiting for an explicit resolution.
    Conflict,
    /// The last transfer or check failed; needs attention or retry.
    Error,
}

impl EntryState {
    /// Text stored in the `state` column.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Synced => "synced",
            Self::Uploading => "uploading",
            Self::Downloading => "downloading",
            Self::Conflict => "conflict",
            Self::Error => "error",
        }
    }

    /// Parses the `state` column; unknown values are an error.
    pub fn from_db(value: &str) -> Result<Self, crate::DbError> {
        match value {
            "synced" => Ok(Self::Synced),
            "uploading" => Ok(Self::Uploading),
            "downloading" => Ok(Self::Downloading),
            "conflict" => Ok(Self::Conflict),
            "error" => Ok(Self::Error),
            other => Err(crate::DbError::InvalidValue {
                column: "state",
                value: other.to_owned(),
            }),
        }
    }
}

/// Values of a path to insert or update in `entries` (the `path` column is
/// the unique key; the row id is assigned by SQLite).
#[derive(Debug, Clone)]
pub struct NewEntry {
    /// Relative path of the file or folder inside the space.
    pub path: String,
    /// Whether this row describes a folder (`true`) or a file.
    pub is_dir: bool,
    /// Local modification time (unix milliseconds; whole seconds × 1000
    /// for entries recorded before schema v3) as seen at the last sync.
    pub local_mtime: Option<i64>,
    /// Local size in bytes as seen at the last sync.
    pub local_size: Option<i64>,
    /// Server ETag as seen at the last sync.
    pub remote_etag: Option<String>,
    /// Remote size in bytes as seen at the last sync.
    pub remote_size: Option<i64>,
    /// Optional content checksum for change detection.
    pub checksum: Option<String>,
    /// When the path was last confirmed in sync (unix seconds).
    pub last_synced_at: Option<i64>,
    /// Current lifecycle state.
    pub state: EntryState,
}

impl NewEntry {
    /// Creates a synced entry with only the identifying fields set.
    pub fn new(path: impl Into<String>, is_dir: bool, state: EntryState) -> Self {
        Self {
            path: path.into(),
            is_dir,
            local_mtime: None,
            local_size: None,
            remote_etag: None,
            remote_size: None,
            checksum: None,
            last_synced_at: None,
            state,
        }
    }
}

/// A row of the `entries` table, as returned by read queries.
#[derive(Debug, Clone)]
pub struct Entry {
    /// Row id assigned by SQLite.
    pub id: i64,
    /// All stored columns (see [`NewEntry`] for their meaning).
    pub entry: NewEntry,
}

/// Resolution state of a conflict, stored in `conflicts.status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictStatus {
    /// Detected but not yet resolved; the file must not be touched.
    Pending,
    /// Resolved by taking the server version.
    ResolvedServer,
    /// Resolved by keeping the local version.
    ResolvedLocal,
}

impl ConflictStatus {
    /// Text stored in the `status` column.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::ResolvedServer => "resolved_server",
            Self::ResolvedLocal => "resolved_local",
        }
    }

    /// Parses the `status` column; unknown values are an error.
    pub fn from_db(value: &str) -> Result<Self, crate::DbError> {
        match value {
            "pending" => Ok(Self::Pending),
            "resolved_server" => Ok(Self::ResolvedServer),
            "resolved_local" => Ok(Self::ResolvedLocal),
            other => Err(crate::DbError::InvalidValue {
                column: "status",
                value: other.to_owned(),
            }),
        }
    }
}

/// Which side wins when a conflict is resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictResolution {
    /// Download and overwrite the local file with the server version.
    Server,
    /// Upload and overwrite the server file with the local version.
    Local,
}

impl ConflictResolution {
    /// The resulting conflict status once this resolution is applied.
    pub fn resulting_status(self) -> ConflictStatus {
        match self {
            Self::Server => ConflictStatus::ResolvedServer,
            Self::Local => ConflictStatus::ResolvedLocal,
        }
    }
}

/// Values for a new row in `conflicts`; the row id and `status` are
/// assigned by the database (`status` starts as `pending`).
#[derive(Debug, Clone)]
pub struct NewConflict {
    /// Relative path of the conflicting file inside the space.
    pub path: String,
    /// When the conflict was detected (unix seconds).
    pub detected_at: i64,
    /// Local modification time (unix milliseconds) at detection time.
    pub local_mtime: Option<i64>,
    /// Local size in bytes at detection time.
    pub local_size: Option<i64>,
    /// Server ETag at detection time.
    pub remote_etag: Option<String>,
    /// Remote size in bytes at detection time.
    pub remote_size: Option<i64>,
    /// Remote modification time (unix seconds) at detection time.
    pub remote_mtime: Option<i64>,
}

/// A row of the `conflicts` table, as returned by read queries.
#[derive(Debug, Clone)]
pub struct Conflict {
    /// Row id assigned by SQLite; used to address resolutions.
    pub id: i64,
    /// All stored columns (see [`NewConflict`]) plus status and timestamps.
    pub path: String,
    /// When the conflict was detected (unix seconds).
    pub detected_at: i64,
    /// Local modification time (unix milliseconds) at detection time.
    pub local_mtime: Option<i64>,
    /// Local size in bytes at detection time.
    pub local_size: Option<i64>,
    /// Server ETag at detection time.
    pub remote_etag: Option<String>,
    /// Remote size in bytes at detection time.
    pub remote_size: Option<i64>,
    /// Remote modification time (unix seconds) at detection time.
    pub remote_mtime: Option<i64>,
    /// Current resolution state.
    pub status: ConflictStatus,
    /// When the conflict was resolved, if it was.
    pub resolved_at: Option<i64>,
}

/// Aggregated numbers for the UI: counts per state, open conflicts, and
/// when the job last completed a sync run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// Number of synced files (folders not included).
    pub files: u64,
    /// Number of folders tracked.
    pub dirs: u64,
    /// Entries currently uploading or downloading.
    pub transfers: u64,
    /// Open (pending) conflicts.
    pub conflicts: u64,
    /// Entries in the `error` state.
    pub errors: u64,
    /// `meta.last_sync` of the job (unix seconds), if a sync ran.
    pub last_sync: Option<i64>,
}
