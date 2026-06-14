//! The generic approval primitive: request, grant, deny, expiry sweep, and the
//! free-form pending-question/response matching.
//!
//! Two invariants are enforced here at the write boundary, not left to callers:
//! only an authorized approver (an administrative user, per claw-permissions)
//! may decide an approval, and an expired approval is never honored — `grant`
//! checks expiry against the caller's `now` even if the sweep has not yet run.

use rusqlite::{Connection, OptionalExtension};

use claw_permissions::require_admin;

use crate::model::{
    Approval, ApprovalError, ApprovalKind, ApprovalStatus, EpochSecs, Question, QuestionStatus,
};

fn read_approval(row: &rusqlite::Row) -> Result<Approval, ApprovalError> {
    let kind_raw: String = row.get(1)?;
    let status_raw: String = row.get(3)?;
    let expires_raw: Option<String> = row.get(5)?;
    Ok(Approval {
        id: row.get(0)?,
        kind: ApprovalKind::parse(&kind_raw).ok_or(ApprovalError::UnknownEnum(kind_raw))?,
        subject: row.get(2)?,
        status: ApprovalStatus::parse(&status_raw)
            .ok_or(ApprovalError::UnknownEnum(status_raw))?,
        requested_by: row.get(4)?,
        // A malformed expiry must NOT silently become "never expires" — that
        // would let grant() skip the expiry gate and honor a stale approval.
        expires_at: match expires_raw {
            Some(s) => Some(
                s.parse::<EpochSecs>()
                    .map_err(|_| ApprovalError::UnknownEnum(s))?,
            ),
            None => None,
        },
    })
}

/// Open an approval request. Returns the new approval id. `expires_at` is an
/// absolute epoch second; `None` means it never expires.
pub fn request_approval(
    conn: &Connection,
    kind: ApprovalKind,
    subject: &str,
    requested_by: Option<i64>,
    expires_at: Option<EpochSecs>,
) -> Result<i64, ApprovalError> {
    conn.execute(
        "INSERT INTO pending_approvals (kind, subject, status, requested_by, expires_at)
         VALUES (?1, ?2, 'pending', ?3, ?4)",
        rusqlite::params![
            kind.as_str(),
            subject,
            requested_by,
            expires_at.map(|e| e.to_string()),
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn get_approval(conn: &Connection, id: i64) -> Result<Option<Approval>, ApprovalError> {
    conn.query_row(
        "SELECT id, kind, subject, status, requested_by, expires_at FROM pending_approvals WHERE id = ?1",
        rusqlite::params![id],
        |row| Ok(read_approval(row)),
    )
    .optional()?
    .transpose()
}

/// Grant an approval. Authorized-approver gated and atomic. Refuses if the
/// approval is missing, not pending, or expired as of `now` — an expired
/// approval is marked expired and never honored.
pub fn grant(
    conn: &Connection,
    approver: i64,
    id: i64,
    now: EpochSecs,
) -> Result<(), ApprovalError> {
    decide(conn, approver, id, now, ApprovalStatus::Granted)
}

/// Deny an approval. Authorized-approver gated and atomic.
pub fn deny(
    conn: &Connection,
    approver: i64,
    id: i64,
    now: EpochSecs,
) -> Result<(), ApprovalError> {
    decide(conn, approver, id, now, ApprovalStatus::Denied)
}

fn decide(
    conn: &Connection,
    approver: i64,
    id: i64,
    now: EpochSecs,
    outcome: ApprovalStatus,
) -> Result<(), ApprovalError> {
    let tx = conn.unchecked_transaction()?;
    require_admin(&tx, approver)?;
    let approval = get_approval(&tx, id)?.ok_or(ApprovalError::NotFound { id })?;
    if approval.status != ApprovalStatus::Pending {
        return Err(ApprovalError::NotDecidable { id });
    }
    if let Some(exp) = approval.expires_at
        && exp <= now
    {
        tx.execute(
            "UPDATE pending_approvals SET status = 'expired' WHERE id = ?1 AND status = 'pending'",
            rusqlite::params![id],
        )?;
        tx.commit()?;
        return Err(ApprovalError::Expired { id });
    }
    tx.execute(
        "UPDATE pending_approvals SET status = ?2 WHERE id = ?1 AND status = 'pending'",
        rusqlite::params![id, outcome.as_str()],
    )?;
    tx.commit()?;
    Ok(())
}

/// Mark every pending approval whose expiry is at or before `now` as expired.
/// Returns how many were swept.
pub fn sweep_expired(conn: &Connection, now: EpochSecs) -> Result<usize, ApprovalError> {
    let swept = conn.execute(
        "UPDATE pending_approvals
             SET status = 'expired'
         WHERE status = 'pending'
           AND expires_at IS NOT NULL
           AND CAST(expires_at AS INTEGER) <= ?1",
        rusqlite::params![now],
    )?;
    Ok(swept)
}

/// How many pending approvals are past their expiry (the readiness signal that
/// the sweep is keeping up).
pub fn pending_past_expiry(conn: &Connection, now: EpochSecs) -> Result<i64, ApprovalError> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM pending_approvals
         WHERE status = 'pending' AND expires_at IS NOT NULL AND CAST(expires_at AS INTEGER) <= ?1",
        rusqlite::params![now],
        |row| row.get(0),
    )?)
}

// ---- Free-form pending questions -------------------------------------------

/// Ask a question, returning its id. A response is matched back to this id.
pub fn ask(
    conn: &Connection,
    session_id: Option<&str>,
    prompt: &str,
) -> Result<i64, ApprovalError> {
    conn.execute(
        "INSERT INTO pending_questions (session_id, prompt, status) VALUES (?1, ?2, 'open')",
        rusqlite::params![session_id, prompt],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn get_question(conn: &Connection, id: i64) -> Result<Option<Question>, ApprovalError> {
    conn.query_row(
        "SELECT id, session_id, prompt, status FROM pending_questions WHERE id = ?1",
        rusqlite::params![id],
        |row| {
            let status_raw: String = row.get(3)?;
            Ok((
                Question {
                    id: row.get(0)?,
                    session_id: row.get(1)?,
                    prompt: row.get(2)?,
                    status: QuestionStatus::Open,
                },
                status_raw,
            ))
        },
    )
    .optional()?
    .map(|(mut q, status_raw)| {
        // An unknown stored status must error, not default to Open — an Open
        // default would make an already-closed question answerable again.
        q.status =
            QuestionStatus::parse(&status_raw).ok_or(ApprovalError::UnknownEnum(status_raw))?;
        Ok(q)
    })
    .transpose()
}

/// Record a response to a question. The response must reference an open
/// question by its id; responding to a missing or already-answered question is
/// rejected, so a response can never satisfy a different request than the one
/// it names.
pub fn respond(conn: &Connection, question_id: i64) -> Result<(), ApprovalError> {
    let tx = conn.unchecked_transaction()?;
    let question =
        get_question(&tx, question_id)?.ok_or(ApprovalError::NotFound { id: question_id })?;
    if question.status != QuestionStatus::Open {
        return Err(ApprovalError::NotDecidable { id: question_id });
    }
    tx.execute(
        "UPDATE pending_questions SET status = 'answered' WHERE id = ?1 AND status = 'open'",
        rusqlite::params![question_id],
    )?;
    tx.commit()?;
    Ok(())
}
