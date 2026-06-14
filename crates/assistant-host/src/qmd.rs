//! The real, node-backed [`MemoryBackend`]: shells the pinned host-side qmd
//! sidecar over the agent's own collection and parses its JSON.
//!
//! `assistant-memory` stays pure (it owns [`parse_qmd_response`] and the trait); the
//! process plumbing lives here because the host is the only place allowed to
//! shell out. The sidecar is a small node program with three subcommands, each
//! reading a JSON request on stdin and writing a JSON response on stdout:
//!
//! - `retrieve` ← `{ message, topN, dbPath }` → the search-hits shape
//!   [`parse_qmd_response`] consumes;
//! - `embed`    ← `{ dbPath, corpusDir }` → a status shape (`{ indexed }` or
//!   `{ error }`) reporting the rebuilt index;
//! - `status`   ← `{ dbPath }` → the same status shape, without a rebuild.
//!
//! Everything is fail-open: a missing `node`, a sidecar crash, a timeout, or
//! malformed output all degrade *memory health* (the host then falls back to the
//! catalog-only path), never an error that fails the turn. The process call is
//! abstracted behind [`SidecarRunner`] so the offline gate exercises the
//! fail-open branches with a fake runner, no node required.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use assistant_memory::{
    parse_qmd_response, DegradedReason, IndexDoc, MemoryBackend, MemoryHealth, SearchOutcome,
};

/// How the backend configures and reaches the sidecar. Plain data so the host
/// can carry it in `HostConfig`; [`NodeQmd::new`] turns it into a live backend.
#[derive(Clone, Debug)]
pub struct QmdSidecar {
    /// The node executable (`node`, or an absolute path).
    pub node_bin: String,
    /// The sidecar script the node binary runs (`qmd-sidecar.mjs`).
    pub sidecar_path: PathBuf,
    /// The qmd index database the sidecar opens (`<root>/qmd/index.sqlite`).
    pub db_path: PathBuf,
    /// The directory the corpus is written to, one file per entry named by the
    /// hex-encoded memory id (`<root>/qmd/corpus`). The sidecar indexes this on
    /// `embed`.
    pub corpus_dir: PathBuf,
    /// Upstream hit cap handed to the sidecar's `topN`. The host still applies
    /// eligibility + its own limit downstream in `inject_from_search`.
    pub top_n: usize,
    /// Wall-clock bound on a `retrieve`/`status` call before the child is killed.
    pub search_timeout: Duration,
    /// Wall-clock bound on an `embed` call. Larger than `search_timeout` because
    /// a first index can download embedding models and embed the whole corpus.
    pub index_timeout: Duration,
}

impl QmdSidecar {
    /// A sidecar config with the production defaults (`node`, `topN` 8, 15s
    /// search / 5m index timeouts) for the given paths.
    pub fn new(sidecar_path: PathBuf, db_path: PathBuf, corpus_dir: PathBuf) -> Self {
        Self {
            node_bin: "node".to_string(),
            sidecar_path,
            db_path,
            corpus_dir,
            top_n: 8,
            search_timeout: Duration::from_secs(15),
            index_timeout: Duration::from_secs(300),
        }
    }
}

/// Why a sidecar invocation failed, before it is mapped to a fail-open
/// [`DegradedReason`]. Kept separate so the mapping (and the timeout kind) is the
/// caller's choice per operation.
#[derive(Debug)]
pub enum SidecarError {
    /// `node` could not be launched (missing binary, bad path).
    Spawn(std::io::Error),
    /// The child outlived its wall-clock budget and was killed.
    Timeout,
    /// The child exited non-zero (a crash inside the sidecar).
    NonZero { code: Option<i32> },
    /// Writing the request to stdin or reading the response failed.
    Io(std::io::Error),
}

