use std::path::PathBuf;

use crate::id::new_request_id;

pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("au-test-{}", new_request_id().unwrap()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    pub fn path(&self) -> &std::path::Path {
        &self.0
    }

    pub fn sock(&self, name: &str) -> PathBuf {
        self.0.join(format!("{name}.sock"))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
