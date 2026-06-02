use std::collections::{BinaryHeap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use jwalk::WalkDir;
use rayon::prelude::*;
use roaring::bitmap::RoaringBitmap;
use tracing::warn;

use crate::kinds::{is_app_bundle, kind_from_path};
use crate::protocol::{ResultKind, SearchResult};

const K1: f32 = 1.2;
const B: f32 = 0.75;

// ── Cache / hidden path classification patterns ────────────────────────────
const CACHE_PATTERNS: &[&str] = &[
    "/.cargo/registry",
    "/Library/Caches",
    "/Library/Developer/Xcode/DerivedData",
    "/Library/Developer/CoreSimulator",
    "/.npm",
    "/.pnpm-store",
    "/go/pkg/mod",
    "/.gradle",
    "/.m2/repository",
    "/node_modules/",
    "/vendor/",
    "/__pycache__",
    "/.pytest_cache",
    "/.tox",
    "/.uv",
    "/.ruff_cache",
    "/.next",
    "/.nuxt",
    "/.parcel-cache",
    "/.cache/",
];

/// Classify a path into a tier based on `IndexConfig`.
/// Returns `None` if the path should be excluded entirely.
pub fn classify_path(path: &Path, config: &IndexConfig) -> Option<DocTier> {
    let path_str = path.to_string_lossy();

    // Custom includes override everything
    for pattern in &config.custom_includes {
        if path_str.contains(pattern.as_str()) {
            return Some(DocTier::Normal);
        }
    }

    // Custom excludes take next priority
    for pattern in &config.custom_excludes {
        if path_str.contains(pattern.as_str()) {
            return None;
        }
    }

    // Is this a hidden path (dotfile / dotdir)?
    let is_hidden = path.components().any(|c| {
        let s = c.as_os_str().to_str().unwrap_or("");
        // Skip "./" or "/" or ".."
        if s == "." || s == ".." || s.is_empty() {
            return false;
        }
        s.starts_with('.')
    });

    // Is this a cache / build path?
    let is_cache = CACHE_PATTERNS.iter().any(|p| path_str.contains(p));

    match (is_hidden, is_cache) {
        (_, true) if !config.index_cache => None,
        (_, true) => Some(DocTier::Cache),
        (true, _) if !config.index_hidden => None,
        (true, _) => Some(DocTier::Hidden),
        _ => Some(DocTier::Normal),
    }
}

// ── Scoring constants ──────────────────────────────────────────────────
// These are calibrated so that:
//   - Exact match always beats prefix match
//   - Prefix match always beats word-boundary match
//   - BM25 base score for a 3-token query ≈ 1–8 points
//   - Bonuses dominate BM25 for short queries (expected for a launcher)
//   - Recency is a tiebreaker, not a ranking signal
const SCORE_EXACT_MATCH: f32 = 500.0;       // beats any combination of bonuses below
const SCORE_PREFIX_MATCH: f32 = 200.0;      // name starts with query
const SCORE_WORD_PREFIX: f32 = 100.0;       // word boundary starts with query
const SCORE_TERM_COVERAGE_FULL: f32 = 150.0;  // multi-term: all terms found
const SCORE_TERM_COVERAGE_HALF: f32 = 60.0;   // multi-term: ≥50% terms found
const SCORE_APP_BOOST: f32 = 50.0;          // apps prioritized over files
const SCORE_ACRONYM_EXACT: f32 = 180.0;     // exact acronym match (e.g. "vsc" → VSCode)
const SCORE_ACRONYM_PREFIX: f32 = 80.0;     // acronym prefix match
const SCORE_PATH_SEGMENT_MAX: f32 = 30.0;   // capped path dir bonus
const SCORE_RECENCY_RECENT: f32 = 40.0;     // < 1 hour old
const SCORE_RECENCY_TODAY: f32 = 25.0;      // < 1 day old
const SCORE_RECENCY_WEEK: f32 = 10.0;       // < 7 days old
const SCORE_RECENCY_MONTH: f32 = 4.0;       // < 30 days old
const SCORE_RECENCY_CAP_RATIO: f32 = 0.30;  // recency can't exceed 30% of base score
const SCORE_DEPTH_PENALTY_PER_LEVEL: f32 = 1.5;
const SCORE_DEPTH_PENALTY_MAX: f32 = 10.0;
const SCORE_NORMALIZATION_DIVISOR: f32 = 800.0; // maps raw score to [0,1]
// ──────────────────────────────────────────────────────────────────────

/// For very short queries (< 3 chars), we can't use trigram intersection.
/// Return the 5,000 most recently modified docs as candidates.
/// This is intentionally limited: 1-2 char queries are typically launcher-style
/// (e.g., "vs" for VSCode), where recency + app cache is more useful than
/// exhaustive scanning. The app cache phase in pipeline.rs handles app matching.
const SHORT_QUERY_CANDIDATE_LIMIT: usize = 5_000;

/// Maximum number of documents in the index.
/// At ~200 bytes average per DocEntry (path + tokens + bitmaps),
/// 1M docs ≈ 200 MB RAM. Cap at 500K to stay under 100 MB for typical use.
const MAX_INDEX_DOCS: usize = 500_000;

/// Interned string — cheap clone (just increment refcount).
pub type IStr = std::sync::Arc<str>;

/// Index tier classification for a document.
/// - Tier 0 (Normal): regular user files — Desktop, Documents, Downloads, etc.
/// - Tier 1 (Hidden): dotfiles, ~/.config, ~/.local, etc.
/// - Tier 2 (Cache): build artifacts, node_modules, package caches, etc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocTier {
    Normal = 0,
    Hidden = 1,
    Cache  = 2,
}

impl DocTier {
    #[inline]
    pub fn to_u8(self) -> u8 { self as u8 }
    #[inline]
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => DocTier::Hidden,
            2 => DocTier::Cache,
            _ => DocTier::Normal,
        }
    }
}

/// Configuration that controls which files get indexed and how they are classified.
#[derive(Debug, Clone)]
pub struct IndexConfig {
    pub scopes: Vec<PathBuf>,
    /// Index hidden files and directories (dotfiles, ~/.config, ~/.local, etc.)
    pub index_hidden: bool,
    /// Index cache and build directories (node_modules, .cargo/registry, DerivedData, etc.)
    pub index_cache: bool,
    /// Additional user-specified exclude patterns (substrings checked against full path)
    pub custom_excludes: Vec<String>,
    /// Override patterns that force inclusion even if they match cache/hidden patterns
    pub custom_includes: Vec<String>,
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            scopes: vec![],
            index_hidden: false,
            index_cache: false,
            custom_excludes: vec![],
            custom_includes: vec![],
        }
    }
}

/// A single document in the index.
pub struct DocEntry {
    pub path: String,
    pub name_lower: String,
    pub acronym: String,
    pub tokens: smallvec::SmallVec<[IStr; 4]>,
    /// Body text (OCR / Calendar / Contacts / iMessage) — empty when not yet indexed.
    pub body_lower: String,
    /// Tokens from body — used separately for BM25 body field scoring.
    pub body_tokens: smallvec::SmallVec<[IStr; 4]>,
    pub kind: ResultKind,
    pub mtime: u64,
    pub size: u64,
    pub deleted: bool,
    /// File extension without the leading dot, lowercase (e.g. "pdf", "rs", "").
    pub extension: String,
    /// Number of times this doc appeared in search results (for frequency boost).
    pub access_count: u32,
    /// Unix timestamp of the last access.
    pub last_accessed: u64,
    /// Index tier — controls visibility in search unless user opts in.
    pub tier: DocTier,
}

/// In-memory trigram index.
pub struct TrigramIndex {
    pub docs: Vec<DocEntry>,
    /// trigram chars → bitmap of doc ids
    pub index: HashMap<[char; 3], RoaringBitmap>,
    pub tombstone_count: usize,
    path_to_id: HashMap<String, u32>,
    /// term → document frequency (number of docs containing the term)
    doc_freq: HashMap<String, u32>,
    /// average number of tokens per doc name (for BM25 dl/avgdl)
    pub avg_doc_len: f32,
    /// Number of non-deleted (active) documents. Used for correct BM25 IDF.
    pub active_doc_count: usize,
    /// Sum of token counts across all active docs. Used to keep avg_doc_len accurate
    /// without scanning all docs on every remove().
    pub total_doc_len: f32,
    /// Bitset for fast tombstone checks during query (1 bit per doc_id).
    /// Replaces the need to access `self.docs[id].deleted` on every candidate.
    deleted_bits: Vec<u64>,
}