impl SidecarError {
    /// Map a launch/transport failure to its fail-open [`DegradedReason`].
    /// `on_timeout` lets the caller pick the timeout flavor (`QueryTimeout` for a
    /// search, `StartupTimeout` for an index/status probe).
    fn degraded(&self, on_timeout: DegradedReason) -> DegradedReason {
        match self {
            SidecarError::Spawn(_) => DegradedReason::MissingBinary,
            SidecarError::Timeout => on_timeout,
            SidecarError::NonZero { code } => {
                DegradedReason::Other(format!("qmd sidecar exited with {code:?}"))
            }
            SidecarError::Io(e) => DegradedReason::Other(format!("qmd sidecar io error: {e}")),
        }
    }
}

/// How the backend invokes the sidecar. The real [`NodeRunner`] shells node;
/// tests inject a closure so the fail-open branches run without a node binary.
pub trait SidecarRunner {
    /// Run `<sidecar> <subcommand>` with `stdin_json` on stdin, bounded by
    /// `timeout`, returning the captured stdout on success.
    fn run(
        &self,
        subcommand: &str,
        stdin_json: &str,
        timeout: Duration,
    ) -> Result<String, SidecarError>;
}

/// Any `Fn(subcommand, stdin, timeout) -> Result<stdout, _>` is a runner, so
/// tests can pass a bare closure.
impl<F> SidecarRunner for F
where
    F: Fn(&str, &str, Duration) -> Result<String, SidecarError>,
{
    fn run(
        &self,
        subcommand: &str,
        stdin_json: &str,
        timeout: Duration,
    ) -> Result<String, SidecarError> {
        self(subcommand, stdin_json, timeout)
    }
}

/// Runs the sidecar as a real `node` child process.
#[derive(Clone, Debug)]
pub struct NodeRunner {
    node_bin: String,
    sidecar_path: PathBuf,
}

impl SidecarRunner for NodeRunner {
    fn run(
        &self,
        subcommand: &str,
        stdin_json: &str,
        timeout: Duration,
    ) -> Result<String, SidecarError> {
        let mut child = Command::new(&self.node_bin)
            .arg(&self.sidecar_path)
            .arg(subcommand)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // The sidecar logs to stderr to keep stdout a clean JSON channel; let
            // that flow to the daemon log (inherited, so it never fills a pipe we
            // would have to drain to avoid deadlock).
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(SidecarError::Spawn)?;

        // Requests are tiny JSON, so this write never blocks on the pipe buffer;
        // dropping the handle closes stdin (EOF) so the sidecar can proceed.
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(stdin_json.as_bytes())
                .map_err(SidecarError::Io)?;
        }

        // Poll for exit against a deadline; kill on overrun. Responses are small
        // (a capped topN of truncated chunks, or a one-line status), so reading
        // stdout after exit never risks a full-pipe stall.
        let deadline = Instant::now() + timeout;
        loop {
            match child.try_wait().map_err(SidecarError::Io)? {
                Some(status) => {
                    let mut out = String::new();
                    if let Some(mut stdout) = child.stdout.take() {
                        stdout
                            .read_to_string(&mut out)
                            .map_err(SidecarError::Io)?;
                    }
                    if !status.success() {
                        return Err(SidecarError::NonZero { code: status.code() });
                    }
                    return Ok(out);
                }
                None => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(SidecarError::Timeout);
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
            }
        }
    }
}

/// The node-backed [`MemoryBackend`]. Generic over the runner so the offline gate
/// drives it with a fake; the default [`NodeRunner`] shells real node.
pub struct NodeQmd<R: SidecarRunner = NodeRunner> {
    config: QmdSidecar,
    runner: R,
}

impl NodeQmd<NodeRunner> {
    /// A live backend that shells the configured `node` binary.
    pub fn new(config: QmdSidecar) -> Self {
        let runner = NodeRunner {
            node_bin: config.node_bin.clone(),
            sidecar_path: config.sidecar_path.clone(),
        };
        Self { config, runner }
    }
}

