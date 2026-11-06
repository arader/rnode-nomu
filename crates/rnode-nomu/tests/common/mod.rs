use std::fs;
use std::path::{Path, PathBuf};

/// A fresh, empty per-test directory under the crate's `target/`.
pub fn test_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("rnode-nomu-tests")
        .join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}
