use serde::{Deserialize, Serialize};

/// Source type for indexed body text (OCR, calendar, contacts, messages).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContentSource {
    OcrImage,
    CalendarEvent,
    Contact,
    IMessage,
}

/// A single content entry to index (OCR text, calendar event body, etc.).
#[derive(Debug, Clone, Deserialize)]
pub struct ContentEntry {
    /// Absolute path to the file (must already exist in the trigram index).
    pub path: String,
    /// Body text extracted from the file (OCR output, event notes, message body…).
    pub body: String,
    /// Source type — used to weight the score appropriately.
    pub source: ContentSource,
}

/// Request sent by the Swift client over the Unix socket.
#[derive(Debug, Clone, Deserialize)]
pub struct SearchRequest {
    pub id: String,
    pub query: String,
    pub max_results: usize,
    pub scopes: Vec<String>,
    pub command: Option<String>,
    pub kind_filter: Option<String>,
    /// Payload for command "index_content" — empty for normal search requests.
    #[serde(default)]
    pub content_batch: Vec<ContentEntry>,
}

/// Individual search result.
#[derive(Debug, Clone, Serialize)]
pub struct SearchResult {
    pub path: String,
    pub name: String,
    pub kind: ResultKind,
    pub size: u64,
    pub modified: u64,
    pub score: f32,
}

/// Result kind mirrors the Swift enum.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResultKind {
    Application,
    Folder,
    File,
    Image,
    Video,
    Audio,
    Document,
    Archive,
    Code,
}

/// Stats about the index, returned for a `stats` command.
#[derive(Debug, Clone, Serialize)]
pub struct IndexStats {
    pub doc_count: usize,
    pub trigram_count: usize,
    pub tombstone_count: usize,
    pub avg_doc_len: f32,
}

/// Batch response streamed back to the client.
#[derive(Debug, Clone, Serialize)]
pub struct SearchBatch {
    pub id: String,
    pub results: Vec<SearchResult>,
    pub done: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stats: Option<IndexStats>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stats_serializes_correctly() {
        let stats = IndexStats {
            doc_count: 42,
            trigram_count: 100,
            tombstone_count: 3,
            avg_doc_len: 2.5,
        };
        let batch = SearchBatch {
            id: "x".to_string(),
            results: vec![],
            done: true,
            stats: Some(stats),
        };
        let json = serde_json::to_string(&batch).unwrap();
        assert!(json.contains("\"doc_count\":42"));
        assert!(json.contains("\"trigram_count\":100"));
        assert!(json.contains("\"done\":true"));
        assert!(json.contains("\"stats\":{"));
        assert!(json.contains("\"results\":[]"));
    }

    #[test]
    fn test_none_stats_not_serialized() {
        let batch = SearchBatch {
            id: "x".to_string(),
            results: vec![],
            done: true,
            stats: None,
        };
        let json = serde_json::to_string(&batch).unwrap();
        assert!(!json.contains("\"stats\""));
    }
}
