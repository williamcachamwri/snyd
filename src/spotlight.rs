use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::task::{Context, Poll};

use futures::Stream;
use strsim::jaro_winkler;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tracing::{error, warn};

use crate::kinds::kind_from_path;
use crate::protocol::SearchResult;

/// Escape special characters in a Spotlight (mdfind) predicate.
fn escape_spotlight_predicate(query: &str) -> String {
    query
        .replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace('*', "\\*")
        .replace('?', "\\?")
        .replace('(', "\\(")
        .replace(')', "\\)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_escape_star() {
        assert_eq!(escape_spotlight_predicate("hello*world"), "hello\\*world");
    }

    #[test]
    fn test_escape_quote() {
        assert_eq!(escape_spotlight_predicate("it's"), "it\\'s");
    }

    #[test]
    fn test_escape_parens() {
        assert_eq!(escape_spotlight_predicate("a(b)"), "a\\(b\\)");
    }

    #[test]
    fn test_score_relevant_result_higher_than_irrelevant() {
        let score_relevant = jaro_winkler("notes", "notes") as f32 * 0.5;
        let score_irrelevant = jaro_winkler("notes", "zxqwerty") as f32 * 0.5;
        assert!(
            score_relevant > score_irrelevant,
            "relevant spotlight result should score higher: {} vs {}",
            score_relevant,
            score_irrelevant
        );
    }
}

// ═════════════════════════════════════════════════════════════════════════════
// macOS: mdfind
// ═════════════════════════════════════════════════════════════════════════════

#[cfg(target_os = "macos")]
pub async fn search(
    query: &str,
    scopes: &[PathBuf],
    max: usize,
) -> impl Stream<Item = SearchResult> {
    let query = query.to_string();
    let scopes = scopes.to_vec();
    MdfindStream::start(query, scopes, max).await
}

#[cfg(target_os = "macos")]
struct MdfindStream {
    lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    _handle: MdfindHandle,
    remaining: usize,
    query_lower: String,
}

#[cfg(target_os = "macos")]
struct MdfindHandle {
    child: tokio::process::Child,
}

#[cfg(target_os = "macos")]
impl Drop for MdfindHandle {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

#[cfg(target_os = "macos")]
impl MdfindStream {
    async fn start(query: String, scopes: Vec<PathBuf>, max: usize) -> Self {
        let query_lower = query.to_lowercase();
        let escaped = escape_spotlight_predicate(&query);
        let predicate = format!("kMDItemDisplayName == '*{}*'cd", escaped);

        let mut cmd = Command::new("/usr/bin/mdfind");
        cmd.arg(&predicate)
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        for scope in &scopes {
            cmd.arg("-onlyin").arg(scope);
        }

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                error!("Failed to spawn mdfind: {}", e);
                let mut dummy = Command::new("true").stdout(Stdio::piped()).spawn().unwrap();
                let stdout = dummy.stdout.take().unwrap();
                return MdfindStream {
                    lines: BufReader::new(stdout).lines(),
                    _handle: MdfindHandle { child: dummy },
                    remaining: 0,
                    query_lower,
                };
            }
        };

        let stdout = child.stdout.take().expect("piped stdout");
        let lines = BufReader::new(stdout).lines();

        MdfindStream {
            lines,
            _handle: MdfindHandle { child },
            remaining: max,
            query_lower,
        }
    }
}

#[cfg(target_os = "macos")]
impl Stream for MdfindStream {
    type Item = SearchResult;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.remaining == 0 {
            return Poll::Ready(None);
        }

        loop {
            match std::pin::Pin::new(&mut self.lines).poll_next_line(cx) {
                Poll::Ready(Ok(Some(line))) => {
                    if line.is_empty() {
                        continue;
                    }
                    let path = std::path::Path::new(&line);
                    let name = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("")
                        .to_string();

                    let kind = kind_from_path(path);
                    self.remaining -= 1;

                    let jw = jaro_winkler(&self.query_lower, &name.to_lowercase()) as f32;
                    let score = (jw * 0.5).clamp(0.1, 0.5);

                    return Poll::Ready(Some(SearchResult {
                        path: line,
                        name,
                        kind,
                        size: 0,
                        modified: 0,
                        score,
                    }));
                }
                Poll::Ready(Ok(None)) => return Poll::Ready(None),
                Poll::Ready(Err(e)) => {
                    warn!("Error reading mdfind output: {}", e);
                    return Poll::Ready(None);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

// ═════════════════════════════════════════════════════════════════════════════
// Linux: locate / plocate
// ═════════════════════════════════════════════════════════════════════════════

#[cfg(not(target_os = "macos"))]
pub async fn search(
    query: &str,
    scopes: &[PathBuf],
    max: usize,
) -> impl Stream<Item = SearchResult> {
    let query = query.to_string();
    let scopes = scopes.to_vec();
    LocateStream::start(query, scopes, max).await
}

#[cfg(not(target_os = "macos"))]
struct LocateStream {
    lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    remaining: usize,
    query_lower: String,
    scopes: Vec<PathBuf>,
}

#[cfg(not(target_os = "macos"))]
impl LocateStream {
    async fn start(query: String, scopes: Vec<PathBuf>, max: usize) -> Self {
        let query_lower = query.to_lowercase();

        // Try plocate first (faster, newer), fallback to locate
        let mut child = Command::new("plocate")
            .arg("-l")
            .arg(max.to_string())
            .arg(&query)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();

        if child.is_err() {
            child = Command::new("locate")
                .arg(&query)
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn();
        }

        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                error!("Failed to spawn locate/plocate: {}", e);
                let mut dummy = Command::new("true").stdout(Stdio::piped()).spawn().unwrap();
                let stdout = dummy.stdout.take().unwrap();
                return LocateStream {
                    lines: BufReader::new(stdout).lines(),
                    remaining: 0,
                    query_lower,
                    scopes,
                };
            }
        };

        let stdout = child.stdout.take().expect("piped stdout");
        let lines = BufReader::new(stdout).lines();

        LocateStream {
            lines,
            remaining: max,
            query_lower,
            scopes,
        }
    }
}

#[cfg(not(target_os = "macos"))]
impl Stream for LocateStream {
    type Item = SearchResult;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.remaining == 0 {
            return Poll::Ready(None);
        }

        loop {
            match std::pin::Pin::new(&mut self.lines).poll_next_line(cx) {
                Poll::Ready(Ok(Some(line))) => {
                    if line.is_empty() {
                        continue;
                    }
                    let path = std::path::Path::new(&line);

                    // Filter to requested scopes
                    let in_scope = self.scopes.is_empty()
                        || self
                            .scopes
                            .iter()
                            .any(|scope| line.starts_with(scope.to_string_lossy().as_ref()));
                    if !in_scope {
                        continue;
                    }

                    let name = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("")
                        .to_string();

                    let kind = kind_from_path(path);
                    self.remaining -= 1;

                    let jw = jaro_winkler(&self.query_lower, &name.to_lowercase()) as f32;
                    let score = (jw * 0.5).clamp(0.1, 0.5);

                    let mtime = std::fs::metadata(path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs())
                        .unwrap_or(0);

                    return Poll::Ready(Some(SearchResult {
                        path: line,
                        name,
                        kind,
                        size: 0,
                        modified: mtime,
                        score,
                    }));
                }
                Poll::Ready(Ok(None)) => return Poll::Ready(None),
                Poll::Ready(Err(e)) => {
                    warn!("Error reading locate output: {}", e);
                    return Poll::Ready(None);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}