impl TrigramIndex {
    /// Fast O(1) tombstone check using the bitset.
    #[inline]
    fn is_deleted(&self, doc_id: u32) -> bool {
        let idx = doc_id as usize / 64;
        let bit = doc_id as usize % 64;
        self.deleted_bits.get(idx).map_or(false, |&w| (w >> bit) & 1 == 1)
    }

    /// Set the deleted bit for a doc_id.
    #[inline]
    fn set_deleted(&mut self, doc_id: u32) {
        let idx = doc_id as usize / 64;
        let bit = doc_id as usize % 64;
        if let Some(word) = self.deleted_bits.get_mut(idx) {
            *word |= 1u64 << bit;
        }
    }
}

/// A scored document for ranking.
#[derive(Debug)]
pub struct ScoredDoc {
    pub doc_id: u32,
    pub score: f32,
}

impl PartialEq for ScoredDoc {
    fn eq(&self, other: &Self) -> bool {
        self.score == other.score
    }
}

impl Eq for ScoredDoc {}

impl PartialOrd for ScoredDoc {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.score.partial_cmp(&other.score)
    }
}

impl Ord for ScoredDoc {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Scores should never be NaN; if they are, treat as 0.0 to maintain heap invariant.
        let a = if self.score.is_nan() { 0.0_f32 } else { self.score };
        let b = if other.score.is_nan() { 0.0_f32 } else { other.score };
        a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal)
    }
}

impl TrigramIndex {
    /// Build the index by walking all scopes.
    ///
    /// This is a **blocking** CPU-bound operation. It uses a rayon thread pool
    /// (via jwalk) to traverse directories in parallel. Noise directories
    /// (e.g. `node_modules`, `.git`, `DerivedData`) are classified as tier 2 (Cache)
    /// or tier 1 (Hidden) depending on `config`. Only tier 0 (Normal) is included
    /// by default; opt-in via `config.index_hidden` / `config.index_cache`.
    pub fn build(scopes: &[PathBuf]) -> Self {
        Self::build_with_config(&IndexConfig {
            scopes: scopes.to_vec(),
            ..Default::default()
        })
    }

    /// Build with full config support — tier classification, custom includes/excludes.
    pub fn build_with_config(config: &IndexConfig) -> Self {
        let mut docs: Vec<DocEntry> = Vec::with_capacity(1024 * 1024);

        let cpu_count = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);

        'outer: for scope in &config.scopes {
            let walker = WalkDir::new(scope)
                .parallelism(jwalk::Parallelism::RayonNewPool(cpu_count))
                .skip_hidden(false);

            for entry in walker {
                let Ok(entry) = entry else { continue };
                let path = entry.path();

                // Skip known VCS / system directories by name (always, regardless of tier)
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if name.starts_with('.')
                        && matches!(name, ".git" | ".svn" | ".hg" | ".DS_Store")
                    {
                        continue;
                    }
                    if matches!(
                        name,
                        "node_modules"
                            | "DerivedData"
                            | ".build"
                            | "target"
                            | "build"
                            | "dist"
                            | "out"
                    ) {
                        continue;
                    }
                }

                // Skip directories (except app bundles)
                if path.is_dir() && !is_app_bundle(&path) {
                    continue;
                }

                // Skip symlinks to avoid infinite loops and double-indexing
                if path.is_symlink() {
                    continue;
                }

                // Classify path into tier
                let Some(tier) = classify_path(&path, config) else {
                    continue; // excluded by user or by default
                };

                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string();
                let name_lower = name.to_lowercase();

                // Get metadata
                let (size, mtime) = entry.metadata().map_or((0, 0), |m| {
                    let size = m.len();
                    let mtime = m
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    (size, mtime)
                });

                let kind = if is_app_bundle(&path) {
                    ResultKind::Application
                } else {
                    kind_from_path(&path)
                };

                let path_str = path.to_string_lossy().to_string();

                if docs.len() >= MAX_INDEX_DOCS {
                    warn!("Index cap reached ({} docs). Stopping walk.", MAX_INDEX_DOCS);
                    break 'outer;
                }

                let extension = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_lowercase();

