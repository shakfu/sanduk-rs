//! Embeds `resources/` in the binary: the shipped agents, recipes and kits. A `pip install`
//! shipped them as files; a single binary carries them and extracts them on first use.

use std::path::{Path, PathBuf};

fn main() {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("resources");
    let mut files = Vec::new();
    walk(&root, &root, &mut files);
    files.sort();
    let mut out = String::from("pub static FILES: &[(&str, &[u8])] = &[\n");
    for (rel, full) in &files {
        println!("cargo:rerun-if-changed={}", full.display());
        out += &format!(
            "    ({rel:?}, include_bytes!({:?})),\n",
            full.display().to_string()
        );
    }
    out += "];\n";
    let dest = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("resources.rs");
    std::fs::write(dest, out).unwrap();
}

fn walk(dir: &Path, root: &Path, files: &mut Vec<(String, PathBuf)>) {
    // A directory is watched too, so an added file triggers a rebuild.
    println!("cargo:rerun-if-changed={}", dir.display());
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            walk(&path, root, files);
        } else {
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            files.push((rel, path));
        }
    }
}
