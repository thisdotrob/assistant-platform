//! The memory browser/editor's filesystem layer — the most safety-critical part
//! of the web UI, because it turns operator HTTP input into writes under a
//! memory root.
//!
//! Every path is confined three ways before any I/O: lexically (relative only,
//! no `..`, no absolute/prefix), then by canonicalizing the parent and proving
//! it still lives under the canonical root (so a symlinked directory cannot
//! redirect a write out of the tree), then by rejecting a target that is itself
//! a symlink or a non-regular file. On top of confinement the editor enforces an
//! extension allowlist, a byte ceiling, an exclusive create, an atomic
//! temp+rename save, optimistic-concurrency version checks, and a protected-path
//! list (qmd indexes, derived catalogs, and the standing-instruction surface are
//! never editable here). RAG eligibility is gated on the required front-matter
//! keys being present — a save without them still succeeds, but is flagged
//! ineligible so it is never auto-injected until classified.
//!
//! This mirrors the lexical confinement in `assistant-memory`'s `root.rs` and adds
//! the runtime symlink/canonical checks the editor needs because it touches the
//! disk directly.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use serde::Serialize;
use sha2::{Digest, Sha256};

/// What the editor will and won't accept. The host supplies the specifics; the
/// defaults are the conservative memory-markdown rules.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditorPolicy {
    /// Lower-case extensions that may be created/edited (no dot).
    pub allowed_extensions: Vec<String>,
    /// Largest body the editor will write.
    pub max_bytes: u64,
    /// Path components (case-insensitive) and file stems that may never be
    /// written: qmd indexes, derived metadata catalogs, the standing-instruction
    /// surface. Editing those goes through their own explicit surfaces.
    pub protected_segments: Vec<String>,
    /// Front-matter keys that must be present (with a non-empty value) for an
    /// entry to be RAG-eligible.
    pub required_metadata: Vec<String>,
}

impl Default for EditorPolicy {
    fn default() -> Self {
        Self {
            allowed_extensions: vec!["md".to_string()],
            max_bytes: 1024 * 1024,
            protected_segments: vec![
                ".qmd".to_string(),
                "qmd_index".to_string(),
                "catalog".to_string(),
                "index".to_string(),
                "standing_instructions".to_string(),
            ],
            required_metadata: vec![
                "scope".to_string(),
                "source_type".to_string(),
                "confidence".to_string(),
                "reuse_policy".to_string(),
                "retention".to_string(),
            ],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MemoryFsError {
    /// The relative path was empty, absolute, or contained `..`/root/prefix.
    PathEscape { path: String },
    /// The resolved path's real parent is outside the memory root (e.g. via a
    /// symlinked directory).
    OutsideRoot { path: String },
    /// The target path is itself a symlink.
    Symlink { path: String },
    /// The target exists but is not a regular file (dir, fifo, socket, device).
    SpecialFile { path: String },
    /// The extension is not on the allowlist.
    ExtensionNotAllowed { path: String },
    /// The body exceeds the configured byte ceiling.
    TooLarge { bytes: u64, max: u64 },
    /// The path touches a protected surface (qmd index, catalog, standing
    /// instructions).
    Protected { path: String },
    /// Exclusive create found the file already present.
    AlreadyExists { path: String },
    /// The file does not exist.
    NotFound { path: String },
    /// Optimistic concurrency: the on-disk version is not the one the editor was
    /// based on.
    VersionConflict { expected: String, actual: String },
    /// Any other I/O failure.
    Io { detail: String },
}

impl std::fmt::Display for MemoryFsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MemoryFsError::PathEscape { path } => write!(f, "path {path:?} is not a safe relative path"),
            MemoryFsError::OutsideRoot { path } => write!(f, "path {path:?} resolves outside the memory root"),
            MemoryFsError::Symlink { path } => write!(f, "path {path:?} is a symlink"),
            MemoryFsError::SpecialFile { path } => write!(f, "path {path:?} is not a regular file"),
            MemoryFsError::ExtensionNotAllowed { path } => write!(f, "path {path:?} has a disallowed extension"),
            MemoryFsError::TooLarge { bytes, max } => write!(f, "body of {bytes} bytes exceeds the {max}-byte limit"),
            MemoryFsError::Protected { path } => write!(f, "path {path:?} is a protected surface and not editable here"),
            MemoryFsError::AlreadyExists { path } => write!(f, "path {path:?} already exists"),
            MemoryFsError::NotFound { path } => write!(f, "path {path:?} does not exist"),
            MemoryFsError::VersionConflict { expected, actual } => write!(f, "version conflict: expected {expected}, on disk {actual}"),
            MemoryFsError::Io { detail } => write!(f, "io error: {detail}"),
        }
    }
}

