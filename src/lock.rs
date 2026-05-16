use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub struct TaskLock {
    path: PathBuf,
    _file: File,
}

impl TaskLock {
    pub fn acquire(task_dir: &Path) -> Result<Option<Self>> {
        let path = task_dir.join(".tickclaw.lock");
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => Ok(Some(Self { path, _file: file })),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
            Err(err) => Err(err).with_context(|| format!("failed to create {}", path.display())),
        }
    }
}

impl Drop for TaskLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
