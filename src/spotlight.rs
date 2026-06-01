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
///
/// Spotlight predicates treat `*` and `?` as wildcards. If the user's query
/// contains these characters we must escape them so they match literal file
/// names rather than expanding to match everything.  `\` and `'` are escaped so
/// they do not break the predicate syntax, and `(` / `)` are escaped because
/// Spotlight uses them for grouping.
///
/// The replacement order matters: `\\` must be substituted first, otherwise the
/// backslashes we insert later would themselves be escaped.
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
        assert!(score_relevant > score_irrelevant,
            "relevant spotlight result should score higher: {} vs {}",
            score_relevant, score_irrelevant);
    }
}

/// A handle to a running mdfind subprocess that kills on drop.
pub struct MdfindHandle {
    child: tokio::process::Child,
}

impl Drop for MdfindHandle {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

/// Run a Spotlight search via mdfind and stream results.
pub async fn search(
    query: &str,
    scopes: &[PathBuf],
    max: usize,
) -> impl Stream<Item = SearchResult> {
    let query = query.to_string();
    let scopes = scopes.to_vec();
    MdfindStream::start(query, scopes, max).await
}

struct MdfindStream {
    lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    _handle: MdfindHandle,
    remaining: usize,
    query_lower: String,
}

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
                // Return an empty stream using a dummy child
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

                    // Score based on Jaro-Winkler similarity between query and filename.
                    // Capped at 0.5 so Spotlight results never outrank trigram index results
                    // (exact match in index = 500/800 ≈ 0.625).
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
