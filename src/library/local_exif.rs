//! EXIF / XMP metadata extraction for local files.
//!
//! Uses `rexiv2` (binding to `gexiv2` → `exiv2`) so the tag reader handles
//! every container exiv2 supports: JPEG, TIFF, PNG, WebP, JXL, HEIC/HEIF/AVIF,
//! and the major RAW formats (CR2/CR3, NEF, ARW, RAF, ORF, RW2, DNG, ...).
//!
//! Results are cached on disk under the existing app cache directory, keyed by
//! `(canonical_path, mtime_nanos, size_bytes)` — re-reading a multi-megabyte
//! RAW each time the user reopens the details pane would be wasteful.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

/// Subset of EXIF/XMP fields the details pane renders. Mirrors the shape of
/// the remote `ExifInfo` returned by Immich so the renderer can be source-agnostic.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LocalExif {
    pub make: Option<String>,
    pub model: Option<String>,
    pub lens_model: Option<String>,
    pub f_number: Option<f64>,
    pub focal_length: Option<f64>,
    pub iso: Option<u32>,
    pub exposure_time: Option<String>,
    /// RFC3339 string when available so we can reuse the existing
    /// `format_datetime_display` helper without a second parser.
    pub date_time_original: Option<String>,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub image_width: Option<u32>,
    pub image_height: Option<u32>,
    pub description: Option<String>,
}

impl LocalExif {
    /// True when no useful fields were parsed (helpful for hiding empty groups).
    pub fn is_empty(&self) -> bool {
        self.make.is_none()
            && self.model.is_none()
            && self.lens_model.is_none()
            && self.f_number.is_none()
            && self.focal_length.is_none()
            && self.iso.is_none()
            && self.exposure_time.is_none()
            && self.date_time_original.is_none()
            && self.latitude.is_none()
            && self.longitude.is_none()
            && self.image_width.is_none()
            && self.image_height.is_none()
            && self.description.is_none()
    }
}

static INIT: OnceLock<bool> = OnceLock::new();

/// Initialize gexiv2 once per process. The underlying library wraps a non-thread-safe
/// XML parser and must be primed before any worker hits the API.
fn ensure_initialized() -> bool {
    *INIT.get_or_init(|| match rexiv2::initialize() {
        Ok(()) => true,
        Err(err) => {
            log::warn!("rexiv2 initialization failed; local EXIF disabled: {}", err);
            false
        }
    })
}

/// Read EXIF/XMP for `path`. Returns `None` when the file can't be opened or
/// has no recognisable metadata container; consumers should hide their EXIF UI
/// in that case.
pub fn read_exif(path: &Path) -> Option<LocalExif> {
    if !ensure_initialized() {
        return None;
    }
    let meta = rexiv2::Metadata::new_from_path(path).ok()?;
    let gps = meta.get_gps_info();

    let exif = LocalExif {
        make: tag_string(&meta, "Exif.Image.Make"),
        model: tag_string(&meta, "Exif.Image.Model"),
        lens_model: tag_string(&meta, "Exif.Photo.LensModel")
            .or_else(|| tag_string(&meta, "Exif.NikonLd3.LensIDNumber"))
            .or_else(|| tag_string(&meta, "Xmp.aux.Lens")),
        f_number: tag_rational(&meta, "Exif.Photo.FNumber"),
        focal_length: tag_rational(&meta, "Exif.Photo.FocalLength"),
        iso: tag_u32(&meta, "Exif.Photo.ISOSpeedRatings")
            .or_else(|| tag_u32(&meta, "Exif.Photo.PhotographicSensitivity")),
        exposure_time: tag_string(&meta, "Exif.Photo.ExposureTime"),
        date_time_original: parse_exif_datetime(
            tag_string(&meta, "Exif.Photo.DateTimeOriginal")
                .or_else(|| tag_string(&meta, "Exif.Image.DateTime"))
                .as_deref(),
            tag_string(&meta, "Exif.Photo.OffsetTimeOriginal")
                .or_else(|| tag_string(&meta, "Exif.Photo.OffsetTime"))
                .as_deref(),
        ),
        latitude: gps.map(|g| g.latitude),
        longitude: gps.map(|g| g.longitude),
        image_width: meta.get_pixel_width().try_into().ok(),
        image_height: meta.get_pixel_height().try_into().ok(),
        description: tag_string(&meta, "Exif.Image.ImageDescription")
            .or_else(|| tag_string(&meta, "Xmp.dc.description"))
            .filter(|s| !s.trim().is_empty()),
    };

    if exif.is_empty() { None } else { Some(exif) }
}

