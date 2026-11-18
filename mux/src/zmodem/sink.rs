//! Where received files land on local disk.
//!
//! The file name comes from the remote peer over the wire, so it is treated as
//! untrusted: path separators are stripped, `.`/`..`/drive-ish names are
//! rejected, and an existing file is never overwritten unless the user asked
//! for that.  This mirrors the rules the iTerm2 download path already uses in
//! `wezterm-gui/src/download.rs`; both now call the helpers here so the two
//! cannot drift apart.
use anyhow::Context;
use std::fs::{File, OpenOptions};
use std::io::{Seek, Write};
use std::path::{Path, PathBuf};

/// Strip any directory component and reject obviously dangerous names.
///
/// Returns `None` for names that must not be used (empty, `.`, `..`, or
/// containing a `:` which is a path separator on Windows/classic Mac).
pub fn neuter_name(name: &str) -> Option<&str> {
    let name = match name.rsplit_once(|c| c == '/' || c == '\\') {
        Some((_, base)) => base,
        None => name,
    };

    if name.is_empty() || name == "." || name == ".." {
        return None;
    }

    if name.contains(':') {
        return None;
    }

    Some(name)
}

/// Directory that downloads go to when the user has not configured one.
pub fn default_download_dir() -> anyhow::Result<PathBuf> {
    dirs_next::download_dir()
        .or_else(dirs_next::home_dir)
        .ok_or_else(|| anyhow::anyhow!("unable to locate a download directory"))
}

/// Open a file for writing in `dir`, deriving a name that does not collide with
/// an existing one unless `overwrite` is set.
///
/// The name is derived from `raw_name`; unhelpful names fall back to a fixed
/// stem.  On success returns the path actually opened and the open handle.
pub fn create_file(
    dir: &Path,
    raw_name: Option<&str>,
    overwrite: bool,
) -> anyhow::Result<(PathBuf, File)> {
    let name = raw_name
        .and_then(neuter_name)
        .unwrap_or("downloaded-via-wezterm");

    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating download directory {}", dir.display()))?;

    if overwrite {
        let candidate = dir.join(name);
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&candidate)
            .with_context(|| format!("opening {} for writing", candidate.display()))?;
        return Ok((candidate, file));
    }

    for n in 0..20 {
        let candidate = if n == 0 {
            dir.join(name)
        } else {
            dir.join(format!("{}.{}", name, n))
        };

        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(err).with_context(|| format!("creating {}", candidate.display()));
            }
        }
    }

    anyhow::bail!(
        "Unable to find a non-conflicting download name for {} in {}",
        name,
        dir.display()
    )
}

/// Open an existing partial file for **append**, for resume.
///
/// Returns `(path, file, already_have_bytes)`; the caller must ask the sender
/// to start at `already_have_bytes`.  The file is truncated to the offset we
/// report so a stale tail from a previous run cannot corrupt the result.
pub fn open_for_resume(
    dir: &Path,
    raw_name: Option<&str>,
    expected_len: u64,
) -> anyhow::Result<(PathBuf, File, u64)> {
    let name = raw_name
        .and_then(neuter_name)
        .unwrap_or("downloaded-via-wezterm");
    let path = dir.join(name);

    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {} for resume", path.display()))?;

    let have = file.metadata()?.len();
    if have > expected_len {
        // The local copy is longer than what the peer is offering: it cannot be
        // the same file, so starting from zero is the safe answer.
        anyhow::bail!(
            "{} is larger ({}) than the incoming file ({})",
            path.display(),
            have,
            expected_len
        );
    }
    file.set_len(have)?;
    file.seek(std::io::SeekFrom::Start(have))?;
    Ok((path, file, have))
}

/// Write `data` to a fresh file in `dir` and return its path.
///
/// This is the whole-file helper used by the iTerm2 download path; ZMODEM
/// streams into the handle from [`create_file`] instead.
pub fn write_file(
    dir: &Path,
    raw_name: Option<&str>,
    data: &[u8],
    overwrite: bool,
) -> anyhow::Result<PathBuf> {
    let (path, mut file) = create_file(dir, raw_name, overwrite)?;
    file.write_all(data)
        .with_context(|| format!("writing {} bytes to {}", data.len(), path.display()))?;
    file.flush()?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neuters_path_components() {
        assert_eq!(neuter_name("/etc/passwd"), Some("passwd"));
        assert_eq!(neuter_name(r"C:\Windows\win.ini"), Some("win.ini"));
        assert_eq!(neuter_name("dir/sub/file.txt"), Some("file.txt"));
        assert_eq!(neuter_name("file.txt"), Some("file.txt"));
    }

    #[test]
    fn rejects_dangerous_names() {
        assert_eq!(neuter_name(".."), None);
        assert_eq!(neuter_name("."), None);
        assert_eq!(neuter_name(""), None);
        assert_eq!(neuter_name("a:b"), None);
        assert_eq!(neuter_name("../"), None);
    }

    #[test]
    fn resume_rewinds_to_the_partial_length() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("r.bin"), b"0123456789").unwrap();
        let (path, mut file, have) = open_for_resume(dir.path(), Some("r.bin"), 100).unwrap();
        assert_eq!(have, 10);
        assert_eq!(path, dir.path().join("r.bin"));
        // Writing continues at the end of what we already have.
        file.write_all(b"abc").unwrap();
        drop(file);
        assert_eq!(std::fs::read(&path).unwrap(), b"0123456789abc");
    }

    #[test]
    fn resume_refuses_a_local_file_larger_than_the_offer() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("big.bin"), vec![0u8; 50]).unwrap();
        assert!(open_for_resume(dir.path(), Some("big.bin"), 10).is_err());
    }

    #[test]
    fn does_not_overwrite_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let (first, _) = create_file(dir.path(), Some("f.bin"), false).unwrap();
        let (second, _) = create_file(dir.path(), Some("f.bin"), false).unwrap();
        assert_eq!(first.file_name().unwrap(), "f.bin");
        assert_eq!(second.file_name().unwrap(), "f.bin.1");
        assert_ne!(first, second);
    }

    #[test]
    fn overwrite_reuses_the_name() {
        let dir = tempfile::tempdir().unwrap();
        let (first, _) = create_file(dir.path(), Some("f.bin"), false).unwrap();
        let (second, _) = create_file(dir.path(), Some("f.bin"), true).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn sanitizes_remote_names_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), Some("../../evil.sh"), b"x", false).unwrap();
        assert_eq!(path, dir.path().join("evil.sh"));
        assert_eq!(std::fs::read(&path).unwrap(), b"x");
    }
}
