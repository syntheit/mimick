//! Per-profile state isolation driven by the `MIMICK_PROFILE` env var.
//!
//! When unset (the default), every state directory uses the literal
//! `mimick` segment under the user's XDG config / data / cache roots.
//! When set to e.g. `dev`, the segment becomes `mimick-dev`, isolating
//! config, the sharded SyncIndex, retries, and the thumbnail cache from
//! the default profile.
//!
//! Names are restricted to `[A-Za-z][A-Za-z0-9_-]{0,31}` so they remain valid
//! both as filesystem path segments (no traversal) and as D-Bus / GTK
//! application-id segments (must start with a letter). Invalid values are
//! logged and ignored.

use std::path::PathBuf;
use std::sync::OnceLock;

const PROFILE_ENV_VAR: &str = "MIMICK_PROFILE";
const MAX_PROFILE_LEN: usize = 32;

static PROFILE: OnceLock<Option<String>> = OnceLock::new();
static DIR_SEGMENT: OnceLock<String> = OnceLock::new();

/// Active profile name, or `None` when the default profile is in use.
pub fn name() -> Option<&'static str> {
    profile().as_deref()
}

/// Directory segment to use inside `dirs::*_dir()` paths.
///
/// Returns `"mimick"` for the default profile and `"mimick-{name}"`
/// for a named profile.
pub fn dir_segment() -> &'static str {
    DIR_SEGMENT
        .get_or_init(|| match profile() {
            Some(name) => format!("mimick-{}", name),
            None => "mimick".to_string(),
        })
        .as_str()
}

pub fn config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join(dir_segment()))
}

pub fn data_dir() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join(dir_segment()))
}

pub fn cache_dir() -> Option<PathBuf> {
    dirs::cache_dir().map(|d| d.join(dir_segment()))
}

/// GTK / D-Bus application id. Varies by profile so multiple profiles can
/// run as independent GTK instances. Always remains a valid D-Bus name
/// because `name()` is sanitised to start with a letter.
pub fn application_id() -> String {
    match name() {
        Some(profile) => format!("dev.nicx.mimick.{}", profile),
        None => "dev.nicx.mimick".to_string(),
    }
}

/// Value for the keyring `account` attribute, scoping the API key per
/// profile so switching profiles doesn't overwrite the default's secret.
/// The default profile keeps the historical `"api_key"` value so existing
/// installations continue to find their stored key.
pub fn keyring_account() -> String {
    match name() {
        Some(profile) => format!("api_key-{}", profile),
        None => "api_key".to_string(),
    }
}

fn profile() -> &'static Option<String> {
    PROFILE.get_or_init(|| match std::env::var(PROFILE_ENV_VAR) {
        Ok(raw) => sanitise(&raw),
        Err(_) => None,
    })
}

fn sanitise(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.len() > MAX_PROFILE_LEN {
        log::warn!(
            "Ignoring {}: name longer than {} characters",
            PROFILE_ENV_VAR,
            MAX_PROFILE_LEN
        );
        return None;
    }
    if !trimmed
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic())
    {
        log::warn!("Ignoring {}: must start with a letter", PROFILE_ENV_VAR);
        return None;
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        log::warn!(
            "Ignoring {}: only [A-Za-z0-9_-] are allowed",
            PROFILE_ENV_VAR
        );
        return None;
    }
    Some(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::sanitise;

    #[test]
    fn sanitise_accepts_typical_names() {
        assert_eq!(sanitise("dev"), Some("dev".to_string()));
        assert_eq!(sanitise("staging-2"), Some("staging-2".to_string()));
        assert_eq!(
            sanitise("personal_local"),
            Some("personal_local".to_string())
        );
    }

    #[test]
    fn sanitise_rejects_path_traversal() {
        assert_eq!(sanitise("../etc"), None);
        assert_eq!(sanitise("foo/bar"), None);
        assert_eq!(sanitise(".."), None);
    }

    #[test]
    fn sanitise_requires_letter_start_for_dbus_validity() {
        assert_eq!(sanitise("2024test"), None);
        assert_eq!(sanitise("-leading-dash"), None);
        assert_eq!(sanitise("_underscore"), None);
        assert_eq!(sanitise("a2024"), Some("a2024".to_string()));
    }

    #[test]
    fn sanitise_rejects_empty_and_oversized() {
        assert_eq!(sanitise(""), None);
        assert_eq!(sanitise("   "), None);
        assert_eq!(sanitise(&"a".repeat(33)), None);
    }

    #[test]
    fn sanitise_trims_whitespace() {
        assert_eq!(sanitise("  dev  "), Some("dev".to_string()));
    }
}
