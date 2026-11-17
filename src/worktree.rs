//! Worktree scanning: which files belong to the repository, how to read them.

use std::fs;
use std::path::{Path, PathBuf};

use crate::ZF_DIR;

/// Directory names never descended into.
const SKIP_DIRS: &[&str] = &[ZF_DIR, "target", ".git", "node_modules", ".venv", "dist", "build"];

/// Walk `root` and return every regular file that belongs to the repository,
/// sorted, with `root`-relative paths. Directories in [`SKIP_DIRS`] are pruned.
pub fn scan_files(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let ft = entry.file_type()?;
            if ft.is_dir() {
                if SKIP_DIRS.contains(&name.as_ref()) {
                    continue;
                }
                stack.push(entry.path());
            } else if ft.is_file() {
                out.push(entry.path());
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Path relative to `root`, always with `/` separators so the database is
/// portable across platforms.
pub fn rel_path(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Read a file as UTF-8. Returns `None` for binary or unreadable content;
/// the caller decides whether that is worth reporting.
pub fn read_text(p: &Path) -> Option<String> {
    let bytes = fs::read(p).ok()?;
    String::from_utf8(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rel_path_uses_forward_slashes() {
        let root = Path::new("/tmp/repo");
        let p = Path::new("/tmp/repo/sub/file.txt");
        assert_eq!(rel_path(root, p), "sub/file.txt");
    }

    #[test]
    fn scan_prunes_vendor_dirs() {
        let tmp = std::env::temp_dir().join(format!("zf_scan_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        for d in [".zenforge", "target", "node_modules", "src"] {
            fs::create_dir_all(tmp.join(d)).unwrap();
        }
        fs::write(tmp.join("src/a.rs"), "fn main(){}").unwrap();
        fs::write(tmp.join(".zenforge/db"), "x").unwrap();
        fs::write(tmp.join("target/bin"), "x").unwrap();
        fs::write(tmp.join("node_modules/m.js"), "x").unwrap();

        let rels: Vec<String> = scan_files(&tmp).unwrap().iter().map(|p| rel_path(&tmp, p)).collect();
        assert_eq!(rels, vec!["src/a.rs"]);

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn read_text_rejects_binary() {
        let tmp = std::env::temp_dir().join(format!("zf_bin_{}", std::process::id()));
        fs::create_dir_all(&tmp).unwrap();
        let p = tmp.join("b.bin");
        fs::write(&p, [0xffu8, 0xfe, 0x00, 0x01]).unwrap();
        assert!(read_text(&p).is_none());
        let _ = fs::remove_dir_all(&tmp);
    }
}
