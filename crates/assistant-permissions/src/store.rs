//! Persistence over the baseline `users`, `user_roles`, and `user_dms` tables.
//!
//! Role changes that must be all-or-nothing run inside a transaction so an
//! interrupted grant never leaves a half-written membership. The admin gate is
//! enforced here at the write boundary, not left to callers.

use rusqlite::{Connection, OptionalExtension};

use crate::model::{PermissionError, Role, User};

/// Insert a user and return its id. Handles are unique (baseline constraint).
pub fn create_user(
    conn: &Connection,
    handle: &str,
    display_name: Option<&str>,
) -> Result<i64, PermissionError> {
    conn.execute(
        "INSERT INTO users (handle, display_name) VALUES (?1, ?2)",
        rusqlite::params![handle, display_name],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn get_user(conn: &Connection, user_id: i64) -> Result<Option<User>, PermissionError> {
    Ok(conn
        .query_row(
            "SELECT id, handle, display_name FROM users WHERE id = ?1",
            rusqlite::params![user_id],
            |row| {
                Ok(User {
                    id: row.get(0)?,
                    handle: row.get(1)?,
                    display_name: row.get(2)?,
                })
            },
        )
        .optional()?)
}

pub fn find_user_by_handle(
    conn: &Connection,
    handle: &str,
) -> Result<Option<User>, PermissionError> {
    Ok(conn
        .query_row(
            "SELECT id, handle, display_name FROM users WHERE handle = ?1",
            rusqlite::params![handle],
            |row| {
                Ok(User {
                    id: row.get(0)?,
                    handle: row.get(1)?,
                    display_name: row.get(2)?,
                })
            },
        )
        .optional()?)
}

/// The roles held by a user, highest privilege first.
pub fn roles_of(conn: &Connection, user_id: i64) -> Result<Vec<Role>, PermissionError> {
    let mut stmt = conn.prepare("SELECT role FROM user_roles WHERE user_id = ?1")?;
    let mut rows = stmt.query(rusqlite::params![user_id])?;
    let mut roles = Vec::new();
    while let Some(row) = rows.next()? {
        let raw: String = row.get(0)?;
        if let Some(role) = Role::parse(&raw) {
            roles.push(role);
        }
    }
    roles.sort_unstable_by(|a, b| b.cmp(a));
    Ok(roles)
}

pub fn has_role(conn: &Connection, user_id: i64, role: Role) -> Result<bool, PermissionError> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM user_roles WHERE user_id = ?1 AND role = ?2",
        rusqlite::params![user_id, role.as_str()],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// Whether a user may pass an administrative gate (owner or admin).
pub fn is_administrative(conn: &Connection, user_id: i64) -> Result<bool, PermissionError> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM user_roles WHERE user_id = ?1 AND role IN ('owner', 'admin')",
        rusqlite::params![user_id],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// Whether any owner exists yet — the readiness and bootstrap precondition.
pub fn owner_exists(conn: &Connection) -> Result<bool, PermissionError> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM user_roles WHERE role = 'owner'",
        [],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// Create the instance owner. Fails if an owner already exists. The user row
/// and the owner role are written together so a partial owner is impossible.
pub fn bootstrap_owner(
    conn: &Connection,
    handle: &str,
    display_name: Option<&str>,
) -> Result<i64, PermissionError> {
    let tx = conn.unchecked_transaction()?;
    if owner_exists(&tx)? {
        return Err(PermissionError::OwnerAlreadyExists);
    }
    tx.execute(
        "INSERT INTO users (handle, display_name) VALUES (?1, ?2)",
        rusqlite::params![handle, display_name],
    )?;
    let id = tx.last_insert_rowid();
    tx.execute(
        "INSERT INTO user_roles (user_id, role) VALUES (?1, 'owner')",
        rusqlite::params![id],
    )?;
    tx.commit()?;
    Ok(id)
}

/// Grant a role to a user. Admin-gated: the actor must be owner/admin. Atomic:
/// the gate check and the write share a transaction, so a denied grant writes
/// nothing.
pub fn grant_role(
    conn: &Connection,
    actor: i64,
    target: i64,
    role: Role,
) -> Result<(), PermissionError> {
    let tx = conn.unchecked_transaction()?;
    if !is_administrative(&tx, actor)? {
        return Err(PermissionError::NotAuthorized { actor });
    }
    // The owner role is established only via bootstrap_owner and is permanent.
    // Without this guard an admin could mint a second owner (escalating itself
    // to owner) and bypass the single-owner invariant, so no one may grant it.
    if role == Role::Owner {
        return Err(PermissionError::NotAuthorized { actor });
    }
    if get_user(&tx, target)?.is_none() {
        return Err(PermissionError::UnknownUser { user_id: target });
    }
    tx.execute(
        "INSERT OR IGNORE INTO user_roles (user_id, role) VALUES (?1, ?2)",
        rusqlite::params![target, role.as_str()],
    )?;
    tx.commit()?;
    Ok(())
}

/// Revoke a role. Admin-gated and atomic, like [`grant_role`].
pub fn revoke_role(
    conn: &Connection,
    actor: i64,
    target: i64,
    role: Role,
) -> Result<(), PermissionError> {
    let tx = conn.unchecked_transaction()?;
    if !is_administrative(&tx, actor)? {
        return Err(PermissionError::NotAuthorized { actor });
    }
    // The owner role is permanent; refusing to revoke it stops an admin from
    // demoting the instance owner.
    if role == Role::Owner {
        return Err(PermissionError::NotAuthorized { actor });
    }
    tx.execute(
        "DELETE FROM user_roles WHERE user_id = ?1 AND role = ?2",
        rusqlite::params![target, role.as_str()],
    )?;
    tx.commit()?;
    Ok(())
}

/// Register a DM route for a user on a channel. One address per (user, channel);
/// re-registering replaces it.
pub fn add_user_dm(
    conn: &Connection,
    user_id: i64,
    channel: &str,
    address: &str,
) -> Result<(), PermissionError> {
    if get_user(conn, user_id)?.is_none() {
        return Err(PermissionError::UnknownUser { user_id });
    }
    conn.execute(
        "INSERT INTO user_dms (user_id, channel, address) VALUES (?1, ?2, ?3)
         ON CONFLICT(user_id, channel) DO UPDATE SET address = excluded.address",
        rusqlite::params![user_id, channel, address],
    )?;
    Ok(())
}

/// Resolve a (channel, address) to a known user, if registered.
pub fn resolve_user_by_dm(
    conn: &Connection,
    channel: &str,
    address: &str,
) -> Result<Option<i64>, PermissionError> {
    Ok(conn
        .query_row(
            "SELECT user_id FROM user_dms WHERE channel = ?1 AND address = ?2",
            rusqlite::params![channel, address],
            |row| row.get::<_, i64>(0),
        )
        .optional()?)
}
