//! The shipped agents, recipes and kits, embedded at build time.
//!
//! Recipes and kits are read from a real directory: a build context copies files from it, and a
//! `copy` section is checked against its path. So the embedded files are written out to the cache
//! directory, under a hash of their content, and checked against the binary on every use: a file
//! edited there is written back rather than trusted.

use std::path::PathBuf;
use std::sync::OnceLock;

use sha2::{Digest, Sha256};

use crate::error::Result;
use crate::util::cache_dir;

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/resources.rs"));
}

pub use embedded::FILES;

/// The directory holding the shipped `agents/`, `recipes/` and `kits/`.
pub fn root() -> Result<PathBuf> {
    static ROOT: OnceLock<std::result::Result<PathBuf, String>> = OnceLock::new();
    ROOT.get_or_init(|| extract().map_err(|e| e.message))
        .clone()
        .map_err(crate::error::Error::new)
}

fn extract() -> Result<PathBuf> {
    let mut hash = Sha256::new();
    for (rel, data) in FILES {
        hash.update(rel.as_bytes());
        hash.update([0]);
        hash.update(Sha256::digest(data));
    }
    let digest = format!("{:x}", hash.finalize());
    let dir = cache_dir().join("resources").join(&digest[..16]);
    for (rel, data) in FILES {
        let path = dir.join(rel);
        if std::fs::read(&path).is_ok_and(|on_disk| on_disk == *data) {
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Written beside, then renamed: two processes extracting at once never read half a file.
        let partial = path.with_extension(format!("partial-{}", std::process::id()));
        std::fs::write(&partial, data)?;
        std::fs::rename(&partial, &path)?;
    }
    Ok(dir)
}