impl std::error::Error for MemoryFsError {}

/// A directory entry in a browse listing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DirEntryInfo {
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
}

/// A file the editor read: its content, the version token to echo back on save,
/// and whether its front matter qualifies it for RAG.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MemoryFile {
    pub path: String,
    pub content: String,
    pub version: String,
    pub rag_eligible: bool,
}

/// The result of a create or save.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SaveResult {
    pub path: String,
    pub version: String,
    pub rag_eligible: bool,
    pub missing_metadata: Vec<String>,
}

/// An editor bound to one memory root under one policy.
pub struct MemoryEditor {
    root: PathBuf,
    policy: EditorPolicy,
}

impl MemoryEditor {
    pub fn new(root: impl Into<PathBuf>, policy: EditorPolicy) -> Self {
        Self {
            root: root.into(),
            policy,
        }
    }

    /// The configured per-file byte ceiling. The HTTP surface uses this to cap
    /// the request body *before* parsing, so an oversized payload is rejected
    /// without first being buffered in full.
    pub fn max_bytes(&self) -> u64 {
        self.policy.max_bytes
    }

    /// List the immediate entries under a relative directory (`""` for the
    /// root). Symlinked entries are omitted — they are never followed.
    pub fn list(&self, rel_dir: &str) -> Result<Vec<DirEntryInfo>, MemoryFsError> {
        let dir = if rel_dir.is_empty() {
            self.root.clone()
        } else {
            let full = self.resolve(rel_dir)?;
            self.confine_existing_dir(&full)?;
            full
        };
        let read = std::fs::read_dir(&dir).map_err(io_err)?;
        let mut out = Vec::new();
        for entry in read {
            let entry = entry.map_err(io_err)?;
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if meta.file_type().is_symlink() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            let rel = if rel_dir.is_empty() {
                name
            } else {
                format!("{}/{}", rel_dir.trim_end_matches('/'), name)
            };
            out.push(DirEntryInfo {
                path: rel,
                is_dir: meta.is_dir(),
                size: meta.len(),
            });
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    /// Read a memory file, returning its content + version token.
    pub fn read(&self, rel: &str) -> Result<MemoryFile, MemoryFsError> {
        let full = self.resolve(rel)?;
        self.guard_target_for_read(&full)?;
        let bytes = std::fs::read(&full).map_err(|e| not_found_or_io(e, rel))?;
        let content = String::from_utf8(bytes).map_err(|_| MemoryFsError::Io {
            detail: "file is not valid UTF-8".to_string(),
        })?;
        let version = version_of(content.as_bytes());
        let (rag_eligible, _) = self.assess_metadata(&content);
        Ok(MemoryFile {
            path: rel.to_string(),
            content,
            version,
            rag_eligible,
        })
    }

    /// Create a new memory file. Fails if it already exists (exclusive create).
    pub fn create(&self, rel: &str, content: &str) -> Result<SaveResult, MemoryFsError> {
        let full = self.resolve(rel)?;
        self.guard_target_for_write(rel, &full)?;
        self.check_size(content)?;
        // Exclusive create: O_EXCL semantics so we never clobber an existing entry.
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&full).map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                MemoryFsError::AlreadyExists {
                    path: rel.to_string(),
                }
            } else {
                io_err(e)
            }
        })?;
        file.write_all(content.as_bytes()).map_err(io_err)?;
        file.sync_all().map_err(io_err)?;
        Ok(self.save_result(rel, content))
    }

    /// Save over an existing file, atomically and only if the on-disk version
    /// still matches `expected_version` (optimistic concurrency).
    pub fn save(
        &self,
        rel: &str,
        content: &str,
        expected_version: &str,
    ) -> Result<SaveResult, MemoryFsError> {
        let full = self.resolve(rel)?;
        self.guard_target_for_write(rel, &full)?;
        self.check_size(content)?;

        let current = std::fs::read(&full).map_err(|e| not_found_or_io(e, rel))?;
        let actual = version_of(&current);
        if actual != expected_version {
            return Err(MemoryFsError::VersionConflict {
                expected: expected_version.to_string(),
                actual,
            });
        }
        self.atomic_write(&full, content.as_bytes())?;
        Ok(self.save_result(rel, content))
    }

    fn save_result(&self, rel: &str, content: &str) -> SaveResult {
        let (rag_eligible, missing) = self.assess_metadata(content);
        SaveResult {
            path: rel.to_string(),
            version: version_of(content.as_bytes()),
            rag_eligible,
            missing_metadata: missing,
        }
    }

    /// Lexical confinement: relative, non-empty, and free of `..`/root/prefix
    /// components. Returns the joined absolute path (not yet canonicalized).
    fn resolve(&self, rel: &str) -> Result<PathBuf, MemoryFsError> {
        let p = Path::new(rel);
        if rel.is_empty() || p.is_absolute() {
            return Err(MemoryFsError::PathEscape {
                path: rel.to_string(),
            });
        }
        let escapes = p.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        });
        if escapes {
            return Err(MemoryFsError::PathEscape {
                path: rel.to_string(),
            });
        }
        Ok(self.root.join(p))
    }

    /// For a write target: the path must have an allowed extension, must not be
    /// protected, its real parent must stay under the root, and the target must
    /// not be an existing symlink or special file.
    fn guard_target_for_write(&self, rel: &str, full: &Path) -> Result<(), MemoryFsError> {
        self.check_protected(rel)?;
        self.check_extension(rel, full)?;
        self.confine_parent(full, rel)?;
        self.reject_symlink_or_special(full, rel)?;
        Ok(())
    }

    fn guard_target_for_read(&self, full: &Path) -> Result<(), MemoryFsError> {
        let rel = full
            .strip_prefix(&self.root)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        self.confine_parent(full, &rel)?;
        self.reject_symlink_or_special(full, &rel)?;
        Ok(())
    }

    /// Canonicalize the target's parent and require it to remain within the
    /// canonical root. This is what defeats a symlinked intermediate directory.
    fn confine_parent(&self, full: &Path, rel: &str) -> Result<(), MemoryFsError> {
        let parent = full.parent().ok_or_else(|| MemoryFsError::PathEscape {
            path: rel.to_string(),
        })?;
        let canon_root = std::fs::canonicalize(&self.root).map_err(io_err)?;
        let canon_parent = std::fs::canonicalize(parent).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                MemoryFsError::NotFound {
                    path: rel.to_string(),
                }
            } else {
                io_err(e)
            }
        })?;
        if canon_parent.starts_with(&canon_root) {
            Ok(())
        } else {
            Err(MemoryFsError::OutsideRoot {
                path: rel.to_string(),
            })
        }
    }

    /// Reject a target that is a symlink, and (if it exists) one that is not a
    /// regular file. Uses `symlink_metadata` so the link itself — not its
    /// target — is inspected.
    fn reject_symlink_or_special(&self, full: &Path, rel: &str) -> Result<(), MemoryFsError> {
        match std::fs::symlink_metadata(full) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    Err(MemoryFsError::Symlink {
                        path: rel.to_string(),
                    })
                } else if !meta.is_file() {
                    Err(MemoryFsError::SpecialFile {
                        path: rel.to_string(),
                    })
                } else {
                    Ok(())
                }
            }
            // A not-yet-existing target is fine (create path).
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io_err(e)),
        }
    }

    fn confine_existing_dir(&self, full: &Path) -> Result<(), MemoryFsError> {
        let canon_root = std::fs::canonicalize(&self.root).map_err(io_err)?;
        let canon = std::fs::canonicalize(full).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                MemoryFsError::NotFound {
                    path: full.to_string_lossy().to_string(),
                }
            } else {
                io_err(e)
            }
        })?;
        if canon.starts_with(&canon_root) {
            Ok(())
        } else {
            Err(MemoryFsError::OutsideRoot {
                path: full.to_string_lossy().to_string(),
            })
        }
    }

    fn check_extension(&self, rel: &str, full: &Path) -> Result<(), MemoryFsError> {
        let ext = full
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase());
        match ext {
            Some(ext) if self.policy.allowed_extensions.iter().any(|a| a == &ext) => Ok(()),
            _ => Err(MemoryFsError::ExtensionNotAllowed {
                path: rel.to_string(),
            }),
        }
    }

    fn check_protected(&self, rel: &str) -> Result<(), MemoryFsError> {
        let path = Path::new(rel);
        for comp in path.components() {
            if let Component::Normal(seg) = comp {
                let seg = seg.to_string_lossy().to_ascii_lowercase();
                let stem = seg.rsplit_once('.').map(|(s, _)| s).unwrap_or(&seg);
                if self
                    .policy
                    .protected_segments
                    .iter()
                    .any(|p| p.eq_ignore_ascii_case(&seg) || p.eq_ignore_ascii_case(stem))
                {
                    return Err(MemoryFsError::Protected {
                        path: rel.to_string(),
                    });
                }
            }
        }
        Ok(())
    }

    fn check_size(&self, content: &str) -> Result<(), MemoryFsError> {
        let bytes = content.len() as u64;
        if bytes > self.policy.max_bytes {
            Err(MemoryFsError::TooLarge {
                bytes,
                max: self.policy.max_bytes,
            })
        } else {
            Ok(())
        }
    }

    /// Whether the content's front matter carries every required key, plus the
    /// list of any that are missing.
    fn assess_metadata(&self, content: &str) -> (bool, Vec<String>) {
        let present = front_matter_keys(content);
        let missing: Vec<String> = self
            .policy
            .required_metadata
            .iter()
            .filter(|k| !present.contains(*k))
            .cloned()
            .collect();
        (missing.is_empty(), missing)
    }

    /// Write atomically: a private temp sibling, fsync, then rename over the
    /// target so a reader never sees a half-written file. The temp is opened
    /// `create_new` under a unique name, so a symlink planted at the temp path
    /// is rejected rather than followed, and a stale temp from a crashed write
    /// can never wedge future saves. The temp is cleaned up on any failure.
    fn atomic_write(&self, full: &Path, body: &[u8]) -> Result<(), MemoryFsError> {
        let dir = full.parent().ok_or_else(|| MemoryFsError::Io {
            detail: "target has no parent".to_string(),
        })?;
        let name = full
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let tmp = dir.join(unique_tmp_name(&name));

        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp).map_err(io_err)?;

        let written = f
            .write_all(body)
            .and_then(|_| f.sync_all())
            .map_err(io_err);
        drop(f);
        let result = written.and_then(|_| std::fs::rename(&tmp, full).map_err(io_err));
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }
}