impl<R: SidecarRunner> NodeQmd<R> {
    /// A backend with an injected runner — the seam offline tests use to exercise
    /// the fail-open branches without a node binary.
    pub fn with_runner(config: QmdSidecar, runner: R) -> Self {
        Self { config, runner }
    }

    /// Write the corpus to disk, one file per entry named by the hex-encoded
    /// memory id, after clearing any stale `.md` files so removed memories stop
    /// being indexed. Hex encoding (`corpus_stem_for_memory_id`) keeps the
    /// filename a fixed point of qmd's path normalization — so the search
    /// parser recovers the id from a hit's `displayPath` — and is inherently
    /// path-safe (pure `[0-9a-f]`, no separators), so a malformed id cannot
    /// escape the corpus dir.
    fn write_corpus(&self, docs: &[IndexDoc]) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.config.corpus_dir)?;
        for entry in std::fs::read_dir(&self.config.corpus_dir)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) == Some("md") {
                let _ = std::fs::remove_file(&path);
            }
        }
        for doc in docs {
            if doc.memory_id.is_empty() {
                eprintln!("qmd reindex skipping entry with empty memory id");
                continue;
            }
            let stem = assistant_memory::corpus_stem_for_memory_id(&doc.memory_id);
            let file = self.config.corpus_dir.join(format!("{stem}.md"));
            std::fs::write(file, &doc.body)?;
        }
        Ok(())
    }
}

impl<R: SidecarRunner> MemoryBackend for NodeQmd<R> {
    fn reindex(&mut self, docs: &[IndexDoc]) -> MemoryHealth {
        if let Err(e) = self.write_corpus(docs) {
            return MemoryHealth::Degraded {
                reason: DegradedReason::Other(format!("qmd corpus write failed: {e}")),
            };
        }
        let payload = serde_json::json!({
            "dbPath": self.config.db_path,
            "corpusDir": self.config.corpus_dir,
        })
        .to_string();
        match self
            .runner
            .run("embed", &payload, self.config.index_timeout)
        {
            Ok(stdout) => parse_status_response(&stdout),
            Err(e) => MemoryHealth::Degraded {
                reason: e.degraded(DegradedReason::StartupTimeout),
            },
        }
    }

    fn search(&self, query: &str, _limit: usize) -> SearchOutcome {
        let payload = serde_json::json!({
            "message": query,
            "topN": self.config.top_n,
            "dbPath": self.config.db_path,
        })
        .to_string();
        match self
            .runner
            .run("retrieve", &payload, self.config.search_timeout)
        {
            Ok(stdout) => parse_qmd_response(&stdout),
            Err(e) => SearchOutcome::Degraded(e.degraded(DegradedReason::QueryTimeout)),
        }
    }

    fn health(&self) -> MemoryHealth {
        let payload = serde_json::json!({ "dbPath": self.config.db_path }).to_string();
        match self
            .runner
            .run("status", &payload, self.config.search_timeout)
        {
            Ok(stdout) => parse_status_response(&stdout),
            Err(e) => MemoryHealth::Degraded {
                reason: e.degraded(DegradedReason::StartupTimeout),
            },
        }
    }
}