/// Load cached EXIF for `path` if the cache entry is still fresh, otherwise
/// parse, store, and return. Cheap when warm; honours `(mtime, size)`
/// invalidation so edits to the file aren't masked.
pub fn load_or_extract(cache_root: &Path, path: &Path) -> Option<LocalExif> {
    let key = cache_key(path)?;
    let cache_path = cache_root.join("exif").join(format!("{}.json", key.digest));

    if let Ok(mut f) = fs::File::open(&cache_path) {
        let mut buf = String::new();
        if f.read_to_string(&mut buf).is_ok()
            && let Ok(entry) = serde_json::from_str::<CacheEntry>(&buf)
            && entry.mtime_nanos == key.mtime_nanos
            && entry.size == key.size
        {
            return entry.exif;
        }
    }

    let parsed = read_exif(path);

    if let Some(parent) = cache_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let entry = CacheEntry {
        mtime_nanos: key.mtime_nanos,
        size: key.size,
        exif: parsed.clone(),
    };
    if let Ok(payload) = serde_json::to_vec(&entry)
        && let Ok(mut f) = fs::File::create(&cache_path)
    {
        let _ = f.write_all(&payload);
    }

    parsed
}

#[derive(Serialize, Deserialize)]
struct CacheEntry {
    mtime_nanos: u128,
    size: u64,
    exif: Option<LocalExif>,
}

struct CacheKey {
    digest: String,
    mtime_nanos: u128,
    size: u64,
}

fn cache_key(path: &Path) -> Option<CacheKey> {
    let meta = fs::metadata(path).ok()?;
    let size = meta.len();
    let mtime = meta.modified().ok()?;
    let mtime_nanos = mtime
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let digest = blake3::hash(canonical.as_os_str().as_encoded_bytes())
        .to_hex()
        .to_string();
    Some(CacheKey {
        digest,
        mtime_nanos,
        size,
    })
}

fn tag_string(meta: &rexiv2::Metadata, tag: &str) -> Option<String> {
    meta.get_tag_interpreted_string(tag)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            meta.get_tag_string(tag)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
}

fn tag_u32(meta: &rexiv2::Metadata, tag: &str) -> Option<u32> {
    // rexiv2 returns 0 for both "tag absent" and "value is 0"; treat <= 0
    // as absent since ISO 0 and orientation 0 are invalid EXIF values.
    let v = meta.get_tag_numeric(tag);
    if v <= 0 { None } else { u32::try_from(v).ok() }
}

fn tag_rational(meta: &rexiv2::Metadata, tag: &str) -> Option<f64> {
    let v = meta.get_tag_rational(tag)?;
    let denom = *v.denom();
    if denom == 0 {
        return None;
    }
    Some(f64::from(*v.numer()) / f64::from(denom))
}

/// EXIF stores `DateTimeOriginal` as `YYYY:MM:DD HH:MM:SS` (no timezone) with
/// an optional sibling `OffsetTimeOriginal` (`+HH:MM`). Normalise to RFC3339
/// so the lightbox formatter can convert to local time consistently.
fn parse_exif_datetime(raw: Option<&str>, offset: Option<&str>) -> Option<String> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    // Replace the two date colons with dashes: `2024:01:15 14:25:15` -> `2024-01-15 14:25:15`.
    if !raw.is_ascii() || raw.len() < 19 {
        return None;
    }
    let mut out = String::with_capacity(25);
    out.push_str(&raw[..4]);
    out.push('-');
    out.push_str(&raw[5..7]);
    out.push('-');
    out.push_str(&raw[8..10]);
    out.push('T');
    out.push_str(&raw[11..19]);
    match offset.map(str::trim).filter(|s| !s.is_empty()) {
        Some(off) => out.push_str(off),
        None => out.push('Z'),
    }
    Some(out)
}

