//! Write a committed file back to disk without following symlinks.
//!
//! `std::fs::write` follows symlinks: if the destination is a symlink to
//! `/etc/passwd`, the write lands outside the repository. This module opens
//! the destination with `O_NOFOLLOW` on Unix so a symlink target is refused
//! instead of overwritten, and refuses symlinked parents on all platforms.

use std::fs;
use std::io::Write;
use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

use crate::support::error::{Code, Error};

/// Write `content` to `root.join(rel)`, refusing to follow any symlink.
///
/// If the destination exists and is a symlink, the write is refused. If any
/// parent directory in the chain is a symlink that would let the write escape
/// `root`, the write is refused.
pub fn write_safe(root: &Path, rel: &str, content: &[u8]) -> crate::ZResult<usize> {
    let target = root.join(rel);

    if let Some(parent) = target.parent() {
        #[cfg(unix)]
        {
            // Open parent with O_NOFOLLOW | O_DIRECTORY so a symlinked parent
            // cannot redirect the write outside the repository. If the parent
            // does not exist yet, create it, then verify again.
            let mut opts = fs::OpenOptions::new();
            opts.read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY);
            match opts.open(parent) {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    fs::create_dir_all(parent)?;
                    opts.open(parent).map_err(|e| {
                        Box::new(Error::new(
                            Code::Usage,
                            format!("cannot open parent directory {}: {}", parent.display(), e),
                        )) as Box<dyn std::error::Error>
                    })?;
                }
                Err(e) => {
                    return Err(Box::new(Error::new(
                        Code::Usage,
                        format!("cannot open parent directory {}: {}", parent.display(), e),
                    )));
                }
            }
        }
        #[cfg(not(unix))]
        {
            if parent.is_symlink() {
                return Err(Box::new(Error::new(
                    Code::Usage,
                    format!("parent directory {} is a symlink", parent.display()),
                ))) as crate::ZResult<()>;
            }
            fs::create_dir_all(parent)?;
        }
    }

    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);

    #[cfg(unix)]
    {
        // Reject hardlinks to files outside the repository: writing through any
        // link to a shared inode overwrites the other link's content too.
        if let Ok(meta) = fs::symlink_metadata(&target) {
            if meta.file_type().is_symlink() {
                return Err(Box::new(Error::new(
                    Code::Usage,
                    format!("{} is a symbolic link", target.display()),
                )));
            }
            if meta.nlink() > 1 {
                return Err(Box::new(Error::new(
                    Code::Usage,
                    format!("{} has multiple hard links", target.display()),
                )));
            }
        }
        opts.custom_flags(libc::O_NOFOLLOW);
    }

    let mut file = opts.open(&target).map_err(|e| {
        Box::new(Error::new(Code::Other, e.to_string())) as Box<dyn std::error::Error>
    })?;
    file.write_all(content)?;
    Ok(content.len())
}
