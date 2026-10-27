/**
 * Private copies of SQLite databases that another process may be writing.
 */

use rusqlite::Connection;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// A copy of a database and its `-wal`/`-shm` files in a fresh temp directory,
/// open for reading. Dropping it closes the connection, then deletes the copy.
pub struct SqliteSnapshot {
    pub conn: Connection,
    _dir: TempDir,
}

impl SqliteSnapshot {
    /// Copy `db_path` into a new directory under `temp_root`, or under the
    /// system temp dir when None, and open the copy.
    pub fn open(db_path: &Path, temp_root: Option<&Path>) -> anyhow::Result<Self> {
        let dir = match temp_root {
            Some(root) => TempDir::new_in(root)?,
            None => TempDir::new()?,
        };
        let copy = dir.path().join("snapshot.db");
        fs::copy(db_path, &copy)?;
        // SQLite only reads a WAL named after its database, so the sidecars keep the copy's name.
        for suffix in ["-wal", "-shm"] {
            match fs::copy(with_suffix(db_path, suffix), with_suffix(&copy, suffix)) {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        let conn = Connection::open(&copy)?;
        Ok(Self { conn, _dir: dir })
    }
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}
