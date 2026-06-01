use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crc32fast::Hasher as Crc32Hasher;
use memmap2::Mmap;

use crate::index::{DocEntry, TrigramIndex};
use crate::protocol::ResultKind;

const CACHE_VERSION: u32 = 3; // bumped for rkyv format
const MAGIC: &[u8] = b"SNYD";

/// File layout (little-endian):
///   [0..4]      magic       "SNYD"
///   [4..8]      version     u32
///   [8..16]     data_len    u64
///   [16..n]     data        rkyv archive bytes
///   [n..n+4]    checksum    crc32(data)

// ── rkyv serializable structs ──────────────────────────────────────────────

#[derive(rkyv_derive::Archive, rkyv_derive::Serialize, rkyv_derive::Deserialize)]
#[archive(check_bytes)]
struct CacheHeader {
    version: u32,
    built_at: u64, // unix seconds
    scope_mtimes: Vec<(String, u64)>, // (scope path, mtime)
}

#[derive(rkyv_derive::Archive, rkyv_derive::Serialize, rkyv_derive::Deserialize)]
#[archive(check_bytes)]
struct SerialDoc {
    path: String,
    name_lower: String,
    acronym: String,
    tokens: Vec<String>,
    body_lower: String,
    body_tokens: Vec<String>,
    kind: ResultKind,
    mtime: u64,
    size: u64,
    extension: String,
    access_count: u32,
    last_accessed: u64,
}

#[derive(rkyv_derive::Archive, rkyv_derive::Serialize, rkyv_derive::Deserialize)]
#[archive(check_bytes)]
struct CacheFile {
    header: CacheHeader,
    docs: Vec<SerialDoc>,
}

// ── Helpers ─────────────────────────────────────────────────────────────────

pub fn cache_path(cache_dir: &std::path::Path) -> PathBuf {
    cache_dir.join("index_v3.bin")
}

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

const MAX_CACHE_AGE_SECS: u64 = 86_400; // 24 hours