                docs.push(DocEntry {
                    path: path_str,
                    name_lower,
                    acronym: String::new(),
                    tokens: smallvec::SmallVec::new(), // filled by from_docs
                    body_lower: String::new(),
                    body_tokens: smallvec::SmallVec::new(),
                    kind,
                    mtime,
                    size,
                    deleted: false,
                    extension,
                    access_count: 0,
                    last_accessed: 0,
                    tier,
                });
            }
        }

        Self::from_docs(docs)
    }

    /// Rebuild index from existing docs (used by persist cache loading).
    ///
    /// Re-tokenizes names, rebuilds trigram bitmaps, doc_freq, and avg_doc_len.
    /// Deleted docs are filtered out.
    ///
    /// **Parallel build:** tokenization and trigram extraction run in parallel
    /// via rayon. The sequential merge phase builds the HashMap index.
    pub fn from_docs(raw_docs: Vec<DocEntry>) -> Self {
        // Phase 1 (parallel): tokenize, extract trigrams, acronym for each doc
        let prepped: Vec<_> = raw_docs
            .into_par_iter()
            .filter(|doc| !doc.deleted)
            .map(|mut doc| {
                let name_lower = doc.name_lower.to_lowercase();
                let terms = tokenize(&name_lower);
                let acronym = extract_acronym(&name_lower);
                let trigrams = extract_trigrams(&name_lower);
                doc.tokens = terms.iter().map(|s| IStr::from(s.as_str())).collect();
                doc.acronym = acronym;
                (doc, terms, trigrams)
            })
            .collect();

        // Phase 2 (sequential): merge into index structures
        let mut docs: Vec<DocEntry> = Vec::with_capacity(prepped.len());
        let mut index: HashMap<[char; 3], RoaringBitmap> = HashMap::with_capacity(prepped.len());
        let mut path_to_id: HashMap<String, u32> = HashMap::with_capacity(prepped.len());
        let mut doc_freq: HashMap<String, u32> = HashMap::new();
        let mut total_doc_len: f32 = 0.0;

        for (doc, terms, trigrams) in prepped {
            let mut seen_terms = HashSet::new();
            for term in &terms {
                if seen_terms.insert(term.clone()) {
                    *doc_freq.entry(term.clone()).or_insert(0) += 1;
                }
            }
            total_doc_len += terms.len() as f32;

            let doc_id = docs.len() as u32;
            path_to_id.insert(doc.path.clone(), doc_id);
            docs.push(doc);

            for tri in trigrams {
                index.entry(tri).or_default().insert(doc_id);
            }
        }

        let active_doc_count = docs.len();
        let avg_doc_len = if docs.is_empty() { 1.0 } else { total_doc_len / docs.len() as f32 };
        let deleted_bits = vec![0u64; (docs.len() + 63) / 64];

        TrigramIndex {
            docs,
            index,
            tombstone_count: 0,
            path_to_id,
            doc_freq,
            avg_doc_len,
            active_doc_count,
            total_doc_len,
            deleted_bits,
        }
    }

    /// Maximum possible additional score beyond cheap structural bonuses.
    /// Used for early-exit pruning: if cheap_score + this bound < heap_min, skip doc.
    /// Conservative sum of: BM25_max (~10) + recency_max (40) + path_max (30) + len_max (10).
    const MAX_POSSIBLE_REMAINING: f32 = 90.0;

    /// Query the index and return top-k scored documents.
    ///
    /// Scoring happens in three stages:
    /// 1. **Candidate selection** — intersect trigram bitmaps for each query term
    ///    (or return the most-recent N docs for very short queries).
    /// 2. **BM25 name score** — standard Okapi BM25 over filename tokens.
    /// 3. **Signal bonuses** — exact/prefix/word-boundary matches, acronym match,
    ///    app boost, path-segment bonus, recency, and depth penalty.
    ///
    /// The raw score is normalized to `[0, 1]` by `to_result()`.
    ///
    /// **Early-exit optimization:** Cheap structural bonuses are computed first.
    /// If the heap is already full and the doc's best-case score (cheap + max remaining)
    /// cannot beat the current minimum, the doc is skipped entirely — no BM25, no recency,
    #[inline]
    fn tier_matches(tier: DocTier, mask: u8) -> bool {
        (1u8 << tier.to_u8()) & mask != 0
    }

    /// Fast path for extension queries like ".har", ".pdf".
    fn query_by_extension(&self, ext: &str, max: usize) -> Vec<ScoredDoc> {
        self.query_by_extension_with_tier(ext, max, 0b111)
    }

    fn query_by_extension_with_tier(&self, ext: &str, max: usize, tier_mask: u8) -> Vec<ScoredDoc> {
        let mut results: Vec<ScoredDoc> = self
            .docs
            .iter()
            .enumerate()
            .filter(|(i, _)| !self.is_deleted(*i as u32))
            .filter(|(_, d)| Self::tier_matches(d.tier, tier_mask))
            .filter(|(_, d)| d.extension == ext)
            .map(|(i, d)| ScoredDoc {
                doc_id: i as u32,
                score: 100.0 + (d.access_count as f32).ln() * 5.0,
            })
            .collect();
        results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(max);
        results
    }

    /// Query with default tier mask (all tiers).
    pub fn query(&self, query: &str, max: usize, fuzzy: bool) -> Vec<ScoredDoc> {
        self.query_with_tier(query, max, fuzzy, 0b111)
    }

    /// Query with explicit tier mask.
    /// `tier_mask` is a bitmask: bit 0 = Normal, bit 1 = Hidden, bit 2 = Cache.
    pub fn query_with_tier(&self, query: &str, max: usize, fuzzy: bool, tier_mask: u8) -> Vec<ScoredDoc> {
        let query_lower = query.to_lowercase();

        // Extension query fast-path: ".har", ".pdf"
        if let Some(ext) = query_lower.strip_prefix('.') {
            return self.query_by_extension_with_tier(ext, max, tier_mask);
        }

        let query_terms = tokenize(&query_lower);

        let mut candidate_ids: Vec<u32> = if query_terms.is_empty() {
            return vec![];
        } else if query_terms.len() == 1 {
            self.candidates_for_term_with_tier(&query_terms[0], tier_mask)
        } else {
            let mut result: Option<HashSet<u32>> = None;
            for term in &query_terms {
                let term_candidates: HashSet<u32> =
                    self.candidates_for_term_with_tier(term, tier_mask).into_iter().collect();
                result = Some(match result {
                    None => term_candidates,
                    Some(existing) => existing.intersection(&term_candidates).copied().collect(),
                });
            }
            result.unwrap_or_default().into_iter().collect()
        };

        // ── Fuzzy fallback: if trigram intersection yields < 5 docs, scan all docs
        // with Damerau-Levenshtein distance heuristic to catch typos like "bdgt" → "budget".
        if candidate_ids.len() < 5 && fuzzy {
            let max_dist = (query_lower.len() / 4).max(1).min(2);
            let mut extra = Vec::new();
            for (i, doc) in self.docs.iter().enumerate() {
                if self.is_deleted(i as u32) {
                    continue;
                }
                if !Self::tier_matches(doc.tier, tier_mask) {
                    continue;
                }
                // Fast reject: length difference > max_dist
                if doc.name_lower.len().abs_diff(query_lower.len()) > max_dist as usize {
                    continue;
                }
                let dist = strsim::damerau_levenshtein(&query_lower, &doc.name_lower);
                if dist <= max_dist {
                    extra.push(i as u32);
                }
            }
            let mut set: HashSet<u32> = candidate_ids.into_iter().collect();
            for id in extra {
                set.insert(id);
            }
            candidate_ids = set.into_iter().collect();
        }

        // ── Optimization B: pre-sort candidates by term coverage (desc) ───
        // High-coverage docs are more likely to score well → fill heap faster →
        // increase early-exit rate for remaining candidates.
        if query_terms.len() > 1 && candidate_ids.len() > max {
            candidate_ids.sort_by_key(|&id| {
                let doc = &self.docs[id as usize];
                let coverage = query_terms
                    .iter()
                    .filter(|t| doc.name_lower.contains(t.as_str()))
                    .count();
                std::cmp::Reverse(coverage)
            });
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let candidate_count = candidate_ids.len();
        let mut heap: BinaryHeap<ScoredDoc> = BinaryHeap::with_capacity(max + 1);
        let query_char_count = query_lower.chars().count() as f32;
        let mut skipped = 0usize;

        // Extract extension from query (e.g. "report.pdf" → "pdf")
        let query_ext = query_lower.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
        // Extract stem from query (e.g. "report.pdf" → "report")
        let query_stem = query_lower.rsplit_once('.').map(|(s, _)| s).unwrap_or(&query_lower);

        for doc_id in candidate_ids {
            let doc = &self.docs[doc_id as usize];
            if self.is_deleted(doc_id) {
                continue;
            }
            if !Self::tier_matches(doc.tier, tier_mask) {
                continue;
            }

            // ── Cheap structural bonuses (fast string ops) ─────────────────
            let mut cheap_score = 0.0;

            if query_terms.len() > 1 {
                let terms_in_name = query_terms
                    .iter()
                    .filter(|t| doc.name_lower.contains(t.as_str()))
                    .count();
                let term_coverage = terms_in_name as f32 / query_terms.len() as f32;

                if term_coverage == 1.0 {
                    if doc.name_lower == query_lower {
                        cheap_score += SCORE_EXACT_MATCH;
                    } else {
                        cheap_score += SCORE_TERM_COVERAGE_FULL * term_coverage;
                    }
                } else if term_coverage >= 0.5 {
                    cheap_score += SCORE_TERM_COVERAGE_HALF * term_coverage;
                }
            } else {
                if doc.name_lower == query_lower {
                    cheap_score += SCORE_EXACT_MATCH;
                } else if doc.name_lower.starts_with(&query_lower) {
                    cheap_score += SCORE_PREFIX_MATCH;
                } else if word_boundary_starts_with(&doc.name_lower, &query_lower) {
                    cheap_score += SCORE_WORD_PREFIX;
                }
            }

            if doc.kind == ResultKind::Application {
                cheap_score += SCORE_APP_BOOST;
            }

            if query_lower.len() >= 2 && query_lower.len() <= 6 {
                if doc.acronym == query_lower {
                    cheap_score += SCORE_ACRONYM_EXACT;
                } else if doc.acronym.starts_with(&query_lower) {
                    cheap_score += SCORE_ACRONYM_PREFIX;
                }
            }

            // Extension-aware boost
            if !query_ext.is_empty() && doc.extension == query_ext {
                cheap_score += 80.0;
            }
            // Stem exact-match boost
            if !query_stem.is_empty() {
                let doc_stem = doc.name_lower.rsplit_once('.').map(|(s, _)| s).unwrap_or(&doc.name_lower);
                if doc_stem == query_stem {
                    cheap_score += 30.0;
                }
            }

            // ── Optimization A: early-exit before expensive scoring ──────
            if heap.len() >= max {
                let min_score = heap.peek().map(|s| s.score).unwrap_or(0.0);
                if cheap_score + Self::MAX_POSSIBLE_REMAINING < min_score {
                    skipped += 1;
                    continue;
                }
            }

            // ── Full scoring (BM25 + expensive bonuses) ──────────────────
            let mut score = self.bm25_score(doc_id, &query_terms);
            score += cheap_score;

            // Name length normalization bonus
            let name_len = doc.name_lower.chars().count() as f32;
            let len_bonus = 10.0 * (query_char_count / name_len.max(1.0)).min(1.0);
            score += len_bonus;

            // ── Recency: only boost files that already have substance ───────
            let age_secs = now.saturating_sub(doc.mtime);
            let recency_raw: f32 = if age_secs < 3600 {
                SCORE_RECENCY_RECENT
            } else if age_secs < 86400 {
                SCORE_RECENCY_TODAY
            } else if age_secs < 7 * 86400 {
                SCORE_RECENCY_WEEK
            } else if age_secs < 30 * 86400 {
                SCORE_RECENCY_MONTH
            } else {
                0.0
            };
            let recency_bonus = if score > 5.0 {
                recency_raw.min(score * SCORE_RECENCY_CAP_RATIO)
            } else {
                0.0
            };
            score += recency_bonus;

            // ── Access frequency boost (logarithmic to avoid over-domination) ─
            let freq_boost = if doc.access_count > 0 {
                (doc.access_count as f32).ln() * 5.0
            } else {
                0.0
            };
            score += freq_boost;

            // ── Depth penalty: softer, max 10 pts ───────────────────────────
            let path_depth = doc.path.matches('/').count() as f32;
            let depth_penalty = ((path_depth - 3.0).max(0.0) * SCORE_DEPTH_PENALTY_PER_LEVEL)
                .min(SCORE_DEPTH_PENALTY_MAX);
            score -= depth_penalty;

            // ── Path segment bonus (lazily computed from path to save RAM) ──
            let path_bonus: f32 = if query_terms.iter().any(|t| t.len() >= 3) {
                let dir = Path::new(&doc.path)
                    .parent()
                    .map(|p| p.to_string_lossy().to_lowercase())
                    .unwrap_or_default();
                query_terms
                    .iter()
                    .filter(|t| t.len() >= 3)
                    .map(|t| if dir.contains(t.as_str()) { 8.0 } else { 0.0 })
                    .sum::<f32>()
                    .min(SCORE_PATH_SEGMENT_MAX)
            } else {
                0.0
            };
            score += path_bonus;

            heap.push(ScoredDoc { doc_id, score });
            if heap.len() > max {
                heap.pop(); // remove lowest
            }
        }

        if skipped > 0 {
            tracing::debug!("query early-exit: skipped {} of {} candidates", skipped, candidate_count);
        }

        let mut results: Vec<ScoredDoc> = heap.into_vec();
        results.sort_by(|a, b| {
            let score_diff = b.score - a.score;
            if score_diff.abs() < 0.5 {
                let pa = &self.docs[a.doc_id as usize].path;
                let pb = &self.docs[b.doc_id as usize].path;
                pa.len().cmp(&pb.len()).then_with(|| pa.cmp(pb))
            } else {
                b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal)
            }
        });
        results
    }

    /// Candidate doc ids for a single query term.
    ///
    /// For terms with at least 2 trigrams we **intersect** bitmaps (fast O(1) lookup).
    /// Intersection (not union) is used because a doc must contain *all* trigrams
    /// of the term to be a plausible match — this gives higher precision.
    /// For very short terms we fall back to a bounded scan of docs whose name
    /// actually contains the term, ordered by recency.
    fn candidates_for_term(&self, term: &str) -> Vec<u32> {
        self.candidates_for_term_with_tier(term, 0b111)
    }

    fn candidates_for_term_with_tier(&self, term: &str, tier_mask: u8) -> Vec<u32> {
        // Extension query shortcut: ".pdf" → match doc.extension == "pdf"
        if let Some(ext) = term.strip_prefix('.') {
            return self
                .docs
                .iter()
                .enumerate()
                .filter(|(i, _)| !self.is_deleted(*i as u32))
                .filter(|(_, d)| Self::tier_matches(d.tier, tier_mask))
                .filter(|(_, d)| d.extension == ext)
                .map(|(i, _)| i as u32)
                .collect();
        }

        let trigrams = extract_trigrams(term);
        if trigrams.len() < 2 {
            // Short term: bounded scan of all docs — scoring phase will filter
            // via acronym/prefix/contains. Limit to avoid flooding scoring.
            let mut heap: BinaryHeap<(std::cmp::Reverse<u64>, u32)> =
                BinaryHeap::with_capacity(SHORT_QUERY_CANDIDATE_LIMIT + 1);
            for (i, doc) in self.docs.iter().enumerate() {
                if self.is_deleted(i as u32) {
                    continue;
                }
                if !Self::tier_matches(doc.tier, tier_mask) {
                    continue;
                }
                heap.push((std::cmp::Reverse(doc.mtime), i as u32));
                if heap.len() > SHORT_QUERY_CANDIDATE_LIMIT {
                    heap.pop();
                }
            }
            let mut ids: Vec<u32> = heap.into_iter().map(|(_, id)| id).collect();
            ids.sort_by_key(|&id| std::cmp::Reverse(self.docs[id as usize].mtime));
            ids
        } else {
            let mut candidates: Option<RoaringBitmap> = None;
            for tri in &trigrams {
                if let Some(bitmap) = self.index.get(tri) {
                    let filtered: RoaringBitmap = bitmap
                        .iter()
                        .filter(|&id| !self.is_deleted(id))
                        .filter(|&id| Self::tier_matches(self.docs[id as usize].tier, tier_mask))
                        .collect();
                    match candidates {
                        None => candidates = Some(filtered),
                        Some(ref mut c) => *c = &*c & &filtered,
                    }
                }
            }
            candidates.map(|b| b.iter().collect()).unwrap_or_default()
        }
    }

    /// BM25 score combining name field and body field.
    ///
    /// Name field contributes at full weight; body field at 0.4 weight so body
    /// matches never outrank strong name matches.
    fn bm25_score(&self, doc_id: u32, query_terms: &[String]) -> f32 {
        let doc = &self.docs[doc_id as usize];
        let name_score = self.bm25_field(doc_id, &doc.tokens, query_terms);
        let body_score = if doc.body_tokens.is_empty() {
            0.0
        } else {
            self.bm25_field(doc_id, &doc.body_tokens, query_terms) * 0.4
        };
        name_score + body_score
    }

    /// BM25 for a single tokenized field.
    fn bm25_field(&self, _doc_id: u32, field_tokens: &[IStr], query_terms: &[String]) -> f32 {
        if query_terms.is_empty() || field_tokens.is_empty() {
            return 0.0;
        }

        let n = self.active_doc_count as f32;
        let doc_len = field_tokens.len() as f32;
        let avgdl = self.avg_doc_len.max(1.0);
        let norm = 1.0 - B + B * (doc_len / avgdl);

        let mut score = 0.0;
        for term in query_terms {
            let tf = field_tokens.iter().filter(|&t| t.as_ref() == term.as_str()).count() as f32;
            if tf == 0.0 {
                continue;
            }

            let df = *self.doc_freq.get(term).unwrap_or(&1) as f32;
            let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();

            let numerator = tf * (K1 + 1.0);
            let denominator = tf + K1 * norm;
            score += idf * (numerator / denominator);
        }

        score
    }

    /// Convert a scored doc to a SearchResult.
    ///
    /// Normalizes the internal raw score to the range `[0, 1]` for the Swift client.
    pub fn to_result(&self, scored: &ScoredDoc) -> SearchResult {
        let doc = &self.docs[scored.doc_id as usize];
        let normalized_score = (scored.score / SCORE_NORMALIZATION_DIVISOR).clamp(0.0, 1.0);
        SearchResult {
            path: doc.path.clone(),
            name: Path::new(&doc.path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string(),
            kind: doc.kind,
            size: doc.size,
            modified: doc.mtime,
            score: normalized_score,
        }
    }

    /// Incremental update: add a new file.
    ///
    /// Skips if the path is already indexed. Updates `doc_freq`, trigram bitmaps,
    /// `active_doc_count`, and `avg_doc_len` incrementally.
    pub fn add(&mut self, path: &Path) {
        // Skip symlinks to avoid double-indexing the same file via multiple paths
        if path.is_symlink() {
            return;
        }

        let path_str = path.to_string_lossy().to_string();

        // Path may already exist in path_to_id if it was previously removed
        // (tombstoned but not yet compacted). Re-indexing a tombstoned path
        // is valid — the watcher calls remove() then add() on file writes.
        if let Some(&existing_id) = self.path_to_id.get(&path_str) {
            if !self.docs[existing_id as usize].deleted {
                return; // Active doc with this path already exists — skip
            }
            // Tombstoned entry: remove from map so we can re-insert cleanly below
            self.path_to_id.remove(&path_str);
        }

        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        let name_lower = name.to_lowercase();

        let (size, mtime) = std::fs::metadata(path).map_or((0, 0), |m| {
            let mtime = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            (m.len(), mtime)
        });

        let kind = if is_app_bundle(path) {
            ResultKind::Application
        } else {
            kind_from_path(path)
        };

        let terms = tokenize(&name_lower);
        let mut seen_terms = HashSet::new();
        for term in &terms {
            if seen_terms.insert(term.clone()) {
                *self.doc_freq.entry(term.clone()).or_insert(0) += 1;
            }
        }

        let acronym = extract_acronym(&name_lower);
        let extension = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();

        let doc_id = self.docs.len() as u32;
        self.docs.push(DocEntry {
            path: path_str.clone(),
            name_lower: name_lower.clone(),
            acronym,
            tokens: terms.iter().map(|s| IStr::from(s.as_str())).collect(),
            body_lower: String::new(),
            body_tokens: smallvec::SmallVec::new(),
            kind,
            mtime,
            size,
            deleted: false,
            extension,
            access_count: 0,
            last_accessed: 0,
            tier: DocTier::Normal,
        });
        self.path_to_id.insert(path_str, doc_id);

        // Update active_doc_count and avg_doc_len incrementally
        self.active_doc_count += 1;
        self.total_doc_len += terms.len() as f32;
        self.avg_doc_len = self.total_doc_len / self.active_doc_count as f32;

        // Insert trigrams
        let trigrams = extract_trigrams(&name_lower);
        for tri in trigrams {
            self.index.entry(tri).or_default().insert(doc_id);
        }
    }

    /// Index body text for a path that already exists in the index.
    ///
    /// Idempotent: calling again with the same path replaces the old body.
    /// No-op if the path does not exist or the doc has been tombstoned.
    /// Body trigrams are added to the same bitmap as name trigrams so the
    /// doc appears in candidate sets when the query matches body text.
    pub fn index_content(&mut self, path: &str, body: &str) {
        let Some(&doc_id) = self.path_to_id.get(path) else { return };
        if self.docs[doc_id as usize].deleted {
            return;
        }

        // Remove old body tokens from doc_freq
        let old_tokens = self.docs[doc_id as usize].body_tokens.clone();
        let mut seen = HashSet::new();
        for token in &old_tokens {
            if seen.insert(token.clone()) {
                if let Some(count) = self.doc_freq.get_mut(token.as_ref()) {
                    *count = count.saturating_sub(1);
                    if *count == 0 {
                        self.doc_freq.remove(token.as_ref());
                    }
                }
            }
        }
        self.total_doc_len -= old_tokens.len() as f32;

        // Tokenize new body
        let body_lower = body.to_lowercase();
        let new_tokens: smallvec::SmallVec<[IStr; 4]> =
            tokenize(&body_lower).iter().map(|s| IStr::from(s.as_str())).collect();

        // Update doc_freq with new tokens
        let mut seen2 = HashSet::new();
        for token in &new_tokens {
            if seen2.insert(token.clone()) {
                *self.doc_freq.entry(token.to_string()).or_insert(0) += 1;
            }
        }

        // Insert body trigrams into index
        let trigrams = extract_trigrams(&body_lower);
        for tri in trigrams {
            self.index.entry(tri).or_default().insert(doc_id);
        }

        self.total_doc_len += new_tokens.len() as f32;
        self.avg_doc_len = self.total_doc_len / self.active_doc_count.max(1) as f32;

        let doc = &mut self.docs[doc_id as usize];
        doc.body_lower = body_lower;
        doc.body_tokens = new_tokens;
    }

    /// Atomic update: remove existing doc at path (if any) then re-add it.
    /// Used by the file watcher for Modified events — cheaper than separate
    /// remove() + add() because it only acquires one write-lock session.
    pub fn update(&mut self, path: &Path) {
        self.remove(path);
        if path.exists() {
            self.add(path);
        }
    }

    /// Record that a document was returned in search results.
    /// Increments access_count and updates last_accessed timestamp.
    pub fn record_access(&mut self, path: &str) {
        if let Some(&doc_id) = self.path_to_id.get(path) {
            let doc = &mut self.docs[doc_id as usize];
            doc.access_count = doc.access_count.saturating_add(1);
            doc.last_accessed = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
        }
    }

    /// Incremental update: mark as deleted.
    /// Also decrements doc_freq for each unique term of the removed document
    /// and updates active_doc_count / total_doc_len so BM25 stays accurate.
    pub fn remove(&mut self, path: &Path) {
        let path_str = path.to_string_lossy().to_string();
        if let Some(&doc_id) = self.path_to_id.get(&path_str) {
            // Scope 1: read everything we need from the doc.
            // The immutable borrow ends here so we can mutate self freely below.
            let (tokens, token_count) = {
                let doc = &self.docs[doc_id as usize];
                if doc.deleted {
                    return; // idempotent
                }
                (doc.tokens.clone(), doc.tokens.len() as f32)
            };

            // Scope 2: mutate index-wide counters.
            let mut seen = HashSet::new();
            for term in &tokens {
                if seen.insert(term.clone()) {
                    if let Some(count) = self.doc_freq.get_mut(term.as_ref()) {
                        *count = count.saturating_sub(1);
                        if *count == 0 {
                            self.doc_freq.remove(term.as_ref());
                        }
                    }
                }
            }

            self.active_doc_count = self.active_doc_count.saturating_sub(1);
            self.total_doc_len = (self.total_doc_len - token_count).max(0.0);
            self.avg_doc_len = if self.active_doc_count == 0 {
                1.0
            } else {
                self.total_doc_len / self.active_doc_count as f32
            };

            self.docs[doc_id as usize].deleted = true;
            self.set_deleted(doc_id);
            self.tombstone_count += 1;

            if self.tombstone_count > 1000 {
                self.compact();
            }
        }
    }

    /// Remove tombstoned docs and rebuild bitmaps.
    ///
    /// Triggered automatically when `tombstone_count` exceeds 1,000.
    /// O(N) over the number of docs, so it should not run on every remove.
    fn compact(&mut self) {
        let mut new_docs: Vec<DocEntry> = Vec::with_capacity(self.docs.len());
        let mut old_to_new: HashMap<u32, u32> = HashMap::with_capacity(self.docs.len());

        for (old_id, doc) in self.docs.drain(..).enumerate() {
            if !doc.deleted {
                let new_id = new_docs.len() as u32;
                old_to_new.insert(old_id as u32, new_id);
                new_docs.push(doc);
            }
        }

        // Rebuild path_to_id
        self.path_to_id.clear();
        for (new_id, doc) in new_docs.iter().enumerate() {
            self.path_to_id.insert(doc.path.clone(), new_id as u32);
        }

        // Rebuild index bitmaps
        let mut new_index: HashMap<[char; 3], RoaringBitmap> =
            HashMap::with_capacity(self.index.len());
        for (tri, bitmap) in self.index.drain() {
            let new_bitmap: RoaringBitmap = bitmap
                .iter()
                .filter_map(|old_id| old_to_new.get(&old_id).copied())
                .collect();
            if !new_bitmap.is_empty() {
                new_index.insert(tri, new_bitmap);
            }
        }

        // Rebuild doc_freq and avg_doc_len
        let mut new_doc_freq: HashMap<String, u32> = HashMap::new();
        let mut total_doc_len: f32 = 0.0;
        for doc in &new_docs {
            let mut seen = HashSet::new();
            for term in &doc.tokens {
                if seen.insert(term.clone()) {
                    *new_doc_freq.entry(term.to_string()).or_insert(0) += 1;
                }
            }
            total_doc_len += doc.tokens.len() as f32;
        }
        self.doc_freq = new_doc_freq;
        self.active_doc_count = new_docs.len();
        self.total_doc_len = total_doc_len;
        self.avg_doc_len = if new_docs.is_empty() { 1.0 } else { total_doc_len / new_docs.len() as f32 };

        self.docs = new_docs;
        self.index = new_index;
        self.tombstone_count = 0;
        self.deleted_bits = vec![0u64; (self.docs.len() + 63) / 64];
    }
}

