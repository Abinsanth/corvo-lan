//! Helpers shared by unit tests.

use std::path::{Path, PathBuf};

/// A fresh folder under the system temp dir, removed when dropped.
pub struct Scratch(PathBuf);

impl Scratch {
    pub fn new(prefix: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("{prefix}-{:016x}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