/// Parse the sidecar's status shape into [`MemoryHealth`], fail-open throughout:
/// `{ "indexed": N }` is healthy when `N > 0` and an empty index otherwise;
/// `{ "error": "<reason>" }` degrades through the shared wire vocabulary; any
/// malformed or unexpected output degrades rather than raising.
pub fn parse_status_response(stdout: &str) -> MemoryHealth {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return MemoryHealth::Degraded {
            reason: DegradedReason::Other("empty qmd status".to_string()),
        };
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return MemoryHealth::Degraded {
            reason: DegradedReason::Other("malformed qmd status".to_string()),
        };
    };
    if let Some(reason) = value.get("error").and_then(|e| e.as_str()) {
        return MemoryHealth::Degraded {
            reason: DegradedReason::from_wire(reason.trim()),
        };
    }
    match value.get("indexed").and_then(|n| n.as_u64()) {
        Some(0) => MemoryHealth::Degraded {
            reason: DegradedReason::EmptyIndex,
        },
        Some(n) => MemoryHealth::Healthy {
            indexed: n as usize,
        },
        None => MemoryHealth::Degraded {
            reason: DegradedReason::Other("qmd status missing indexed".to_string()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn config(corpus_dir: PathBuf) -> QmdSidecar {
        QmdSidecar::new(
            PathBuf::from("/nonexistent/qmd-sidecar.mjs"),
            PathBuf::from("/nonexistent/index.sqlite"),
            corpus_dir,
        )
    }

    fn doc(id: &str, body: &str) -> IndexDoc {
        IndexDoc {
            memory_id: id.to_string(),
            rel_path: format!("notes/{id}.md"),
            body: body.to_string(),
        }
    }

    #[test]
    fn search_parses_runner_hits() {
        let backend = NodeQmd::with_runner(
            config(PathBuf::from("/tmp/unused")),
            |sub: &str, _stdin: &str, _t: Duration| {
                assert_eq!(sub, "retrieve");
                // displayPath stem is the hex-encoded memory id the host wrote.
                let stem = assistant_memory::corpus_stem_for_memory_id("mem_a");
                Ok(format!(
                    r#"{{"hits":[{{"displayPath":"notes/{stem}.md","bestChunk":"alice","score":0.9}}]}}"#
                ))
            },
        );
        let SearchOutcome::Hits(hits) = backend.search("who is alice", 5) else {
            panic!("expected hits");
        };
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].memory_id, "mem_a");
        assert_eq!(hits[0].snippet, "alice");
    }

    #[test]
    fn search_sends_expected_request_shape() {
        let backend = NodeQmd::with_runner(
            config(PathBuf::from("/tmp/unused")),
            |sub: &str, stdin: &str, _t: Duration| {
                assert_eq!(sub, "retrieve");
                let req: serde_json::Value = serde_json::from_str(stdin).unwrap();
                assert_eq!(req["message"], "find me");
                assert_eq!(req["topN"], 8);
                assert!(req["dbPath"].is_string());
                Ok(r#"{"hits":[]}"#.to_string())
            },
        );
        let SearchOutcome::Hits(hits) = backend.search("find me", 5) else {
            panic!("expected (empty) hits");
        };
        assert!(hits.is_empty());
    }

    #[test]
    fn search_missing_binary_is_degraded() {
        let backend = NodeQmd::with_runner(
            config(PathBuf::from("/tmp/unused")),
            |_s: &str, _i: &str, _t: Duration| {
                Err(SidecarError::Spawn(std::io::Error::from(
                    std::io::ErrorKind::NotFound,
                )))
            },
        );
        assert_eq!(
            backend.search("q", 5),
            SearchOutcome::Degraded(DegradedReason::MissingBinary)
        );
    }

    #[test]
    fn search_timeout_is_query_timeout() {
        let backend = NodeQmd::with_runner(
            config(PathBuf::from("/tmp/unused")),
            |_s: &str, _i: &str, _t: Duration| Err(SidecarError::Timeout),
        );
        assert_eq!(
            backend.search("q", 5),
            SearchOutcome::Degraded(DegradedReason::QueryTimeout)
        );
    }

    #[test]
    fn search_malformed_output_is_degraded_not_fatal() {
        let backend = NodeQmd::with_runner(
            config(PathBuf::from("/tmp/unused")),
            |_s: &str, _i: &str, _t: Duration| Ok("not json".to_string()),
        );
        assert!(backend.search("q", 5).is_degraded());
    }

    #[test]
    fn reindex_writes_corpus_and_reports_health() {
        let tmp = tempfile::tempdir().unwrap();
        let corpus = tmp.path().join("corpus");
        let seen = RefCell::new(Vec::<String>::new());
        let mut backend = NodeQmd::with_runner(config(corpus.clone()), |sub: &str,
                                                                         stdin: &str,
                                                                         _t: Duration| {
            seen.borrow_mut().push(format!("{sub}:{stdin}"));
            assert_eq!(sub, "embed");
            let req: serde_json::Value = serde_json::from_str(stdin).unwrap();
            assert!(req["dbPath"].is_string());
            assert!(req["corpusDir"].is_string());
            Ok(r#"{"indexed":2}"#.to_string())
        });

        let health = backend.reindex(&[doc("mem_a", "alice body"), doc("mem_b", "bob body")]);
        assert_eq!(health, MemoryHealth::Healthy { indexed: 2 });
        let a = assistant_memory::corpus_stem_for_memory_id("mem_a");
        let b = assistant_memory::corpus_stem_for_memory_id("mem_b");
        assert!(corpus.join(format!("{a}.md")).exists());
        assert!(corpus.join(format!("{b}.md")).exists());
        assert_eq!(
            std::fs::read_to_string(corpus.join(format!("{a}.md"))).unwrap(),
            "alice body"
        );
        assert_eq!(seen.borrow().len(), 1);
    }

    #[test]
    fn reindex_clears_stale_corpus_files() {
        let tmp = tempfile::tempdir().unwrap();
        let corpus = tmp.path().join("corpus");
        std::fs::create_dir_all(&corpus).unwrap();
        std::fs::write(corpus.join("old.md"), "stale").unwrap();

        let mut backend = NodeQmd::with_runner(
            config(corpus.clone()),
            |_s: &str, _i: &str, _t: Duration| Ok(r#"{"indexed":1}"#.to_string()),
        );
        backend.reindex(&[doc("fresh", "new body")]);

        assert!(!corpus.join("old.md").exists(), "stale file removed");
        let fresh = assistant_memory::corpus_stem_for_memory_id("fresh");
        assert!(corpus.join(format!("{fresh}.md")).exists());
    }

    #[test]
    fn reindex_missing_binary_is_degraded() {
        let tmp = tempfile::tempdir().unwrap();
        let mut backend = NodeQmd::with_runner(
            config(tmp.path().join("corpus")),
            |_s: &str, _i: &str, _t: Duration| {
                Err(SidecarError::Spawn(std::io::Error::from(
                    std::io::ErrorKind::NotFound,
                )))
            },
        );
        let health = backend.reindex(&[doc("mem_a", "body")]);
        assert_eq!(
            health,
            MemoryHealth::Degraded {
                reason: DegradedReason::MissingBinary
            }
        );
    }

    #[test]
    fn health_reports_indexed_count() {
        let backend = NodeQmd::with_runner(
            config(PathBuf::from("/tmp/unused")),
            |sub: &str, _i: &str, _t: Duration| {
                assert_eq!(sub, "status");
                Ok(r#"{"indexed":7}"#.to_string())
            },
        );
        assert_eq!(backend.health(), MemoryHealth::Healthy { indexed: 7 });
    }

    #[test]
    fn parse_status_response_covers_each_shape() {
        assert_eq!(
            parse_status_response(r#"{"indexed":3}"#),
            MemoryHealth::Healthy { indexed: 3 }
        );
        assert_eq!(
            parse_status_response(r#"{"indexed":0}"#),
            MemoryHealth::Degraded {
                reason: DegradedReason::EmptyIndex
            }
        );
        assert_eq!(
            parse_status_response(r#"{"error":"corrupt_fts"}"#),
            MemoryHealth::Degraded {
                reason: DegradedReason::CorruptFts
            }
        );
        // Malformed / wrong-shape / empty all degrade, never panic.
        assert!(!parse_status_response("garbage").is_healthy());
        assert!(!parse_status_response("").is_healthy());
        assert!(!parse_status_response(r#"{"unexpected":true}"#).is_healthy());
    }
}
