//! XMP sidecar file discovery.
//!
//! Locates `.xmp` companion files alongside media assets using the same
//! naming conventions that Immich recognises:
//!   1. `photo.jpg.xmp`  (preferred -- includes the media extension)
//!   2. `photo.xmp`      (fallback  -- stem only)
//!
//! Both patterns are probed case-insensitively so `photo.CR2.XMP` is found.

use std::path::{Path, PathBuf};

/// Find the XMP sidecar companion for a media file, if one exists on disk.
///
/// Returns the preferred `<name>.<ext>.xmp` path when present, otherwise
/// falls back to `<stem>.xmp`. Returns `None` when neither exists.
pub fn find_sidecar(media_path: &Path) -> Option<PathBuf> {
    let parent = media_path.parent()?;
    let filename = media_path.file_name()?.to_str()?;

    // Preferred: photo.jpg.xmp
    let preferred = parent.join(format!("{}.xmp", filename));
    if case_insensitive_exists(&preferred) {
        return Some(resolve_case_insensitive(&preferred).unwrap_or(preferred));
    }

    // Fallback: photo.xmp
    let stem = media_path.file_stem()?.to_str()?;
    let fallback = parent.join(format!("{}.xmp", stem));
    if case_insensitive_exists(&fallback) {
        return Some(resolve_case_insensitive(&fallback).unwrap_or(fallback));
    }

    None
}
/// Check whether a file exists using case-insensitive extension matching.
///
/// Tries the exact path first (fast), then scans the parent directory for
/// a case-variant match (e.g. `.XMP` vs `.xmp`).
fn case_insensitive_exists(path: &Path) -> bool {
    if path.exists() {
        return true;
    }
    resolve_case_insensitive(path).is_some()
}

/// Scan the parent directory for a file whose name matches case-insensitively.
fn resolve_case_insensitive(path: &Path) -> Option<PathBuf> {
    let parent = path.parent()?;
    let target = path.file_name()?.to_str()?.to_ascii_lowercase();
    let entries = std::fs::read_dir(parent).ok()?;
    for entry in entries.flatten() {
        if let Some(name) = entry.file_name().to_str()
            && name.to_ascii_lowercase() == target
        {
            return Some(entry.path());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn setup() -> TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn preferred_pattern_found() {
        let dir = setup();
        let media = dir.path().join("photo.jpg");
        let sidecar = dir.path().join("photo.jpg.xmp");
        fs::write(&media, b"img").unwrap();
        fs::write(&sidecar, b"<xmp/>").unwrap();

        assert_eq!(find_sidecar(&media), Some(sidecar));
    }

    #[test]
    fn fallback_pattern_found() {
        let dir = setup();
        let media = dir.path().join("photo.cr2");
        let sidecar = dir.path().join("photo.xmp");
        fs::write(&media, b"raw").unwrap();
        fs::write(&sidecar, b"<xmp/>").unwrap();

        assert_eq!(find_sidecar(&media), Some(sidecar));
    }

    #[test]
    fn preferred_takes_priority_over_fallback() {
        let dir = setup();
        let media = dir.path().join("photo.dng");
        let preferred = dir.path().join("photo.dng.xmp");
        let _fallback = dir.path().join("photo.xmp");
        fs::write(&media, b"raw").unwrap();
        fs::write(&preferred, b"<preferred/>").unwrap();
        fs::write(&_fallback, b"<fallback/>").unwrap();

        assert_eq!(find_sidecar(&media), Some(preferred));
    }

    #[test]
    fn no_sidecar_returns_none() {
        let dir = setup();
        let media = dir.path().join("photo.jpg");
        fs::write(&media, b"img").unwrap();

        assert_eq!(find_sidecar(&media), None);
    }

    #[test]
    fn case_insensitive_extension() {
        let dir = setup();
        let media = dir.path().join("photo.nef");
        let sidecar = dir.path().join("photo.nef.XMP");
        fs::write(&media, b"raw").unwrap();
        fs::write(&sidecar, b"<xmp/>").unwrap();

        let result = find_sidecar(&media);
        assert!(result.is_some(), "should find .XMP variant");
        assert_eq!(
            result.unwrap().file_name().unwrap().to_str().unwrap(),
            "photo.nef.XMP"
        );
    }
}
