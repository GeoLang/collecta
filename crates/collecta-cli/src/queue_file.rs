//! The sync queue as a file on disk.

use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use collecta_core::SyncQueue;

use crate::Result;

/// Read the queue at `path`. A file that is not there yet is an empty queue, so
/// the first `submit` on a device works like every later one.
pub fn load(path: &Path) -> Result<SyncQueue> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(serde_json::from_str(&contents)?),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(SyncQueue::new()),
        Err(error) => Err(error.into()),
    }
}

/// Write the queue through a temporary file beside it, so an interrupted write
/// cannot replace collected data with half a file.
pub fn save(path: &Path, queue: &SyncQueue) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    let temporary_path = path.with_extension("json.writing");
    fs::write(&temporary_path, serde_json::to_vec_pretty(queue)?)?;
    fs::rename(&temporary_path, path)?;
    Ok(())
}
