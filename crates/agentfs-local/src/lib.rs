//! A single local persistence boundary for metadata, objects and working files.

mod objects;
mod sqlite;
mod working;

use agentfs_model::{Error, ErrorCode, FORMAT_VERSION, LocationId, ObjectId, Result, WorkspaceId};
use parking_lot::Mutex;
use rusqlite::Connection;
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone)]
pub struct LocalBackend {
    inner: Arc<Inner>,
}

struct Inner {
    root: PathBuf,
    connection: Mutex<Connection>,
    leases: Mutex<BTreeMap<(WorkspaceId, ObjectId), usize>>,
    verified: Mutex<BTreeMap<(WorkspaceId, ObjectId), (u64, SystemTime)>>,
    object_io: Mutex<()>,
    _lock: File,
    location: LocationId,
}

impl std::fmt::Debug for LocalBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalBackend")
            .field("data_dir", &self.inner.root)
            .field("location", &self.inner.location)
            .finish_non_exhaustive()
    }
}

impl LocalBackend {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        create_private_dir(path.as_ref())?;
        let root = path.as_ref().canonicalize()?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join("engine.lock"))?;
        lock.try_lock().map_err(|error| {
            Error::new(
                ErrorCode::Busy,
                format!("data directory is already in use: {error}"),
            )
        })?;
        for name in ["objects", "staging", "working"] {
            create_private_dir(&root.join(name))?;
        }
        let database = root.join("meta.db");
        if std::fs::symlink_metadata(&database).is_ok_and(|meta| meta.file_type().is_symlink()) {
            return Err(Error::new(
                ErrorCode::PermissionDenied,
                "database path must not be a symbolic link",
            ));
        }
        let mut connection = Connection::open(database).map_err(sqlite::sql_error)?;
        sqlite::validate_format(&connection, FORMAT_VERSION)?;
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(sqlite::sql_error)?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(sqlite::sql_error)?;
        connection
            .pragma_update(None, "synchronous", "FULL")
            .map_err(sqlite::sql_error)?;
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .map_err(sqlite::sql_error)?;
        let location = sqlite::initialize(&mut connection, FORMAT_VERSION)?;
        sync_directory(&root)?;
        Ok(Self {
            inner: Arc::new(Inner {
                root,
                connection: Mutex::new(connection),
                leases: Mutex::new(BTreeMap::new()),
                verified: Mutex::new(BTreeMap::new()),
                object_io: Mutex::new(()),
                _lock: lock,
                location,
            }),
        })
    }

    pub fn data_dir(&self) -> &Path {
        &self.inner.root
    }

    async fn blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce(Self) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let backend = self.clone();
        tokio::task::spawn_blocking(move || f(backend))
            .await
            .map_err(|error| Error::new(ErrorCode::Io, format!("local I/O task failed: {error}")))?
    }

    async fn db<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        self.blocking(move |backend| f(&mut backend.inner.connection.lock()))
            .await
    }
}

fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|time| time.as_nanos().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

fn create_private_dir(path: &Path) -> Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)?;
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(windows)]
fn sync_directory(_path: &Path) -> Result<()> {
    // Windows publishes sealed objects with MOVEFILE_WRITE_THROUGH; SQLite
    // flushes its own WAL. Directory handles do not support FlushFileBuffers.
    Ok(())
}

#[cfg(unix)]
fn publish_object(temporary: tempfile::TempPath, destination: &Path) -> std::io::Result<()> {
    temporary
        .persist_noclobber(destination)
        .map_err(|error| error.error)
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn publish_object(temporary: tempfile::TempPath, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_NORMAL, MOVEFILE_WRITE_THROUGH, MoveFileExW, SetFileAttributesW,
    };
    let source: Vec<u16> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
    let target: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    // SAFETY: The source is a valid, terminated UTF-16 path owned by this operation.
    // Clear tempfile's temporary-file attribute before durable publication.
    if unsafe { SetFileAttributesW(source.as_ptr(), FILE_ATTRIBUTE_NORMAL) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: Both paths are valid, terminated UTF-16 buffers alive for the call.
    // Omitting REPLACE_EXISTING preserves immutable object publication.
    if unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), MOVEFILE_WRITE_THROUGH) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