fn scopes_stale(header: &CacheHeader) -> bool {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
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

fn compute_checksum(data: &[u8]) -> u32 {
    let mut hasher = Crc32Hasher::new();
    hasher.update(data);
    hasher.finalize()
}

// ── Public API ──────────────────────────────────────────────────────────────

/// Save the index atomically via temp file + rename.
pub fn save(
    index: &TrigramIndex,
    scopes: &[PathBuf],
    cache_dir: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let header = build_header(scopes);
    let docs: Vec<SerialDoc> = index
        .docs
        .iter()
        .filter(|d| !d.deleted)
        .map(|d| SerialDoc {
            path: d.path.clone(),
            name_lower: d.name_lower.clone(),
            acronym: d.acronym.clone(),
            tokens: d.tokens.iter().map(|s| s.to_string()).collect(),
            body_lower: d.body_lower.clone(),
            body_tokens: d.body_tokens.iter().map(|s| s.to_string()).collect(),
            kind: d.kind,
            mtime: d.mtime,
            size: d.size,
            extension: d.extension.clone(),
            access_count: d.access_count,
            last_accessed: d.last_accessed,
        })
        .collect();

    let cache = CacheFile { header, docs };

    // Serialize with rkyv
    use rkyv::ser::{Serializer, serializers::AllocSerializer};
    let mut serializer = AllocSerializer::<256>::default();
    serializer.serialize_value(&cache)?;
    let archive = serializer.into_serializer().into_inner();
    let data = archive.as_slice();

    let data_len = data.len() as u64;
    let checksum = compute_checksum(data);

    let path = cache_path(cache_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let tmp_path = path.with_extension("tmp");
    {
        use std::io::Write;
        let mut file = std::fs::File::create(&tmp_path)?;
        file.write_all(MAGIC)?;
        file.write_all(&CACHE_VERSION.to_le_bytes())?;
        file.write_all(&data_len.to_le_bytes())?;
        file.write_all(data)?;
        file.write_all(&checksum.to_le_bytes())?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp_path, &path)?;
    Ok(())
}

/// Load a cached index via mmap + rkyv deserialization.
/// Returns `None` if cache is missing, corrupt, version-mismatch, or stale.
pub fn load(_scopes: &[PathBuf], cache_dir: &std::path::Path) -> Option<TrigramIndex> {
    let path = cache_path(cache_dir);
    let file = std::fs::File::open(&path).ok()?;
    let mmap = unsafe { Mmap::map(&file).ok()? };
    let bytes: &[u8] = &mmap;

    if bytes.len() < 20 {
        return None;
    }

    // Magic
    if &bytes[0..4] != MAGIC {
        return None;
    }

    // Version
    let version = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    if version != CACHE_VERSION {
        return None;
    }

    // Data length
    let data_len = u64::from_le_bytes(bytes[8..16].try_into().unwrap()) as usize;
    let data_start = 16usize;
    let data_end = data_start.checked_add(data_len)?;
    let checksum_start = data_end;
    if bytes.len() < checksum_start.checked_add(4)? {
        return None;
    }

    let data = &bytes[data_start..data_end];
    let stored_checksum = u32::from_le_bytes(bytes[checksum_start..checksum_start + 4].try_into().unwrap());
    if compute_checksum(data) != stored_checksum {
        tracing::warn!("Cache checksum mismatch — rebuilding index");
        return None;
    }

    // Deserialize from rkyv (faster than bincode; mmap avoids file read copy)
    let cache: CacheFile = rkyv::from_bytes(data).ok()?;

    if scopes_stale(&cache.header) {
        return None;
    }

    let docs: Vec<DocEntry> = cache
        .docs
        .into_iter()
        .map(|d| DocEntry {
            path: d.path,
            name_lower: d.name_lower,
            acronym: d.acronym,
            tokens: d.tokens.iter().map(|s| crate::index::IStr::from(s.as_str())).collect(),
            body_lower: d.body_lower,
            body_tokens: d.body_tokens.iter().map(|s| crate::index::IStr::from(s.as_str())).collect(),
            kind: d.kind,
            mtime: d.mtime,
            size: d.size,
            deleted: false,
            extension: d.extension,
            access_count: d.access_count,
            last_accessed: d.last_accessed,
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
        let dir = std::env::temp_dir().join("snyd_test_persist_v3");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "test").unwrap();

        let idx = TrigramIndex::build(&[dir.clone()]);
        let doc_count = idx.docs.len();
        assert!(doc_count > 0);

        let cache_dir = std::env::temp_dir().join("snyd_test_cache_v3");
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
        let dir = std::env::temp_dir().join("snyd_test_stale_v3");
        let cache_dir = std::env::temp_dir().join("snyd_test_stale_cache_v3");
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
        let cache_dir = std::env::temp_dir().join("snyd_test_version_v3");
        let _ = std::fs::remove_dir_all(&cache_dir);
        std::fs::create_dir_all(&cache_dir).unwrap();

        // Write a file with wrong magic so version check fails early
        let mut buf = Vec::new();
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&0u32.to_le_bytes()); // wrong version
        buf.extend_from_slice(&0u64.to_le_bytes());
        let cache = cache_path(&cache_dir);
        std::fs::write(&cache, buf).unwrap();

        assert!(load(&[], &cache_dir).is_none());

        // Cleanup
        let _ = std::fs::remove_file(&cache);
    }

    #[test]
    fn test_cache_expires_after_24h() {
        let _guard = CACHE_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join("snyd_test_expire_v3");
        let cache_dir = std::env::temp_dir().join("snyd_test_expire_cache_v3");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&cache_dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "test").unwrap();

        let idx = TrigramIndex::build(&[dir.clone()]);
        save(&idx, &[dir.clone()], &cache_dir).unwrap();

        assert!(MAX_CACHE_AGE_SECS <= 86_400 * 7);
        assert!(MAX_CACHE_AGE_SECS >= 3_600);

        // Cleanup
        let _ = std::fs::remove_dir_all(&cache_dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_cache_path_is_user_specific() {
        let cache_dir = std::env::temp_dir().join("snyd_test_path_v3");
        let path = cache_path(&cache_dir);
        assert!(
            path.to_string_lossy().contains("snyd_test_path_v3"),
            "Cache path should contain cache_dir name, got: {:?}",
            path
        );
    }
}
