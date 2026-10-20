//! Active application reporting for the shortcut-deck feature.
//!
//! Wraps [`active_win_pos_rs::get_active_window`] and normalizes the result
//! into a phone-friendly payload: raw app name + url-friendly slug + OS.
//!
//! The phone resolves `app` -> canonical slug via the preset repo's root
//! `apps.json` (which maps variant names like "Google Chrome"/"chrome.exe"
//! to one slug), then fetches `<slug>_<os>.json` for the button deck.

use serde_json::Value;

/// Phone-friendly view of the foreground application.
#[derive(Debug, Clone)]
pub struct ActiveApp {
    /// Raw app name as reported by the OS (e.g. "Google Chrome").
    pub app_name: String,
    /// Url-friendly slug (e.g. "google-chrome").
    pub slug: String,
    /// Window title (may be empty on macOS without Screen Recording perm).
    pub title: String,
    /// Full process path when available.
    pub process_path: String,
    /// OS process id when available.
    pub pid: u64,
    /// "windows" | "macos" | "linux" (from `std::env::consts::OS`).
    pub os: String,
}

impl ActiveApp {
    /// Serialize for the `active-app` WebSocket message.
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "type": "active-app",
            "app": self.app_name,
            "slug": self.slug,
            "title": self.title,
            "process": self.process_path,
            "pid": self.pid,
            "os": self.os,
        })
    }
}

/// Lowercase, replace every run of non-alphanumeric with `-`, trim dashes.
/// `Google Chrome` -> `google-chrome`, `After Effects` -> `after-effects`.
pub fn slugify(name: &str) -> String {
    let lower = name.to_lowercase();
    let mut out = String::with_capacity(lower.len());
    let mut dash_pending = false;
    for c in lower.chars() {
        if c.is_ascii_alphanumeric() {
            if dash_pending && !out.is_empty() {
                out.push('-');
            }
            out.push(c);
            dash_pending = false;
        } else if !out.is_empty() {
            dash_pending = true;
        }
    }
    out
}

/// Current OS string used in preset filenames (`<slug>_<os>.json`).
pub fn os_name() -> String {
    std::env::consts::OS.to_owned()
}

/// Query the OS for the foreground window. Returns `None` when the platform
/// backend cannot determine it (headless, permission denied, unsupported
/// compositor).
pub fn current() -> Option<ActiveApp> {
    let w = active_win_pos_rs::get_active_window().ok()?;
    let process_path_buf = w.process_path.clone();
    let process_path = process_path_buf.to_string_lossy().to_string();
    let app_name = if w.app_name.trim().is_empty() {
        fallback_app_name(&process_path_buf)
    } else {
        w.app_name.clone()
    };
    Some(ActiveApp {
        slug: slugify(&app_name),
        app_name,
        title: w.title.clone(),
        process_path,
        pid: w.process_id,
        os: os_name(),
    })
}

/// When `app_name` is empty (some Linux/Wayland cases), derive it from the
/// process path file stem: `/usr/bin/google-chrome` -> `google-chrome`.
fn fallback_app_name(process_path: &std::path::Path) -> String {
    process_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_owned()
}

/// Build the `active-app` payload, or an `unknown` placeholder so the phone
/// always gets a reply to `get-active-app` / hello.
pub fn payload_or_unknown() -> Value {
    match current() {
        Some(a) => a.to_json(),
        None => serde_json::json!({
            "type": "active-app",
            "app": "unknown",
            "slug": "unknown",
            "title": "",
            "process": "",
            "pid": 0,
            "os": os_name(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_rules() {
        assert_eq!(slugify("Google Chrome"), "google-chrome");
        assert_eq!(slugify("After Effects"), "after-effects");
        assert_eq!(slugify("  Visual   Studio  Code  "), "visual-studio-code");
        assert_eq!(slugify("Photoshop.exe"), "photoshop-exe");
        assert_eq!(slugify("com.google.Chrome"), "com-google-chrome");
        assert_eq!(slugify(""), "");
        assert_eq!(slugify("---"), "");
    }

    #[test]
    fn fallback_uses_file_stem() {
        assert_eq!(
            fallback_app_name(std::path::Path::new("/usr/bin/google-chrome")),
            "google-chrome"
        );
        assert_eq!(fallback_app_name(std::path::Path::new("code")), "code");
        assert_eq!(fallback_app_name(std::path::Path::new("")), "unknown");
    }

    #[test]
    fn unknown_payload_shape() {
        let v = serde_json::json!({
            "type": "active-app",
            "app": "unknown",
            "slug": "unknown",
            "title": "",
            "process": "",
            "pid": 0,
            "os": os_name(),
        });
        assert_eq!(v["type"], "active-app");
        assert_eq!(v["slug"], "unknown");
    }
}