// ------------------------------------------------------------------------------
// Tokenization
// ------------------------------------------------------------------------------

/// Tokenize a filename into lowercase terms.
///
/// Splits on whitespace, underscore, hyphen, dot.
/// Also splits camelCase / PascalCase boundaries (e.g. "MyGreatApp" → "my great app").
/// Used for both document names and query strings.
pub fn tokenize(s: &str) -> Vec<String> {
    let mut result = Vec::new();
    for token in s.split(|c: char| c == ' ' || c == '_' || c == '-' || c == '.') {
        if token.is_empty() {
            continue;
        }
        let split = split_camel_case(token);
        for word in split.split_whitespace() {
            if !word.is_empty() {
                result.push(word.to_lowercase());
            }
        }
    }
    result
}

/// Split a camelCase / PascalCase token into space-separated words.
fn split_camel_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    let chars: Vec<char> = s.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        if i > 0 && c.is_uppercase() {
            out.push(' ');
        }
        out.push(*c);
    }
    out
}

// ------------------------------------------------------------------------------
// Trigrams (Unicode-safe)
// ------------------------------------------------------------------------------

/// Extract all trigrams from a string using Unicode scalars.
fn extract_trigrams(s: &str) -> Vec<[char; 3]> {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() < 3 {
        return Vec::new();
    }
    let mut trigrams = Vec::with_capacity(chars.len().saturating_sub(2));
    for i in 0..=chars.len() - 3 {
        trigrams.push([chars[i], chars[i + 1], chars[i + 2]]);
    }
    trigrams
}

