//! Permission readiness checks: an instance is only operable once an admin
//! exists and the role/DM tables reference real users.
//!
//! `CheckStatus` mirrors the readiness status shape used by other platform
//! crates; the small enum is duplicated rather than shared to honor the module
//! dependency boundary. Both checks are pure central-DB queries and run
//! anywhere.

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::model::PermissionError;
use crate::store;

/// The outcome of one readiness check.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CheckStatus {
    Pass,
    Fail { detail: String },
    Skipped { detail: String },
}

impl CheckStatus {
    pub fn is_pass(&self) -> bool {
        matches!(self, CheckStatus::Pass)
    }

    pub fn is_blocking_failure(&self) -> bool {
        matches!(self, CheckStatus::Fail { .. })
    }
}

/// At least one administrative user (owner or admin) exists. Without one, no
/// admin-gated action could ever be authorized.
pub fn admin_exists(conn: &Connection) -> Result<CheckStatus, PermissionError> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM user_roles WHERE role IN ('owner', 'admin')",
        [],
        |row| row.get(0),
    )?;
    if count > 0 {
        Ok(CheckStatus::Pass)
    } else {
        Ok(CheckStatus::Fail {
            detail: "no owner or admin user exists".to_string(),
        })
    }
}

/// Every role and DM row references an existing user — no orphaned grants or
/// routes left behind by a botched delete.
pub fn references_intact(conn: &Connection) -> Result<CheckStatus, PermissionError> {
    let orphan_roles: i64 = conn.query_row(
        "SELECT COUNT(*) FROM user_roles r LEFT JOIN users u ON u.id = r.user_id WHERE u.id IS NULL",
        [],
        |row| row.get(0),
    )?;
    let orphan_dms: i64 = conn.query_row(
        "SELECT COUNT(*) FROM user_dms d LEFT JOIN users u ON u.id = d.user_id WHERE u.id IS NULL",
        [],
        |row| row.get(0),
    )?;
    if orphan_roles == 0 && orphan_dms == 0 {
        Ok(CheckStatus::Pass)
    } else {
        Ok(CheckStatus::Fail {
            detail: format!(
                "{orphan_roles} orphaned role row(s) and {orphan_dms} orphaned DM row(s)"
            ),
        })
    }
}

/// Whether the actor passes an admin gate — exported as a readiness-style helper
/// for callers that want a boolean rather than the error from
/// [`crate::access::require_admin`].
pub fn is_admin(conn: &Connection, user_id: i64) -> Result<bool, PermissionError> {
    store::is_administrative(conn, user_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use assistant_db::{apply, baseline_migrations, baseline_owner_modules};

    fn db() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        let order: Vec<String> = baseline_owner_modules()
            .into_iter()
            .map(str::to_string)
            .collect();
        let set = baseline_migrations(order);
        apply(&mut conn, &set).unwrap();
        conn
    }

    #[test]
    fn admin_check_fails_until_owner_then_passes() {
        let conn = db();
        assert!(admin_exists(&conn).unwrap().is_blocking_failure());
        store::bootstrap_owner(&conn, "root", None).unwrap();
        assert!(admin_exists(&conn).unwrap().is_pass());
    }

    #[test]
    fn references_intact_detects_orphans() {
        let conn = db();
        assert!(references_intact(&conn).unwrap().is_pass());
        // A role row pointing at a non-existent user is an orphan.
        conn.execute(
            "INSERT INTO user_roles (user_id, role) VALUES (404, 'member')",
            [],
        )
        .unwrap();
        assert!(references_intact(&conn).unwrap().is_blocking_failure());
    }

    #[test]
    fn check_status_round_trips_json() {
        let s = CheckStatus::Fail { detail: "x".into() };
        let json = serde_json::to_string(&s).unwrap();
        let back: CheckStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }
}
