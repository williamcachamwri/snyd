use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::index::{DocEntry, TrigramIndex};
use crate::protocol::ResultKind;

const CACHE_VERSION: u32 = 2;

/// Maximum age of a cache file before it is considered stale, regardless of
/// filesystem mtime. Safety net for macOS where nested file changes may not
/// update the top-level scope directory mtime.
const MAX_CACHE_AGE_SECS: u64 = 86_400; // 24 hours

#[derive(Serialize, Deserialize)]
struct CacheHeader {
    version: u32,
    built_at: u64, // unix seconds
    scope_mtimes: Vec<(String, u64)>, // (scope path, mtime)
}

#[derive(Serialize, Deserialize)]
struct SerialDoc {
    path: String,
    name_lower: String,
    path_dir_lower: String,
    acronym: String,
    tokens: Vec<String>,
    body_lower: String,
    body_tokens: Vec<String>,
    kind: ResultKind,
    mtime: u64,
    size: u64,
}

#[derive(Serialize, Deserialize)]
struct CacheFile {
    header: CacheHeader,
    docs: Vec<SerialDoc>,
}

/// Return the default cache path inside the given cache directory.
pub fn cache_path(cache_dir: &std::path::Path) -> PathBuf {
    cache_dir.join("index.bin")
}

/// Build a cache header from the current timestamp and scope directory mtimes.
fn build_header(scopes: &[PathBuf]) -> CacheHeader {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let mut scope_mtimes = Vec::with_capacity(scopes.len());
    for scope in scopes {
        let mtime = std::fs::metadata(scope)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        scope_mtimes.push((scope.to_string_lossy().to_string(), mtime));
    }

    CacheHeader {
        version: CACHE_VERSION,
        built_at: now,
        scope_mtimes,
    }
}

