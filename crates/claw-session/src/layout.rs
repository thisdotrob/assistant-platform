//! Per-session folder layout under the sessions root.
//!
//! Layout mirrors the architecture's session folder:
//! `<sessions>/<agent_group_id>/<session_id>/{inbound.db,outbound.db,.heartbeat,inbox/,outbox/}`.
//! Identifiers are validated so they can never introduce path traversal.

use std::path::{Path, PathBuf};

use crate::error::SessionError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DbKind {
    Inbound,
    Outbound,
}

impl DbKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DbKind::Inbound => "inbound",
            DbKind::Outbound => "outbound",
        }
    }

    pub fn file_name(self) -> &'static str {
        match self {
            DbKind::Inbound => "inbound.db",
            DbKind::Outbound => "outbound.db",
        }
    }
}

impl std::fmt::Display for DbKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An identifier is safe if it is non-empty and contains only ascii
/// alphanumerics, '-' or '_'. This forbids '/', '.', and '..' so a session
/// directory can never climb out of the sessions root.
fn validate_id(id: &str) -> Result<(), SessionError> {
    let ok = !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err(SessionError::InvalidId(id.to_string()))
    }
}

/// The fully derived on-disk layout for one session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionLayout {
    dir: PathBuf,
}

impl SessionLayout {
    pub fn derive(
        sessions_root: &Path,
        agent_group_id: &str,
        session_id: &str,
    ) -> Result<Self, SessionError> {
        validate_id(agent_group_id)?;
        validate_id(session_id)?;
        Ok(Self {
            dir: sessions_root.join(agent_group_id).join(session_id),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn db_path(&self, kind: DbKind) -> PathBuf {
        self.dir.join(kind.file_name())
    }

    pub fn inbound_db_path(&self) -> PathBuf {
        self.db_path(DbKind::Inbound)
    }

    pub fn outbound_db_path(&self) -> PathBuf {
        self.db_path(DbKind::Outbound)
    }

    pub fn heartbeat_path(&self) -> PathBuf {
        self.dir.join(".heartbeat")
    }

    pub fn lock_path(&self) -> PathBuf {
        self.dir.join(".recovery.lock")
    }

    pub fn inbox_dir(&self) -> PathBuf {
        self.dir.join("inbox")
    }

    pub fn outbox_dir(&self) -> PathBuf {
        self.dir.join("outbox")
    }

    /// Attachment storage for one inbound message. Callers must run candidate
    /// file names through [`crate::attachment::safe_attachment_path`].
    pub fn inbox_message_dir(&self, message_id: &str) -> Result<PathBuf, SessionError> {
        validate_id(message_id)?;
        Ok(self.inbox_dir().join(message_id))
    }

    pub fn outbox_message_dir(&self, message_id: &str) -> Result<PathBuf, SessionError> {
        validate_id(message_id)?;
        Ok(self.outbox_dir().join(message_id))
    }

    /// Directories created when a session folder is initialized.
    pub fn managed_dirs(&self) -> Vec<PathBuf> {
        vec![self.dir.clone(), self.inbox_dir(), self.outbox_dir()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_is_under_sessions_root() {
        let root = Path::new("/data/sessions");
        let layout = SessionLayout::derive(root, "orchestrator", "sess-1").unwrap();
        assert_eq!(layout.dir(), Path::new("/data/sessions/orchestrator/sess-1"));
        assert!(layout.inbound_db_path().starts_with(layout.dir()));
        assert!(layout.outbound_db_path().starts_with(layout.dir()));
        assert!(layout.managed_dirs().iter().all(|d| d.starts_with(root)));
    }

    #[test]
    fn traversal_ids_are_rejected() {
        let root = Path::new("/data/sessions");
        assert!(matches!(
            SessionLayout::derive(root, "..", "s"),
            Err(SessionError::InvalidId(_))
        ));
        assert!(matches!(
            SessionLayout::derive(root, "ok", "../escape"),
            Err(SessionError::InvalidId(_))
        ));
        assert!(matches!(
            SessionLayout::derive(root, "a/b", "s"),
            Err(SessionError::InvalidId(_))
        ));
    }
}
