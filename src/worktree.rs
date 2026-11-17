//! Worktree scanning: ignore rules, file walking, text/binary detection.

use std::fs;
use std::path::{Path, PathBuf};

use crate::{LEGACY_DIR, ZF_DIR};

/// Directory names never descended into, regardless of ignore rules.
const SKIP_DIRS: &[&str] = &[ZF_DIR, LEGACY_DIR, ".git", ".hg", ".svn", "node_modules", "target", ".venv", "venv", "dist", "build", "__pycache__"];

/// A gitignore-style pattern.
///
/// Supports the subset that actually matters in practice: `*` (any run of
/// non-separator characters), `**` (any run including separators), `?` (one
/// non-separator character), a leading `/` to anchor at the repository root, a
/// trailing `/` to match directories only, and a leading `!` to un-ignore.
#[derive(Debug, Clone)]
pub struct Pattern {
    glob: Vec<Segment>,
    pub negated: bool,
    pub dir_only: bool,
    pub anchored: bool,
}

#[derive(Debug, Clone)]
enum Segment {
    Literal(char),
    AnyChar,        // ?
    AnyRun,         // *
    AnyRunCross,    // **
}

impl Pattern {
    pub fn parse(raw: &str) -> Option<Pattern> {
        let mut s = raw.trim();
        if s.is_empty() || s.starts_with('#') {
            return None;
        }
        let mut negated = false;
        if let Some(rest) = s.strip_prefix('!') {
            negated = true;
            s = rest.trim();
        }
        let dir_only = s.ends_with('/');
        if dir_only {
            s = &s[..s.len() - 1];
        }
        let anchored = s.starts_with('/') || s.contains('/');
        let s = s.trim_start_matches('/');
        if s.is_empty() {
            return None;
        }

        let mut glob = Vec::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '*' => {
                    if chars.peek() == Some(&'*') {
                        chars.next();
                        // `**/` also matches zero directories
                        if chars.peek() == Some(&'/') {
                            chars.next();
                        }
                        glob.push(Segment::AnyRunCross);
                    } else {
                        glob.push(Segment::AnyRun);
                    }
                }
                '?' => glob.push(Segment::AnyChar),
                _ => glob.push(Segment::Literal(c)),
            }
        }
        Some(Pattern { glob, negated, dir_only, anchored })
    }

    /// Does this pattern match `path` (a `/`-separated repo-relative path)?
    pub fn matches(&self, path: &str, is_dir: bool) -> bool {
        if self.dir_only && is_dir {
            return self.match_any_prefix(path);
        }
        if self.dir_only && !is_dir {
            // A directory-only rule also matches files inside that directory,
            // at any depth: `secret/` covers `deep/secret/key.pem`.
            return self.match_any_prefix(path);
        }
        if self.anchored {
            return self.match_text(path);
        }
        self.match_suffix(path)
    }

    /// Match the pattern against the whole path, or against any parent prefix
    /// of it (used for directory-only rules).
    fn match_any_prefix(&self, path: &str) -> bool {
        if self.anchored {
            let parts: Vec<&str> = path.split('/').collect();
            for end in 1..=parts.len() {
                if self.match_text(&parts[..end].join("/")) {
                    return true;
                }
            }
            return false;
        }
        let parts: Vec<&str> = path.split('/').collect();
        for start in 0..parts.len() {
            for end in start + 1..=parts.len() {
                if self.match_text(&parts[start..end].join("/")) {
                    return true;
                }
            }
        }
        false
    }

    /// An unanchored pattern matches the name or any trailing sub-path.
    fn match_suffix(&self, path: &str) -> bool {
        if self.match_text(path) {
            return true;
        }
        let name = path.rsplit('/').next().unwrap_or(path);
        if self.match_text(name) {
            return true;
        }
        let parts: Vec<&str> = path.split('/').collect();
        for i in 1..parts.len() {
            if self.match_text(&parts[i..].join("/")) {
                return true;
            }
        }
        false
    }

    fn match_text(&self, text: &str) -> bool {
        let t: Vec<char> = text.chars().collect();
        self.match_from(0, 0, &t)
    }

    fn match_from(&self, gi: usize, ti: usize, t: &[char]) -> bool {
        if gi == self.glob.len() {
            return ti == t.len();
        }
        match &self.glob[gi] {
            Segment::Literal(c) => ti < t.len() && t[ti] == *c && self.match_from(gi + 1, ti + 1, t),
            Segment::AnyChar => {
                ti < t.len() && t[ti] != '/' && self.match_from(gi + 1, ti + 1, t)
            }
            Segment::AnyRun => {
                if self.match_from(gi + 1, ti, t) {
                    return true;
                }
                let mut i = ti;
                while i < t.len() && t[i] != '/' {
                    i += 1;
                    if self.match_from(gi + 1, i, t) {
                        return true;
                    }
                }
                false
            }
            Segment::AnyRunCross => {
                if self.match_from(gi + 1, ti, t) {
                    return true;
                }
                for i in ti..t.len() {
                    if self.match_from(gi + 1, i + 1, t) {
                        return true;
                    }
                }
                false
            }
        }
    }
}