// ------------------------------------------------------------------------------
// Acronym extraction
// ------------------------------------------------------------------------------

/// Extract acronym from a filename: first char of each word after splitting on
/// common separators and camelCase boundaries.
/// "visual_studio_code" -> "vsc"
/// "iTerm2 Preferences" -> "ip"
/// "MyGreatApp" -> "mga"
pub(crate) fn extract_acronym(name_lower: &str) -> String {
    tokenize(name_lower)
        .iter()
        .filter_map(|token| token.chars().next())
        .collect()
}

// ------------------------------------------------------------------------------
// Word boundary check (Unicode-safe)
// ------------------------------------------------------------------------------

/// Check if `name` starts with `query` at a word boundary (Unicode-safe).
fn word_boundary_starts_with(name: &str, query: &str) -> bool {
    if name.starts_with(query) {
        return true;
    }
    let name_chars: Vec<char> = name.chars().collect();
    let query_chars: Vec<char> = query.chars().collect();
    if query_chars.is_empty() || name_chars.len() < query_chars.len() {
        return false;
    }
    for i in 0..=name_chars.len() - query_chars.len() {
        if i > 0 && !is_word_boundary_char(name_chars[i - 1]) {
            continue;
        }
        if name_chars[i..i + query_chars.len()] == query_chars[..] {
            return true;
        }
    }
    false
}

