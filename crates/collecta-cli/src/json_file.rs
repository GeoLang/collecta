//! The queue and the pulled forms as JSON files on disk.

use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::Result;

/// Read the file at `path`. A file that is not there yet reads as empty, so the
/// first `submit` or `pull` on a device works like every later one.
pub fn load<T: DeserializeOwned + Default>(path: &Path) -> Result<T> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(serde_json::from_str(&contents)?),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(error.into()),
    }
}

/// Write through a temporary file beside it, so an interrupted write cannot
/// replace collected data with half a file.
pub fn save<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    let temporary_path = path.with_extension("json.writing");
    fs::write(&temporary_path, serde_json::to_vec_pretty(value)?)?;
    fs::rename(&temporary_path, path)?;
    Ok(())
}