/// Ordered set of ignore rules. Later rules win, so a `!` rule can re-include
/// something an earlier rule excluded.
pub struct IgnoreRules {
    patterns: Vec<Pattern>,
}

impl IgnoreRules {
    pub fn new(patterns: Vec<String>) -> IgnoreRules {
        IgnoreRules {
            patterns: patterns.iter().filter_map(|p| Pattern::parse(p)).collect(),
        }
    }

    /// Built-in defaults plus whatever the caller supplies.
    pub fn with_defaults(extra: &[String]) -> IgnoreRules {
        let mut all: Vec<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
        all.extend(extra.iter().cloned());
        IgnoreRules::new(all)
    }

    pub fn is_ignored(&self, path: &str, is_dir: bool) -> bool {
        let mut ignored = false;
        for p in &self.patterns {
            if p.matches(path, is_dir) {
                ignored = !p.negated;
            }
        }
        ignored
    }

    pub fn len(&self) -> usize {
        self.patterns.len()
    }

}

/// Rules applied even when the repository has no ignore file.
pub const DEFAULT_IGNORES: &[&str] = &[
    ".DS_Store",
    "Thumbs.db",
    "*.swp",
    "*.tmp",
    "*~",
    "/.gesta/", "/.zenforge/",
    "/.git/",
];

