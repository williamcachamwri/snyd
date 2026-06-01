use std::path::{Path, PathBuf};

use crate::index::ScoredDoc;
use crate::protocol::ResultKind;

/// A single application bundle entry.
#[derive(Debug, Clone)]
pub struct AppEntry {
    pub path: String,
    pub display_name: String,
}

/// In-memory cache of application bundles.
pub struct AppCache {
    pub entries: Vec<AppEntry>,
}

impl AppCache {
    /// Load app bundles from the given directories.
    /// Uses platform-specific default directories when `dirs` is empty.
    pub fn load(dirs: &[PathBuf]) -> Self {
        let mut entries = Vec::new();

        let default_dirs: Vec<PathBuf> = if dirs.is_empty() {
            #[cfg(target_os = "macos")]
            {
                vec![PathBuf::from("/Applications")]
            }
            #[cfg(not(target_os = "macos"))]
            {
                vec![
                    PathBuf::from("/usr/share/applications"),
                    PathBuf::from("/usr/local/share/applications"),
                    dirs::home_dir()
                        .map(|h| h.join(".local/share/applications"))
                        .unwrap_or_default(),
                ]
            }
        } else {
            dirs.to_vec()
        };

        for dir in &default_dirs {
            if let Ok(read_dir) = std::fs::read_dir(dir) {
                for entry in read_dir.flatten() {
                    let path = entry.path();
                    if is_app_dir(&path) {
                        if let Some(app) = parse_app_entry(&path) {
                            entries.push(app);
                        }
                    }
                }
            }
        }

        AppCache { entries }
    }
}

/// Score apps against the query and return top-k matches.
///
/// Prefers exact and prefix matches over Jaro-Winkler fuzzy scoring
/// so that short queries like "vs" match "Visual Studio Code" reliably.
pub fn score_apps(cache: &AppCache, query: &str, max: usize) -> Vec<ScoredDoc> {
    let query_lower = query.to_lowercase();
    let mut results = Vec::with_capacity(cache.entries.len().min(max * 3));

    for (idx, app) in cache.entries.iter().enumerate() {
        let name_lower = app.display_name.to_lowercase();

        let score = if name_lower == query_lower {
            120.0_f32
        } else if name_lower.starts_with(&query_lower) {
            // Prefix: longer query relative to name = better match
            100.0 + (query_lower.len() as f32 / name_lower.len() as f32) * 20.0
        } else if word_boundary_starts_with_str(&name_lower, &query_lower) {
            85.0 + (query_lower.len() as f32 / name_lower.len() as f32) * 15.0
        } else {
            // Fuzzy fallback via Jaro-Winkler
            let jw = strsim::jaro_winkler(&query_lower, &name_lower) as f32;
            if jw < 0.75 {
                continue;
            } // slightly higher threshold for purity
            jw * 70.0
        };

        results.push(ScoredDoc {
            doc_id: idx as u32,
            score,
        });
    }

    results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    results.truncate(max);
    results
}

/// Check if `name` starts with `query` at a word boundary (space, underscore, etc.)
fn word_boundary_starts_with_str(name: &str, query: &str) -> bool {
    if name.starts_with(query) {
        return true;
    }
    for (i, _ch) in name.char_indices() {
        if i == 0 {
            continue;
        }
        let prev = name[..i].chars().last().unwrap_or('a');
        if matches!(prev, ' ' | '_' | '-' | '.') && name[i..].starts_with(query) {
            return true;
        }
    }
    false
}

/// Convert an AppCache entry to a SearchResult.
pub fn app_to_result(app: &AppEntry, score: f32) -> crate::protocol::SearchResult {
    let (size, mtime) = std::fs::metadata(&app.path).map_or((0, 0), |m| {
        let mtime = m
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        (m.len(), mtime)
    });

    crate::protocol::SearchResult {
        path: app.path.clone(),
        name: app.display_name.clone(),
        kind: ResultKind::Application,
        size,
        modified: mtime,
        score,
    }
}

/// Platform-agnostic entry point — dispatches to macOS or Linux parser.
fn parse_app_entry(path: &Path) -> Option<AppEntry> {
    #[cfg(target_os = "macos")]
    {
        parse_app_bundle(path)
    }
    #[cfg(not(target_os = "macos"))]
    {
        parse_desktop_file(path)
    }
}

