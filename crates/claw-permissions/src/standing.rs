//! Durable standing-instruction capture.
//!
//! A standing instruction is a directive an authorized user wants applied going
//! forward. Not every instruction-shaped message should become one: some are
//! merely facts worth remembering when useful, and some express a hard invariant
//! that belongs in code or tooling rather than in prose memory. [`classify`]
//! sorts an instruction into those three buckets, and [`capture`] acts on the
//! classification — persisting only genuine standing instructions, and returning
//! a typed routing outcome the host uses for the other two (a memory-update
//! signal, or an invariant report). Memory writes and code changes are not this
//! crate's to make; permissions only owns the durable standing-instruction
//! surface and the authorization gate around it.
//!
//! Only administrative users may write standing instructions. Unknown/untrusted
//! senders have no user identity and thus can never reach this surface — the
//! same trust boundary the memory observer enforces from the other side.

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use claw_db::Migration;

use crate::access::require_admin;
use crate::model::PermissionError;

const PERMISSIONS_V2: &str = "
CREATE TABLE standing_instructions (
    id         INTEGER PRIMARY KEY,
    user_id    INTEGER NOT NULL,
    scope      TEXT NOT NULL,
    body       TEXT NOT NULL,
    active     INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX idx_standing_instructions_scope_active
    ON standing_instructions (scope, active);
";

/// claw-permissions migrations beyond the baseline (v1 owns users/roles/dms).
pub fn migrations() -> Vec<Migration> {
    vec![Migration::new(
        crate::MODULE_ID,
        2,
        "standing_instructions",
        PERMISSIONS_V2,
    )]
}

/// How an instruction-shaped message should be routed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstructionClass {
    /// A durable directive to apply going forward — stored on this surface.
    PersistentInstruction,
    /// A fact worth remembering only when contextually useful — left to memory.
    MemoryUpdate,
    /// A hard rule about code or tooling — reported for encoding in code/tests,
    /// not stored as prose.
    CodeToolInvariant,
}

/// A stored standing instruction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StandingInstruction {
    pub id: i64,
    pub user_id: i64,
    pub scope: String,
    pub body: String,
    pub active: bool,
}

/// The result of capturing an instruction: the bucket it fell into plus the
/// side output the host should act on for non-persistent buckets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureOutcome {
    pub class: InstructionClass,
    /// Set when a persistent instruction row was stored.
    pub stored_id: Option<i64>,
    /// Set for `MemoryUpdate`: the body the host may persist to memory if it
    /// judges it contextually useful.
    pub memory_update: Option<String>,
    /// Set for `CodeToolInvariant`: a human-readable report to surface so the
    /// invariant is encoded in code/tooling rather than only remembered.
    pub invariant_report: Option<String>,
}

const HARD_CONSTRAINT_TERMS: &[&str] =
    &["never", "always", "must", "don't", "do not", "ensure", "no "];

const CODE_TOOL_TERMS: &[&str] = &[
    "code",
    "commit",
    "deploy",
    "test",
    "build",
    "lint",
    " ci",
    "tool",
    "command",
    "migration",
    "branch",
    "merge",
    "production",
    "secret",
    "pipeline",
];

/// Classify an instruction. This is a transparent heuristic the orchestrator may
/// override by calling [`capture_as`] with an explicit class; it errs toward the
/// least-privileged bucket (a plain fact becomes a memory update, not a standing
/// instruction).
pub fn classify(text: &str) -> InstructionClass {
    let lower = text.to_lowercase();
    let hard = HARD_CONSTRAINT_TERMS.iter().any(|t| lower.contains(t));
    let code_tool = CODE_TOOL_TERMS.iter().any(|t| lower.contains(t));
    if hard && code_tool {
        InstructionClass::CodeToolInvariant
    } else if hard
        || lower.starts_with("from now on")
        || lower.contains("whenever")
        || lower.starts_with("please always")
    {
        InstructionClass::PersistentInstruction
    } else {
        InstructionClass::MemoryUpdate
    }
}

/// Capture an instruction, classifying it automatically. Admin-gated.
pub fn capture(
    conn: &Connection,
    actor: i64,
    scope: &str,
    body: &str,
) -> Result<CaptureOutcome, PermissionError> {
    capture_as(conn, actor, scope, body, classify(body))
}

/// Capture an instruction with an explicit class (used when the orchestrator has
/// already decided). Admin-gated: a non-administrative actor is rejected before
/// anything is written.
pub fn capture_as(
    conn: &Connection,
    actor: i64,
    scope: &str,
    body: &str,
    class: InstructionClass,
) -> Result<CaptureOutcome, PermissionError> {
    require_admin(conn, actor)?;
    match class {
        InstructionClass::PersistentInstruction => {
            conn.execute(
                "INSERT INTO standing_instructions (user_id, scope, body) VALUES (?1, ?2, ?3)",
                rusqlite::params![actor, scope, body],
            )?;
            Ok(CaptureOutcome {
                class,
                stored_id: Some(conn.last_insert_rowid()),
                memory_update: None,
                invariant_report: None,
            })
        }
        InstructionClass::MemoryUpdate => Ok(CaptureOutcome {
            class,
            stored_id: None,
            memory_update: Some(body.to_string()),
            invariant_report: None,
        }),
        InstructionClass::CodeToolInvariant => Ok(CaptureOutcome {
            class,
            stored_id: None,
            memory_update: None,
            invariant_report: Some(format!(
                "instruction implies a code/tool invariant; encode and verify it rather than \
                 relying on memory: {body}"
            )),
        }),
    }
}