/// Build the on-disk EXIF cache directory rooted under the same cache parent
/// used by the thumbnail cache. Created lazily on first cache miss.
pub fn cache_root() -> PathBuf {
    crate::profile::cache_dir().unwrap_or_else(std::env::temp_dir)
}

/// Linker shim: rexiv2 0.10 references `gexiv2_metadata_free` but
/// gexiv2 >= 0.14 removed it. Forwards to `g_object_unref`.
// nosemgrep: rust.lang.security.unsafe-usage.unsafe-usage
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gexiv2_metadata_free(metadata: *mut std::ffi::c_void) {
    unsafe extern "C" {
        fn g_object_unref(object: *mut std::ffi::c_void);
    }
    if !metadata.is_null() {
        // SAFETY: pointer is a live GObject owned by rexiv2::Metadata being dropped.
        unsafe {
            g_object_unref(metadata);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_empty_detects_no_fields() {
        assert!(LocalExif::default().is_empty());
    }

    #[test]
    fn is_empty_false_when_any_field_set() {
        let e = LocalExif {
            iso: Some(400),
            ..Default::default()
        };
        assert!(!e.is_empty());

        let e = LocalExif {
            description: Some("hi".into()),
            ..Default::default()
        };
        assert!(!e.is_empty());
    }

    #[test]
    fn parse_exif_datetime_with_offset() {
        assert_eq!(
            parse_exif_datetime(Some("2024:01:15 19:55:15"), Some("+05:30")),
            Some("2024-01-15T19:55:15+05:30".to_string())
        );
    }

    #[test]
    fn parse_exif_datetime_defaults_to_z_without_offset() {
        assert_eq!(
            parse_exif_datetime(Some("2024:01:15 19:55:15"), None),
            Some("2024-01-15T19:55:15Z".to_string())
        );
    }

    #[test]
    fn parse_exif_datetime_treats_empty_offset_as_missing() {
        assert_eq!(
            parse_exif_datetime(Some("2024:01:15 19:55:15"), Some("   ")),
            Some("2024-01-15T19:55:15Z".to_string())
        );
    }

    #[test]
    fn parse_exif_datetime_rejects_short_input() {
        assert_eq!(parse_exif_datetime(Some("2024:01:15"), None), None);
    }

    #[test]
    fn parse_exif_datetime_returns_none_for_empty_and_missing() {
        assert_eq!(parse_exif_datetime(Some("   "), None), None);
        assert_eq!(parse_exif_datetime(None, None), None);
    }

    #[test]
    fn cache_key_invalidates_on_mtime_change() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let k1 = cache_key(tmp.path()).expect("key1");
        // Touching the file with a different mtime should yield a different key.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(tmp.path(), b"changed").unwrap();
        let k2 = cache_key(tmp.path()).expect("key2");
        assert_eq!(k1.digest, k2.digest, "digest is canonical-path-based");
        assert_ne!(
            k1.mtime_nanos, k2.mtime_nanos,
            "rewriting the file must bump mtime"
        );
    }

    #[test]
    fn load_or_extract_caches_negative_result() {
        // A plain text file has no EXIF; we still cache the `None` so we don't
        // re-parse on every open.
        let cache_dir = tempfile::tempdir().expect("cache dir");
        let mut data = tempfile::NamedTempFile::new().expect("tempfile");
        use std::io::Write;
        data.write_all(b"not an image").unwrap();
        let first = load_or_extract(cache_dir.path(), data.path());
        let cache_file_count = std::fs::read_dir(cache_dir.path().join("exif"))
            .map(|d| d.count())
            .unwrap_or(0);
        let second = load_or_extract(cache_dir.path(), data.path());
        assert_eq!(first.is_some(), second.is_some());
        assert_eq!(cache_file_count, 1, "negative result is persisted once");
    }
}