// ── macOS ──────────────────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
fn is_app_dir(path: &Path) -> bool {
    path.is_dir()
        && path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e == "app")
            .unwrap_or(false)
}

#[cfg(target_os = "macos")]
fn parse_app_bundle(path: &Path) -> Option<AppEntry> {
    let info_plist = path.join("Contents/Info.plist");
    if !info_plist.exists() {
        return fallback_app_name(path);
    }

    let val: plist::Value = plist::from_file(&info_plist).ok()?;
    let dict = val.as_dictionary()?;

    let display_name = dict
        .get("CFBundleDisplayName")
        .or_else(|| dict.get("CFBundleName"))
        .and_then(|v| v.as_string())
        .unwrap_or_else(|| {
            path.file_stem()
                .and_then(|n| n.to_str())
                .unwrap_or("Unknown")
        })
        .to_string();

    Some(AppEntry {
        path: path.to_string_lossy().to_string(),
        display_name,
    })
}

#[cfg(target_os = "macos")]
fn fallback_app_name(path: &Path) -> Option<AppEntry> {
    let name = path
        .file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or("Unknown")
        .to_string();
    Some(AppEntry {
        path: path.to_string_lossy().to_string(),
        display_name: name,
    })
}

// ── Linux ──────────────────────────────────────────────────────────────────

#[cfg(not(target_os = "macos"))]
fn is_app_dir(path: &Path) -> bool {
    path.is_file()
        && path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e == "desktop")
            .unwrap_or(false)
}

#[cfg(not(target_os = "macos"))]
fn parse_desktop_file(path: &Path) -> Option<AppEntry> {
    let content = std::fs::read_to_string(path).ok()?;
    let mut name = None;
    let mut is_app = false;

    for line in content.lines() {
        if line.trim() == "[Desktop Entry]" {
            is_app = true;
            continue;
        }
        if line.starts_with('[') {
            // New section — break if we've passed Desktop Entry
            if is_app {
                break;
            }
            continue;
        }
        if let Some(val) = line.strip_prefix("Name=") {
            name = Some(val.trim().to_string());
        }
        if let Some(val) = line.strip_prefix("Type=") {
            if val.trim() != "Application" {
                return None; // Not an app
            }
        }
    }

    let display_name = name.unwrap_or_else(|| {
        path.file_stem()
            .and_then(|n| n.to_str())
            .unwrap_or("Unknown")
            .to_string()
    });

    Some(AppEntry {
        path: path.to_string_lossy().to_string(),
        display_name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_score_apps_exact_match_wins() {
        let cache = AppCache {
            entries: vec![
                AppEntry {
                    path: "/Applications/Zoom.app".into(),
                    display_name: "Zoom".into(),
                },
                AppEntry {
                    path: "/Applications/Zotero.app".into(),
                    display_name: "Zotero".into(),
                },
            ],
        };
        let results = score_apps(&cache, "zoom", 10);
        assert!(!results.is_empty());
        assert_eq!(results[0].doc_id, 0); // "Zoom" exact match wins over "Zotero"
    }

    #[test]
    fn test_score_apps_short_prefix_matches() {
        let cache = AppCache {
            entries: vec![
                AppEntry {
                    path: "/Applications/VSCode.app".into(),
                    display_name: "VS Code".into(),
                },
                AppEntry {
                    path: "/Applications/VSCodium.app".into(),
                    display_name: "VSCodium".into(),
                },
            ],
        };
        // "vs" matches both via prefix ("VS Code" and "VSCodium")
        let results = score_apps(&cache, "vs", 10);
        assert!(!results.is_empty(), "Short query 'vs' should match app names");
    }

    #[test]
    fn test_score_apps_no_false_positives() {
        let cache = AppCache {
            entries: vec![
                AppEntry {
                    path: "/Applications/Xcode.app".into(),
                    display_name: "Xcode".into(),
                },
            ],
        };
        let results = score_apps(&cache, "zzz", 10);
        assert!(results.is_empty(), "Completely unrelated query should return no results");
    }
}