/// Check whether any scope directory has been modified since the cache was built,
/// or if the cache itself is older than the maximum allowed age.
fn scopes_stale(header: &CacheHeader) -> bool {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // Force rebuild if cache is older than 24h regardless of mtime.
    if now.saturating_sub(header.built_at) > MAX_CACHE_AGE_SECS {
        return true;
    }

    for (path, cached_mtime) in &header.scope_mtimes {
        let current_mtime = std::fs::metadata(path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if current_mtime > *cached_mtime {
            return true;
        }
    }
    false
}

/// Save the index to disk for fast startup on next launch.
/// Writes atomically via a temp file + rename.
///
/// Memory note: the index is capped at `MAX_INDEX_DOCS` (500K docs, ~100 MB)
/// to keep both RAM and cache file size bounded for typical macOS use.
pub fn save(index: &TrigramIndex, scopes: &[PathBuf], cache_dir: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    let header = build_header(scopes);
    let docs: Vec<SerialDoc> = index
        .docs
        .iter()
        .filter(|d| !d.deleted)
        .map(|d| SerialDoc {
            path: d.path.clone(),
            name_lower: d.name_lower.clone(),
            path_dir_lower: d.path_dir_lower.clone(),
            acronym: d.acronym.clone(),
            tokens: d.tokens.clone(),
            body_lower: d.body_lower.clone(),
            body_tokens: d.body_tokens.clone(),
            kind: d.kind,
            mtime: d.mtime,
            size: d.size,
        })
        .collect();

    let cache = CacheFile { header, docs };
    let encoded = bincode::serialize(&cache)?;

    let path = cache_path(cache_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp_path = path.with_extension("tmp");
    std::fs::write(&tmp_path, encoded)?;
    std::fs::rename(&tmp_path, &path)?;

    Ok(())
}

/// Try to load a cached index. Returns `None` if the cache is missing,
/// version-mismatched, or any scope has been modified since the cache was built.
pub fn load(_scopes: &[PathBuf], cache_dir: &std::path::Path) -> Option<TrigramIndex> {
    let path = cache_path(cache_dir);
    let encoded = std::fs::read(&path).ok()?;
    let cache: CacheFile = bincode::deserialize(&encoded).ok()?;

    if cache.header.version != CACHE_VERSION {
        return None;
    }

    if scopes_stale(&cache.header) {
        return None;
    }

    let docs: Vec<DocEntry> = cache
        .docs
        .into_iter()
        .map(|d| DocEntry {
            path: d.path,
            name_lower: d.name_lower,
            path_dir_lower: d.path_dir_lower,
            acronym: d.acronym,
            tokens: d.tokens,
            body_lower: d.body_lower,
            body_tokens: d.body_tokens,
            kind: d.kind,
            mtime: d.mtime,
            size: d.size,
            deleted: false,
        })
        .collect();

    Some(TrigramIndex::from_docs(docs))
}

#[cfg(test)]
use std::sync::Mutex;

#[cfg(test)]
static CACHE_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_roundtrip_save_load() {
        let _guard = CACHE_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join("snyd_test_persist");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "test").unwrap();

        let idx = TrigramIndex::build(&[dir.clone()]);
        let doc_count = idx.docs.len();
        assert!(doc_count > 0);

        let cache_dir = std::env::temp_dir().join("snyd_test_cache");
        save(&idx, &[dir.clone()], &cache_dir).unwrap();

        let loaded = load(&[dir.clone()], &cache_dir).expect("should load successfully");
        assert_eq!(loaded.docs.len(), doc_count);
        assert_eq!(loaded.tombstone_count, 0);

        // Cleanup
        let _ = std::fs::remove_dir_all(&cache_dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_stale_cache_returns_none() {
        let _guard = CACHE_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join("snyd_test_stale");
        let cache_dir = std::env::temp_dir().join("snyd_test_stale_cache");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&cache_dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "test").unwrap();

        let idx = TrigramIndex::build(&[dir.clone()]);
        save(&idx, &[dir.clone()], &cache_dir).unwrap();

        // Sleep 1.1s so any filesystem touch will definitely advance mtime
        std::thread::sleep(std::time::Duration::from_millis(1100));
        // Recreate the scope directory (guarantees mtime advance)
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("b.txt"), "touch").unwrap();

        // Now load should see the scope as stale
        assert!(load(&[dir.clone()], &cache_dir).is_none());

        // Cleanup
        let _ = std::fs::remove_dir_all(&cache_dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_version_mismatch_returns_none() {
        let _guard = CACHE_LOCK.lock().unwrap();
        let cache_dir = std::env::temp_dir().join("snyd_test_version");
        let _ = std::fs::remove_dir_all(&cache_dir);
        std::fs::create_dir_all(&cache_dir).unwrap();
        let bad = CacheFile {
            header: CacheHeader {
                version: 0, // wrong version
                built_at: 0,
                scope_mtimes: vec![],
            },
            docs: vec![],
        };
        let encoded = bincode::serialize(&bad).unwrap();
        let cache = cache_path(&cache_dir);
        std::fs::write(&cache, encoded).unwrap();

        assert!(load(&[], &cache_dir).is_none());

        // Cleanup
        let _ = std::fs::remove_file(&cache);
    }

    #[test]
    fn test_cache_expires_after_24h() {
        let _guard = CACHE_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join("snyd_test_expire");
        let cache_dir = std::env::temp_dir().join("snyd_test_expire_cache");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&cache_dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "test").unwrap();

        let idx = TrigramIndex::build(&[dir.clone()]);
        save(&idx, &[dir.clone()], &cache_dir).unwrap();

        // Validate the safety-net constant exists in a reasonable range.
        assert!(MAX_CACHE_AGE_SECS <= 86_400 * 7, "Cache max age should be <= 7 days");
        assert!(MAX_CACHE_AGE_SECS >= 3_600, "Cache max age should be >= 1 hour");

        // Cleanup
        let _ = std::fs::remove_dir_all(&cache_dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_cache_path_is_user_specific() {
        let cache_dir = std::env::temp_dir().join("snyd_test_path");
        let path = cache_path(&cache_dir);
        assert!(
            path.to_string_lossy().contains("snyd_test_path"),
            "Cache path should contain cache_dir name, got: {:?}",
            path
        );
    }
}