/// The content-hash version token used for optimistic concurrency.
fn version_of(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut s = String::with_capacity(digest.len() * 2);
    for b in digest {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Collect the top-level front-matter keys that have a non-empty value. Only a
/// presence scan — the host's memory module does the authoritative YAML parse;
/// here we just gate RAG eligibility.
fn front_matter_keys(content: &str) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    let trimmed = content.strip_prefix('\u{feff}').unwrap_or(content);
    let after_open = match trimmed
        .strip_prefix("---\n")
        .or_else(|| trimmed.strip_prefix("---\r\n"))
    {
        Some(rest) => rest,
        None => return keys,
    };
    for line in after_open.lines() {
        let line = line.trim_end_matches('\r');
        if line.trim() == "---" {
            break;
        }
        // Only consider unindented `key: value` lines (top-level keys).
        if line.starts_with(char::is_whitespace) {
            continue;
        }
        if let Some((key, value)) = line.split_once(':')
            && !value.trim().is_empty()
        {
            keys.insert(key.trim().to_string());
        }
    }
    keys
}

/// A collision-resistant temp name for atomic writes. Pid + nanos is unique
/// enough for a single-process synchronous server, and the leading dot keeps it
/// out of normal listings; `create_new` guarantees we never reuse a stale one.
fn unique_tmp_name(name: &str) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(".{name}.{}.{nanos}.tmp", std::process::id())
}