/// The active standing instructions, optionally filtered to one scope, newest
/// first.
pub fn active_standing_instructions(
    conn: &Connection,
    scope: Option<&str>,
) -> Result<Vec<StandingInstruction>, PermissionError> {
    let mut out = Vec::new();
    let mut push = |row: &rusqlite::Row| -> rusqlite::Result<()> {
        out.push(StandingInstruction {
            id: row.get(0)?,
            user_id: row.get(1)?,
            scope: row.get(2)?,
            body: row.get(3)?,
            active: row.get::<_, i64>(4)? != 0,
        });
        Ok(())
    };
    match scope {
        Some(scope) => {
            let mut stmt = conn.prepare(
                "SELECT id, user_id, scope, body, active FROM standing_instructions \
                 WHERE active = 1 AND scope = ?1 ORDER BY id DESC",
            )?;
            let mut rows = stmt.query(rusqlite::params![scope])?;
            while let Some(row) = rows.next()? {
                push(row)?;
            }
        }
        None => {
            let mut stmt = conn.prepare(
                "SELECT id, user_id, scope, body, active FROM standing_instructions \
                 WHERE active = 1 ORDER BY id DESC",
            )?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                push(row)?;
            }
        }
    }
    Ok(out)
}

/// Retire a standing instruction. Admin-gated.
pub fn deactivate(conn: &Connection, actor: i64, id: i64) -> Result<(), PermissionError> {
    require_admin(conn, actor)?;
    conn.execute(
        "UPDATE standing_instructions SET active = 0 WHERE id = ?1",
        rusqlite::params![id],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{bootstrap_owner, create_user, grant_role};
    use crate::Role;
    use claw_db::{apply, baseline_migrations, baseline_owner_modules};

    fn db() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        let order: Vec<String> = baseline_owner_modules()
            .into_iter()
            .map(str::to_string)
            .collect();
        let mut set = baseline_migrations(order);
        for m in migrations() {
            set.add(m);
        }
        apply(&mut conn, &set).unwrap();
        conn
    }

    #[test]
    fn classifier_sorts_each_bucket() {
        assert_eq!(
            classify("Never deploy to production on Fridays"),
            InstructionClass::CodeToolInvariant
        );
        assert_eq!(
            classify("From now on, always greet me in French"),
            InstructionClass::PersistentInstruction
        );
        assert_eq!(classify("My dog's name is Rex"), InstructionClass::MemoryUpdate);
    }

    #[test]
    fn persistent_instruction_is_stored_and_retrievable() {
        let conn = db();
        let owner = bootstrap_owner(&conn, "owner", None).unwrap();
        let outcome = capture(&conn, owner, "global", "From now on, always greet me in French").unwrap();
        assert_eq!(outcome.class, InstructionClass::PersistentInstruction);
        assert!(outcome.stored_id.is_some());

        let active = active_standing_instructions(&conn, Some("global")).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].user_id, owner);

        deactivate(&conn, owner, active[0].id).unwrap();
        assert!(active_standing_instructions(&conn, Some("global")).unwrap().is_empty());
    }

    #[test]
    fn memory_update_and_invariant_route_without_storing() {
        let conn = db();
        let owner = bootstrap_owner(&conn, "owner", None).unwrap();

        let mem = capture(&conn, owner, "global", "My dog's name is Rex").unwrap();
        assert_eq!(mem.class, InstructionClass::MemoryUpdate);
        assert_eq!(mem.memory_update.as_deref(), Some("My dog's name is Rex"));
        assert!(mem.stored_id.is_none());

        let inv = capture(&conn, owner, "global", "Never commit secrets to the repo").unwrap();
        assert_eq!(inv.class, InstructionClass::CodeToolInvariant);
        assert!(inv.invariant_report.is_some());
        assert!(inv.stored_id.is_none());

        // Neither stored a row.
        assert!(active_standing_instructions(&conn, None).unwrap().is_empty());
    }

    #[test]
    fn non_admin_cannot_write_a_standing_instruction() {
        let conn = db();
        let owner = bootstrap_owner(&conn, "owner", None).unwrap();
        let member = create_user(&conn, "member", None).unwrap();
        grant_role(&conn, owner, member, Role::Member).unwrap();

        let result = capture(&conn, member, "global", "From now on, always do X");
        assert!(matches!(result, Err(PermissionError::NotAuthorized { .. })));
        assert!(active_standing_instructions(&conn, None).unwrap().is_empty());
    }
}