/// Walk `root` and return every regular file that belongs to the repository,
/// together with the metadata the walk already had to fetch. Sorted, with
/// `root`-relative paths, honouring the ignore rules.
///
/// Callers that need size or mtime used to stat every file a second time after
/// a path-only walk. On a 2000 file repository that is 2000 extra syscalls for
/// information the directory entry already carried.
pub fn scan_files_with_meta(
    root: &Path,
    rules: &IgnoreRules,
) -> std::io::Result<Vec<(PathBuf, fs::Metadata)>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let name = entry.file_name().to_string_lossy().to_string();
            if SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            let ft = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            let path = entry.path();
            let rel = rel_path(root, &path);
            if ft.is_dir() {
                if rules.is_ignored(&rel, true) {
                    continue;
                }
                stack.push(path);
            } else if ft.is_file() {
                if rules.is_ignored(&rel, false) {
                    continue;
                }
                // entry.metadata() does not follow extra symlinks and on most
                // filesystems costs nothing beyond what read_dir returned.
                match entry.metadata() {
                    Ok(m) => out.push((path, m)),
                    Err(_) => continue,
                }
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// Path relative to `root`, always with `/` separators so the database is
/// portable across platforms.
pub fn rel_path(root: &Path, p: &Path) -> String {
    p.strip_prefix(root).unwrap_or(p).to_string_lossy().replace('\\', "/")
}

/// Outcome of reading a file as text.
pub enum ReadOutcome {
    Text(String),
    Binary,
    Unreadable(String),
}

/// Read a file and classify it. A NUL byte in the first 8000 bytes means
/// binary, matching git's heuristic closely enough for source code.
pub fn read_file(p: &Path) -> ReadOutcome {
    match fs::read(p) {
        Ok(bytes) => {
            let head = &bytes[..bytes.len().min(8000)];
            if head.contains(&0u8) {
                return ReadOutcome::Binary;
            }
            match String::from_utf8(bytes) {
                Ok(s) => ReadOutcome::Text(s),
                Err(_) => ReadOutcome::Binary,
            }
        }
        Err(e) => ReadOutcome::Unreadable(e.to_string()),
    }
}

/// Read a file as UTF-8 text, `None` for binary or unreadable content.
pub fn read_text(p: &Path) -> Option<String> {
    match read_file(p) {
        ReadOutcome::Text(s) => Some(s),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(pats: &[&str]) -> IgnoreRules {
        IgnoreRules::new(pats.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn rel_path_uses_forward_slashes() {
        let root = Path::new("/tmp/repo");
        assert_eq!(rel_path(root, Path::new("/tmp/repo/sub/file.txt")), "sub/file.txt");
    }

    #[test]
    fn simple_glob_matches_filename_anywhere() {
        let r = rules(&["*.log"]);
        assert!(r.is_ignored("app.log", false));
        assert!(r.is_ignored("logs/app.log", false));
        assert!(!r.is_ignored("app.txt", false));
    }

    #[test]
    fn anchored_pattern_only_matches_at_root() {
        let r = rules(&["/build"]);
        assert!(r.is_ignored("build", true));
        assert!(!r.is_ignored("src/build", true));
    }

    #[test]
    fn dir_only_pattern_covers_contents() {
        let r = rules(&["secret/"]);
        assert!(r.is_ignored("secret", true));
        assert!(r.is_ignored("secret/key.pem", false));
        assert!(r.is_ignored("deep/secret/key.pem", false));
        assert!(!r.is_ignored("secretfile.txt", false));
    }

    #[test]
    fn double_star_crosses_directories() {
        let r = rules(&["docs/**/tmp"]);
        assert!(r.is_ignored("docs/a/tmp", false));
        assert!(r.is_ignored("docs/a/b/c/tmp", false));
        assert!(!r.is_ignored("other/tmp", false));
    }

    #[test]
    fn negation_re_includes_a_file() {
        let r = rules(&["*.log", "!keep.log"]);
        assert!(r.is_ignored("app.log", false));
        assert!(!r.is_ignored("keep.log", false), "later ! rule must win");
    }

    #[test]
    fn question_mark_matches_one_character() {
        let r = rules(&["file?.txt"]);
        assert!(r.is_ignored("file1.txt", false));
        assert!(!r.is_ignored("file10.txt", false));
    }

    #[test]
    fn comments_and_blanks_are_skipped() {
        let r = rules(&["# a comment", "", "  ", "*.o"]);
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn defaults_ignore_os_noise_but_not_source() {
        let r = IgnoreRules::with_defaults(&[]);
        assert!(r.is_ignored(".DS_Store", false));
        assert!(!r.is_ignored("src/.gesta/x", false)); // nested, not the repo dir
        assert!(!r.is_ignored("src/main.rs", false));
    }

    #[test]
    fn env_file_is_ignored_when_user_asks() {
        let r = IgnoreRules::with_defaults(&[".env".to_string(), "*.key".to_string()]);
        assert!(r.is_ignored(".env", false));
        assert!(r.is_ignored("certs/server.key", false));
        assert!(!r.is_ignored("app.rs", false));
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("zf_wt_{}_{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn scan_prunes_vendor_dirs_and_ignored_paths() {
        let tmp = tmpdir("scan");
        for d in [".gesta", "target", "node_modules", "src", "secret"] {
            fs::create_dir_all(tmp.join(d)).unwrap();
        }
        fs::write(tmp.join("src/a.rs"), "fn main(){}").unwrap();
        fs::write(tmp.join(".gesta/db"), "x").unwrap();
        fs::write(tmp.join("target/bin"), "x").unwrap();
        fs::write(tmp.join("node_modules/m.js"), "x").unwrap();
        fs::write(tmp.join("secret/key.pem"), "PRIVATE").unwrap();
        fs::write(tmp.join(".env"), "SECRET=1").unwrap();
        fs::write(tmp.join("app.rs"), "fn main(){}").unwrap();

        let r = IgnoreRules::with_defaults(&[
            "secret/".to_string(),
            ".env".to_string(),
        ]);
        let got: Vec<String> = scan_files_with_meta(&tmp, &r)
            .unwrap()
            .iter()
            .map(|(p, _)| rel_path(&tmp, p))
            .collect();
        assert_eq!(got, vec!["app.rs", "src/a.rs"], "ignored paths must not be tracked");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn read_file_separates_binary_from_text() {
        let tmp = tmpdir("bin");
        let text = tmp.join("a.txt");
        let bin = tmp.join("b.bin");
        fs::write(&text, "plain text\n").unwrap();
        fs::write(&bin, [0x89u8, 0x50, 0x4e, 0x47, 0x00, 0x01]).unwrap();

        assert!(matches!(read_file(&text), ReadOutcome::Text(_)));
        assert!(matches!(read_file(&bin), ReadOutcome::Binary));
        assert!(matches!(read_file(&tmp.join("missing")), ReadOutcome::Unreadable(_)));
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn utf8_without_nul_is_text() {
        let tmp = tmpdir("utf8");
        let p = tmp.join("vi.txt");
        fs::write(&p, "Tiếng Việt có dấu\n\u{7bc0}\u{70b9}\n").unwrap();
        assert_eq!(read_text(&p).unwrap(), "Tiếng Việt có dấu\n\u{7bc0}\u{70b9}\n");
        let _ = fs::remove_dir_all(&tmp);
    }
}