fn io_err(e: std::io::Error) -> MemoryFsError {
    MemoryFsError::Io {
        detail: e.to_string(),
    }
}

fn not_found_or_io(e: std::io::Error, rel: &str) -> MemoryFsError {
    if e.kind() == std::io::ErrorKind::NotFound {
        MemoryFsError::NotFound {
            path: rel.to_string(),
        }
    } else {
        io_err(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD_FM: &str = "---\nmemory_id: mem_1\nowner_agent_group_id: ag_x\nscope: all_chats\nsource_type: user_said\nconfidence: high\nreuse_policy: same_scope\nretention: normal\n---\nbody\n";

    fn editor() -> (tempfile::TempDir, MemoryEditor) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("memory");
        std::fs::create_dir_all(&root).unwrap();
        let ed = MemoryEditor::new(root, EditorPolicy::default());
        (dir, ed)
    }

    #[test]
    fn traversal_absolute_and_empty_paths_are_rejected() {
        let (_d, ed) = editor();
        for bad in ["../escape.md", "a/../../escape.md", "/etc/passwd.md", ""] {
            assert!(
                matches!(ed.create(bad, GOOD_FM), Err(MemoryFsError::PathEscape { .. })),
                "{bad} not rejected"
            );
        }
    }

    #[test]
    fn disallowed_extension_is_rejected() {
        let (_d, ed) = editor();
        assert!(matches!(
            ed.create("notes.txt", GOOD_FM),
            Err(MemoryFsError::ExtensionNotAllowed { .. })
        ));
        // No extension at all is also rejected.
        assert!(matches!(
            ed.create("notes", GOOD_FM),
            Err(MemoryFsError::ExtensionNotAllowed { .. })
        ));
    }

    #[test]
    fn oversized_body_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("memory");
        std::fs::create_dir_all(&root).unwrap();
        let ed = MemoryEditor::new(
            root,
            EditorPolicy {
                max_bytes: 16,
                ..EditorPolicy::default()
            },
        );
        assert!(matches!(
            ed.create("big.md", "0123456789abcdefg"),
            Err(MemoryFsError::TooLarge { .. })
        ));
    }

    #[test]
    fn exclusive_create_then_conflict_on_second_create() {
        let (_d, ed) = editor();
        std::fs::create_dir(ed.root.join("people")).unwrap();
        let res = ed.create("people/alice.md", GOOD_FM).unwrap();
        assert!(res.rag_eligible);
        assert!(matches!(
            ed.create("people/alice.md", GOOD_FM),
            Err(MemoryFsError::AlreadyExists { .. })
        ));
    }

    #[test]
    fn create_requires_an_existing_parent() {
        let (_d, ed) = editor();
        // The intermediate dir does not exist, so the canonical-parent check
        // reports NotFound rather than silently creating it.
        assert!(matches!(
            ed.create("nope/deep/file.md", GOOD_FM),
            Err(MemoryFsError::NotFound { .. })
        ));
    }

    #[test]
    fn read_round_trips_and_reports_version() {
        let (_d, ed) = editor();
        ed.create("n.md", GOOD_FM).unwrap();
        let file = ed.read("n.md").unwrap();
        assert_eq!(file.content, GOOD_FM);
        assert_eq!(file.version, version_of(GOOD_FM.as_bytes()));
        assert!(file.rag_eligible);
    }

    #[test]
    fn save_enforces_optimistic_concurrency() {
        let (_d, ed) = editor();
        ed.create("n.md", GOOD_FM).unwrap();
        let v = ed.read("n.md").unwrap().version;

        // A stale version is rejected.
        assert!(matches!(
            ed.save("n.md", GOOD_FM, "deadbeef"),
            Err(MemoryFsError::VersionConflict { .. })
        ));
        // The current version saves and yields a new version.
        let updated = format!("{GOOD_FM}\nmore\n");
        let res = ed.save("n.md", &updated, &v).unwrap();
        assert_ne!(res.version, v);
        assert_eq!(ed.read("n.md").unwrap().content, updated);
    }

    #[test]
    fn protected_surfaces_cannot_be_edited_here() {
        let (_d, ed) = editor();
        // qmd index dir, derived catalog, and the standing-instruction surface.
        for bad in [
            ".qmd/segment.md",
            "catalog.md",
            "index.md",
            "standing_instructions.md",
        ] {
            assert!(
                matches!(ed.create(bad, GOOD_FM), Err(MemoryFsError::Protected { .. })),
                "{bad} should be protected"
            );
        }
    }

    #[test]
    fn missing_required_metadata_saves_but_is_not_rag_eligible() {
        let (_d, ed) = editor();
        let no_fm = "just a plain note with no front matter\n";
        let res = ed.create("draft.md", no_fm).unwrap();
        assert!(!res.rag_eligible);
        assert!(res.missing_metadata.contains(&"scope".to_string()));
        // The file is still written (operators can draft).
        assert!(ed.read("draft.md").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_target_is_rejected() {
        let (_d, ed) = editor();
        ed.create("real.md", GOOD_FM).unwrap();
        // Create memory/link.md -> memory/real.md
        let root = ed.root.clone();
        std::os::unix::fs::symlink(root.join("real.md"), root.join("link.md")).unwrap();
        assert!(matches!(
            ed.read("link.md"),
            Err(MemoryFsError::Symlink { .. })
        ));
        assert!(matches!(
            ed.save("link.md", GOOD_FM, "x"),
            Err(MemoryFsError::Symlink { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_parent_directory_cannot_redirect_writes_out_of_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("memory");
        std::fs::create_dir_all(&root).unwrap();
        // An outside directory and a symlink to it placed inside the root.
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();

        let ed = MemoryEditor::new(root, EditorPolicy::default());
        // Writing "through" the symlinked dir must be caught by canonical
        // parent containment.
        assert!(matches!(
            ed.create("escape/pwned.md", GOOD_FM),
            Err(MemoryFsError::OutsideRoot { .. })
        ));
    }

    #[test]
    fn a_directory_target_is_a_special_file_not_a_regular_one() {
        let (_d, ed) = editor();
        std::fs::create_dir(ed.root.join("adir.md")).unwrap();
        assert!(matches!(
            ed.read("adir.md"),
            Err(MemoryFsError::SpecialFile { .. })
        ));
    }

    #[test]
    fn listing_omits_symlinks_and_sorts() {
        let (_d, ed) = editor();
        ed.create("b.md", GOOD_FM).unwrap();
        ed.create("a.md", GOOD_FM).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(ed.root.join("a.md"), ed.root.join("z_link.md")).unwrap();
        let entries = ed.list("").unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(names, vec!["a.md", "b.md"]);
    }
}
