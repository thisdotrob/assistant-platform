//! Attachment path safety.
//!
//! Attachment file names arrive from channels and the container; they must
//! never escape the session-scoped inbox/outbox directory. We reject absolute
//! paths and any component that is `..`, then confirm the lexically-joined path
//! stays under the base.

use std::path::{Component, Path, PathBuf};

use crate::error::SessionError;

/// Resolve `candidate` (a relative file name, possibly with subdirectories)
/// against `base`, rejecting anything that could escape `base`.
pub fn safe_attachment_path(base: &Path, candidate: &str) -> Result<PathBuf, SessionError> {
    let rel = Path::new(candidate);
    let escapes = candidate.is_empty()
        || rel.is_absolute()
        || rel.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        });
    if escapes {
        return Err(SessionError::AttachmentEscape {
            path: rel.to_path_buf(),
        });
    }
    let joined = base.join(rel);
    if joined.starts_with(base) {
        Ok(joined)
    } else {
        Err(SessionError::AttachmentEscape { path: joined })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_name_is_allowed() {
        let base = Path::new("/data/sessions/g/s/inbox/m1");
        let p = safe_attachment_path(base, "photo.png").unwrap();
        assert_eq!(p, base.join("photo.png"));
    }

    #[test]
    fn nested_relative_is_allowed() {
        let base = Path::new("/data/sessions/g/s/inbox/m1");
        let p = safe_attachment_path(base, "sub/dir/file.txt").unwrap();
        assert!(p.starts_with(base));
    }

    #[test]
    fn traversal_and_absolute_are_rejected() {
        let base = Path::new("/data/sessions/g/s/inbox/m1");
        assert!(matches!(
            safe_attachment_path(base, "../escape"),
            Err(SessionError::AttachmentEscape { .. })
        ));
        assert!(matches!(
            safe_attachment_path(base, "a/../../escape"),
            Err(SessionError::AttachmentEscape { .. })
        ));
        assert!(matches!(
            safe_attachment_path(base, "/etc/passwd"),
            Err(SessionError::AttachmentEscape { .. })
        ));
        assert!(matches!(
            safe_attachment_path(base, ""),
            Err(SessionError::AttachmentEscape { .. })
        ));
    }
}