fn is_word_boundary_char(c: char) -> bool {
    matches!(c, ' ' | '.' | '_' | '-' | '/' | '\\' | '(' | '[' | '{' | '@')
}

// ------------------------------------------------------------------------------
// Tests
// ------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_index_with(docs: &[(String, String, u64)]) -> TrigramIndex {
        let mut index = TrigramIndex {
            docs: Vec::new(),
            index: HashMap::new(),
            tombstone_count: 0,
            path_to_id: HashMap::new(),
            doc_freq: HashMap::new(),
            avg_doc_len: 1.0,
            active_doc_count: 0,
            total_doc_len: 0.0,
            deleted_bits: Vec::new(),
        };
        for (path, name, mtime) in docs {
            let name_lower = name.to_lowercase();
            let acronym = extract_acronym(&name_lower);
            let terms = tokenize(&name_lower);
            let mut seen = HashSet::new();
            for term in &terms {
                if seen.insert(term.clone()) {
                    *index.doc_freq.entry(term.clone()).or_insert(0) += 1;
                }
            }
            let doc_id = index.docs.len() as u32;
            let extension = Path::new(path.as_str())
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();
            index.docs.push(DocEntry {
                path: path.clone(),
                name_lower: name_lower.clone(),
                acronym,
                tokens: terms.iter().map(|s| IStr::from(s.as_str())).collect(),
                body_lower: String::new(),
                body_tokens: smallvec::SmallVec::new(),
                kind: ResultKind::File,
                mtime: *mtime,
                size: 0,
                deleted: false,
                extension,
                access_count: 0,
                last_accessed: 0,
                tier: DocTier::Normal,
            });
            index.path_to_id.insert(path.clone(), doc_id);
            let trigrams = extract_trigrams(&name_lower);
            for tri in trigrams {
                index.index.entry(tri).or_default().insert(doc_id);
            }
        }
        let total_doc_len: f32 = index.docs.iter().map(|d| d.tokens.len() as f32).sum();
        index.avg_doc_len = if index.docs.is_empty() { 1.0 } else { total_doc_len / index.docs.len() as f32 };
        index.active_doc_count = index.docs.len();
        index.total_doc_len = total_doc_len;
        index
    }

    #[test]
    fn test_exact_match_scores_highest() {
        let idx = make_index_with(&[
            ("/a/foo.txt".into(), "foo".into(), 0),
            ("/b/foobar.txt".into(), "foobar".into(), 0),
            ("/c/bar.txt".into(), "bar".into(), 0),
        ]);
        let scored = idx.query("foo", 10, true);
        assert_eq!(scored[0].doc_id, 0); // exact match "foo"
    }

    #[test]
    fn test_prefix_beats_contains() {
        let idx = make_index_with(&[
            ("/a/xcode.app".into(), "Xcode".into(), 0),
            ("/b/com.example.xco.plist".into(), "com.example.xco".into(), 0),
        ]);
        let scored = idx.query("xco", 10, true);
        assert_eq!(scored[0].doc_id, 0); // "Xcode" prefix beats contains
    }

    #[test]
    fn test_recency_bonus() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let idx = make_index_with(&[
            ("/a/old.txt".into(), "report".into(), now - 5_184_000), // 60 days ago
            ("/b/new.txt".into(), "report".into(), now - 1_800),    // 30 min ago
        ]);
        let scored = idx.query("report", 10, true);
        assert_eq!(scored[0].doc_id, 1); // newer file wins
    }

    #[test]
    fn test_depth_penalty() {
        let idx = make_index_with(&[
            ("/Applications/Foo.app".into(), "Foo".into(), 0),
            ("/Users/x/a/b/c/d/Foo.app".into(), "Foo".into(), 0),
        ]);
        let scored = idx.query("foo", 10, true);
        assert_eq!(scored[0].doc_id, 0); // shallow path wins
    }

    #[test]
    fn test_short_query_uses_recent_docs() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let idx = make_index_with(&[
            ("/a/foo.txt".into(), "foo".into(), now - 1_000),
            ("/b/bar.txt".into(), "bar".into(), now - 4_000), // > 1h old
            ("/c/baz.txt".into(), "baz".into(), now - 100),
        ]);
        // 2-char query => short-query path; "baz" is most recent and starts with "ba"
        let scored = idx.query("ba", 10, true);
        assert_eq!(scored[0].doc_id, 2); // most recent doc wins
    }

    #[test]
    fn test_recency_does_not_promote_irrelevant_file() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let idx = make_index_with(&[
            ("/Applications/Xcode.app".into(), "Xcode".into(), now - 86400 * 10), // 10 days old
            ("/tmp/zxqwerty.txt".into(), "zxqwerty".into(), now - 60),             // 1 min old, irrelevant
        ]);
        let scored = idx.query("xco", 10, true);
        // Xcode matches "xco" (prefix), zxqwerty does not — Xcode must rank first
        // regardless of recency
        assert_eq!(scored[0].doc_id, 0);
    }

    #[test]
    fn test_multi_term_query_both_terms_required() {
        let idx = make_index_with(&[
            ("/a/xcode_docs.txt".into(), "xcode_docs".into(), 0),
            ("/b/xcode.app".into(), "Xcode".into(), 0),
            ("/c/documentation.pdf".into(), "documentation".into(), 0),
        ]);
        // "xcode doc" should rank xcode_docs.txt first (contains both "xcode" and "doc")
        let scored = idx.query("xcode doc", 10, true);
        assert_eq!(scored[0].doc_id, 0);
    }

    #[test]
    fn test_multi_term_candidate_intersection() {
        let idx = make_index_with(&[
            ("/a/meeting_notes.txt".into(), "meeting_notes".into(), 0),
            ("/b/meeting.txt".into(), "meeting".into(), 0),
            ("/c/notes.txt".into(), "notes".into(), 0),
        ]);
        // "meeting notes" — only doc 0 has both terms
        let scored = idx.query("meeting notes", 10, true);
        assert!(!scored.is_empty());
        assert_eq!(scored[0].doc_id, 0);
    }

    #[test]
    fn test_path_segment_bonus() {
        let idx = make_index_with(&[
            ("/x/myproject/src/main.rs".into(), "main.rs".into(), 0),
            ("/x/other/lib/main.rs".into(), "main.rs".into(), 0),
        ]);
        // "src main" — first file has "src" in parent dir, should rank higher
        let scored = idx.query("src main", 10, true);
        assert_eq!(scored[0].doc_id, 0);
    }

    #[test]
    fn test_acronym_exact_match() {
        let idx = make_index_with(&[
            ("/a/VisualStudioCode.app".into(), "Visual Studio Code".into(), 0),
            ("/b/VideoStreamCapture.app".into(), "VideoStreamCapture".into(), 0),
            ("/c/something_else.app".into(), "SomethingElse".into(), 0),
        ]);
        let scored = idx.query("vsc", 10, true);
        assert!(scored.iter().any(|s| s.doc_id == 0));
        assert_eq!(scored[0].doc_id, 0);
    }

    #[test]
    fn test_acronym_prefix_match() {
        let idx = make_index_with(&[
            ("/a/iTerm2Preferences.app".into(), "iTerm2 Preferences".into(), 0),
            ("/b/internet_photos.txt".into(), "internet_photos".into(), 0),
        ]);
        // "it" matches acronym prefix of "itp" (iTerm2 Preferences)
        let scored = idx.query("it", 10, true);
        assert_eq!(scored[0].doc_id, 0);
    }

    #[test]
    fn test_extract_acronym() {
        assert_eq!(extract_acronym("visual_studio_code"), "vsc");
        assert_eq!(extract_acronym("MyGreatApp"), "mga");
        assert_eq!(extract_acronym("iterm2 preferences"), "ip");
    }

    #[test]
    fn test_tie_breaking_shorter_path_wins() {
        let idx = make_index_with(&[
            ("/Applications/Zoom.app".into(), "Zoom".into(), 0),
            ("/Users/x/Applications/Zoom.app".into(), "Zoom".into(), 0),
        ]);
        let scored = idx.query("zoom", 10, true);
        // Both match equally on name; shorter path should win
        assert_eq!(scored[0].doc_id, 0);
    }

    #[test]
    fn test_normalized_score_in_range() {
        let idx = make_index_with(&[
            ("/a/foo.txt".into(), "foo".into(), 0),
        ]);
        let scored = idx.query("foo", 10, true);
        assert!(!scored.is_empty());
        let result = idx.to_result(&scored[0]);
        assert!(result.score >= 0.0 && result.score <= 1.0,
            "score {} out of [0,1] range", result.score);
    }

    #[test]
    fn test_remove_decrements_doc_freq() {
        let mut idx = make_index_with(&[
            ("/a/foobar.txt".into(), "foobar".into(), 0),
        ]);
        let initial = *idx.doc_freq.get("foobar").unwrap_or(&0);
        assert_eq!(initial, 1);
        idx.remove(std::path::Path::new("/a/foobar.txt"));
        assert_eq!(idx.doc_freq.get("foobar"), None); // cleaned up zero entries
    }

    #[test]
    fn test_remove_updates_active_doc_count() {
        let idx = make_index_with(&[
            ("/a/alpha.txt".into(), "alpha".into(), 0),
            ("/b/beta.txt".into(), "beta".into(), 0),
            ("/c/gamma.txt".into(), "gamma".into(), 0),
        ]);
        let mut idx = idx;
        assert_eq!(idx.active_doc_count, 3);
        idx.remove(std::path::Path::new("/b/beta.txt"));
        assert_eq!(idx.active_doc_count, 2);
    }

    #[test]
    fn test_remove_is_idempotent() {
        let idx = make_index_with(&[
            ("/a/foo.txt".into(), "foo".into(), 0),
        ]);
        let mut idx = idx;
        idx.remove(std::path::Path::new("/a/foo.txt"));
        let count_after_first = idx.active_doc_count;
        idx.remove(std::path::Path::new("/a/foo.txt")); // second remove should be no-op
        assert_eq!(idx.active_doc_count, count_after_first);
    }

    #[test]
    fn test_add_then_remove_then_add_cycle_maintains_correct_doc_freq() {
        let mut idx = make_index_with(&[
            ("/a/report.txt".into(), "report".into(), 0),
        ]);
        assert_eq!(*idx.doc_freq.get("report").unwrap_or(&0), 1);

        idx.remove(std::path::Path::new("/a/report.txt"));
        assert_eq!(idx.doc_freq.get("report"), None);

        // Re-add (simulate file write)
        // Note: add() checks path_to_id, but the doc is still there (tombstoned).
        // In real usage compact() would run first. For this test we just verify
        // that after a remove the freq is correct, and after a fresh build it stays at 1.
        let idx2 = make_index_with(&[
            ("/a/report.txt".into(), "report".into(), 0),
        ]);
        assert_eq!(*idx2.doc_freq.get("report").unwrap_or(&0), 1);
    }

    #[test]
    fn test_compact_rebuilds_doc_freq_correctly() {
        let idx = make_index_with(&[
            ("/a/alpha.txt".into(), "alpha".into(), 0),
            ("/b/alpha_beta.txt".into(), "alpha_beta".into(), 0),
            ("/c/gamma.txt".into(), "gamma".into(), 0),
        ]);
        let mut idx = idx;
        // "alpha" appears in 2 docs initially (alpha, alpha_beta)
        assert_eq!(*idx.doc_freq.get("alpha").unwrap_or(&0), 2);

        idx.remove(std::path::Path::new("/a/alpha.txt"));
        // After compact the freq should still be correct
        assert_eq!(*idx.doc_freq.get("alpha").unwrap_or(&0), 1); // only alpha_beta remains
        assert_eq!(*idx.doc_freq.get("gamma").unwrap_or(&0), 1);
    }

    #[test]
    fn test_bm25_n_excludes_tombstones() {
        let idx = make_index_with(&[
            ("/a/foo.txt".into(), "foo".into(), 0),
            ("/b/foo2.txt".into(), "foo".into(), 0),
            ("/c/bar.txt".into(), "bar".into(), 0),
        ]);
        let mut idx = idx;
        assert_eq!(idx.active_doc_count, 3);

        idx.remove(std::path::Path::new("/c/bar.txt"));

        assert_eq!(idx.active_doc_count, 2);
        let results = idx.query("foo", 10, true);
        assert!(!results.is_empty());
    }

    #[test]
    fn test_avg_doc_len_accurate_after_removes() {
        let idx = make_index_with(&[
            ("/a/hello_world.txt".into(), "hello_world".into(), 0),   // 2 tokens
            ("/b/foo.txt".into(), "foo".into(), 0),                    // 1 token
        ]);
        let mut idx = idx;
        // avg = (2+1)/2 = 1.5
        assert!((idx.avg_doc_len - 1.5).abs() < 0.01, "avg_doc_len was {}", idx.avg_doc_len);

        idx.remove(std::path::Path::new("/a/hello_world.txt"));
        // avg should now be 1.0 (only foo.txt with 1 token remains)
        assert!((idx.avg_doc_len - 1.0).abs() < 0.01, "avg_doc_len after remove was {}", idx.avg_doc_len);
    }

    #[test]
    fn test_total_doc_len_never_goes_negative() {
        let mut idx = make_index_with(&[
            ("/a/foo.txt".into(), "foo".into(), 0),
        ]);
        // Remove the only doc — total_doc_len must not go below 0
        idx.remove(std::path::Path::new("/a/foo.txt"));
        assert!(idx.total_doc_len >= 0.0,
            "total_doc_len should never be negative, got {}", idx.total_doc_len);
        assert!(idx.avg_doc_len >= 0.0,
            "avg_doc_len should never be negative, got {}", idx.avg_doc_len);
    }

    #[test]
    fn test_add_after_remove_same_path_restores_count() {
        let mut idx = make_index_with(&[
            ("/a/foo.txt".into(), "foo".into(), 0),
            ("/b/bar.txt".into(), "bar".into(), 0),
        ]);
        assert_eq!(idx.active_doc_count, 2);

        idx.remove(std::path::Path::new("/a/foo.txt"));
        assert_eq!(idx.active_doc_count, 1);

        // add() must not skip a tombstoned path — re-index it
        idx.add(std::path::Path::new("/a/foo.txt"));
        assert_eq!(idx.active_doc_count, 2,
            "active_doc_count should restore after add on tombstoned path");

        // avg_doc_len should be positive and consistent
        assert!(idx.avg_doc_len > 0.0,
            "avg_doc_len went non-positive after add: {}", idx.avg_doc_len);
    }

    #[test]
    fn test_build_skips_symlinks() {
        use std::os::unix::fs::symlink;
        let dir = std::env::temp_dir().join("snyd_test_symlink");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("real.txt"), "content").unwrap();
        let _ = symlink(dir.join("real.txt"), dir.join("link.txt"));

        let idx = TrigramIndex::build(&[dir.clone()]);
        // Should index 1 file (real.txt), not 2 (would double-count via symlink)
        assert_eq!(idx.active_doc_count, 1,
            "symlink should not be indexed separately: got {} docs", idx.active_doc_count);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_index_doc_count_does_not_exceed_cap() {
        assert!(MAX_INDEX_DOCS >= 100_000, "cap too low for normal use");
        assert!(MAX_INDEX_DOCS <= 2_000_000, "cap too high, memory risk");
    }

    #[test]
    fn test_index_content_makes_body_searchable() {
        let mut idx = make_index_with(&[("/a/photo.png".into(), "photo.png".into(), 0)]);
        idx.index_content("/a/photo.png", "starbucks receipt 2024");
        let results = idx.query("starbucks", 10, true);
        assert!(!results.is_empty(), "body text should be searchable after index_content");
        assert_eq!(results[0].doc_id, 0);
    }

    #[test]
    fn test_index_content_skips_unknown_path() {
        let mut idx = make_index_with(&[("/a/photo.png".into(), "photo.png".into(), 0)]);
        idx.index_content("/nonexistent/file.png", "some text");
        assert_eq!(idx.active_doc_count, 1, "unknown path must not create new doc");
    }

    #[test]
    fn test_index_content_overwrites_old_body() {
        let mut idx = make_index_with(&[("/a/img.png".into(), "img.png".into(), 0)]);
        idx.index_content("/a/img.png", "old body coffee");
        idx.index_content("/a/img.png", "new body matcha");
        let coffee = idx.query("coffee", 10, true);
        let matcha = idx.query("matcha", 10, true);
        assert!(matcha.len() >= coffee.len(),
            "new body should replace old: coffee={}, matcha={}", coffee.len(), matcha.len());
    }

    // ── Tier classification tests ────────────────────────────────────────────

    #[test]
    fn test_classify_path_hidden_without_opt_in_is_excluded() {
        let config = IndexConfig {
            scopes: vec![PathBuf::from("/home/user")],
            index_hidden: false,
            index_cache: false,
            custom_excludes: vec![],
            custom_includes: vec![],
        };
        assert_eq!(classify_path(Path::new("/home/user/.config/nvim/init.lua"), &config), None);
        assert_eq!(classify_path(Path::new("/home/user/.bashrc"), &config), None);
        assert_eq!(
            classify_path(Path::new("/home/user/Documents/file.txt"), &config),
            Some(DocTier::Normal)
        );
    }

    #[test]
    fn test_classify_path_hidden_with_opt_in_is_included() {
        let config = IndexConfig {
            scopes: vec![PathBuf::from("/home/user")],
            index_hidden: true,
            index_cache: false,
            custom_excludes: vec![],
            custom_includes: vec![],
        };
        assert_eq!(
            classify_path(Path::new("/home/user/.config/nvim/init.lua"), &config),
            Some(DocTier::Hidden)
        );
        assert_eq!(
            classify_path(Path::new("/home/user/.bashrc"), &config),
            Some(DocTier::Hidden)
        );
    }

    #[test]
    fn test_classify_path_cache_with_opt_in_is_included() {
        let config = IndexConfig {
            scopes: vec![PathBuf::from("/home/user")],
            index_hidden: false,
            index_cache: true,
            custom_excludes: vec![],
            custom_includes: vec![],
        };
        assert_eq!(
            classify_path(Path::new("/home/user/.cargo/registry/src/foo/bar.rs"), &config),
            Some(DocTier::Cache)
        );
        assert_eq!(
            classify_path(Path::new("/home/user/project/node_modules/lodash/index.js"), &config),
            Some(DocTier::Cache)
        );
    }

    #[test]
    fn test_classify_path_custom_include_overrides() {
        let config = IndexConfig {
            scopes: vec![PathBuf::from("/home/user")],
            index_hidden: false,
            index_cache: false,
            custom_excludes: vec![],
            custom_includes: vec![".config/nvim".to_string()],
        };
        // Even though it's hidden, custom_include forces Normal tier
        assert_eq!(
            classify_path(Path::new("/home/user/.config/nvim/init.lua"), &config),
            Some(DocTier::Normal)
        );
    }

    #[test]
    fn test_classify_path_custom_exclude_takes_priority() {
        let config = IndexConfig {
            scopes: vec![PathBuf::from("/home/user")],
            index_hidden: true,
            index_cache: true,
            custom_excludes: vec!["secret".to_string()],
            custom_includes: vec![],
        };
        assert_eq!(
            classify_path(Path::new("/home/user/secret/password.txt"), &config),
            None
        );
    }

    #[test]
    fn test_query_with_tier_filters_by_tier_mask() {
        let mut idx = make_index_with(&[
            ("/a/normal.txt".into(), "budget".into(), 0),
            ("/b/.hidden.txt".into(), "budget".into(), 0),
            ("/c/node_modules/pkg/index.js".into(), "budget".into(), 0),
        ]);
        // Manually set tiers
        idx.docs[0].tier = DocTier::Normal;
        idx.docs[1].tier = DocTier::Hidden;
        idx.docs[2].tier = DocTier::Cache;
        // Rebuild path_to_id and index after manual mutation (tokens already set)
        idx.path_to_id = idx.docs.iter().enumerate().map(|(i, d)| (d.path.clone(), i as u32)).collect();
        idx.index.clear();
        for (i, doc) in idx.docs.iter().enumerate() {
            for tri in extract_trigrams(&doc.name_lower) {
                idx.index.entry(tri).or_default().insert(i as u32);
            }
        }

        // Default mask (Normal only) → 1 result
        let normal_only = idx.query_with_tier("budget", 10, true, 0b001);
        assert_eq!(normal_only.len(), 1);
        assert_eq!(normal_only[0].doc_id, 0);

        // Hidden included → 2 results
        let with_hidden = idx.query_with_tier("budget", 10, true, 0b011);
        let ids: Vec<u32> = with_hidden.iter().map(|s| s.doc_id).collect();
        assert!(ids.contains(&0));
        assert!(ids.contains(&1));
        assert!(!ids.contains(&2));

        // All tiers → 3 results
        let all = idx.query_with_tier("budget", 10, true, 0b111);
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn test_query_default_includes_all_tiers() {
        let mut idx = make_index_with(&[
            ("/a/normal.txt".into(), "report".into(), 0),
            ("/b/.hidden.txt".into(), "report".into(), 0),
        ]);
        idx.docs[0].tier = DocTier::Normal;
        idx.docs[1].tier = DocTier::Hidden;
        idx.path_to_id = idx.docs.iter().enumerate().map(|(i, d)| (d.path.clone(), i as u32)).collect();
        idx.index.clear();
        for (i, doc) in idx.docs.iter().enumerate() {
            for tri in extract_trigrams(&doc.name_lower) {
                idx.index.entry(tri).or_default().insert(i as u32);
            }
        }
        // query() default mask is 0b111 — all tiers
        let all = idx.query("report", 10, true);
        assert_eq!(all.len(), 2);
    }
}
