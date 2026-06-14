//! Reusable admin actions both products expose through thin subcommands.
//!
//! The Slack serve path is deny-by-default ([`claw_permissions::UnknownPolicy::Strict`]):
//! an inbound message only drives a turn when its `(channel, address)` resolves
//! to a known user. `register_user` is how an operator makes a sender known —
//! it creates (or reuses) the user and binds the DM route the sender gate reads.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use claw_config::{home_dir, InstanceLayout};
use claw_db::open_central;
use claw_permissions::{
    add_user_dm, bootstrap_owner, create_user, find_user_by_handle, PermissionError,
};
use claw_scheduler::{upsert_item, ContextPolicy, Recurrence, ScheduleIntent, ScheduledMessageMeta};

use crate::error::HostError;
use crate::HOST_AGENT_GROUP;

/// Inputs for [`register_user`]. Mirrors the run/serve option structs: the
/// product supplies its namespace and the on-disk home, and the platform derives
/// the instance layout and central DB.
pub struct RegisterUserOptions {
    pub namespace: String,
    pub instance: Option<String>,
    pub home: Option<PathBuf>,
    /// The user's unique handle (e.g. `rob`).
    pub handle: String,
    /// Optional display name.
    pub display_name: Option<String>,
    /// The channel the DM route is on (e.g. `slack`).
    pub channel: String,
    /// The platform address on that channel (e.g. a Slack user id `U0123ABC`).
    pub address: String,
    /// Register this user as the instance owner (the bootstrap admin identity).
    /// Fails if an owner already exists and this handle is new.
    pub owner: bool,
}

/// Register (or look up) a user and bind a DM route, returning a process exit
/// code (0 on success, 1 on error).
pub fn register_user(opts: RegisterUserOptions) -> i32 {
    match register_user_inner(opts) {
        Ok(id) => {
            println!("registered user #{id}");
            0
        }
        Err(e) => {
            eprintln!("register-user error: {e}");
            1
        }
    }
}

fn register_user_inner(opts: RegisterUserOptions) -> Result<i64, HostError> {
    let home = opts
        .home
        .clone()
        .or_else(home_dir)
        .ok_or_else(|| HostError::Layout("HOME is not set; pass --home <path>".to_string()))?;
    let layout = InstanceLayout::derive(&home, &opts.namespace, opts.instance.as_deref())
        .map_err(|e| HostError::Layout(e.to_string()))?;
    let conn = open_central(&layout.central_db_path()).map_err(|e| HostError::Db(e.to_string()))?;

    // Idempotent: an existing handle is reused (so a re-run just refreshes the DM
    // route below) rather than failing on the unique-handle constraint.
    let user_id = match find_user_by_handle(&conn, &opts.handle).map_err(as_db)? {
        Some(user) => user.id,
        None if opts.owner => bootstrap_owner(&conn, &opts.handle, opts.display_name.as_deref())
            .map_err(as_db)?,
        None => create_user(&conn, &opts.handle, opts.display_name.as_deref()).map_err(as_db)?,
    };
    add_user_dm(&conn, user_id, &opts.channel, &opts.address).map_err(as_db)?;
    Ok(user_id)
}

fn as_db(e: PermissionError) -> HostError {
    HostError::Db(e.to_string())
}

/// Inputs for [`create_scheduled_message`]. A stopgap until message-driven
/// scheduling lands: an operator seeds a one-off scheduled item the live Slack
/// daemon will fire into the given channel's session.
pub struct ScheduleMessageOptions {
    pub namespace: String,
    pub instance: Option<String>,
    pub home: Option<PathBuf>,
    /// The session the firing turn runs in — the Slack channel id (e.g. `C0123`).
    pub session: String,
    /// Seconds from now until the item is due.
    pub in_seconds: i64,
    /// When set, the item recurs on this fixed interval (seconds) instead of
    /// firing once; the daemon advances it after each firing. `None` = one-off.
    pub every_seconds: Option<i64>,
    /// The message the scheduled turn processes (becomes the intent summary).
    pub text: String,
}

/// Seed a one-off scheduled item into the instance's central projection,
/// returning a process exit code (0 on success, 1 on error).
pub fn create_scheduled_message(opts: ScheduleMessageOptions) -> i32 {
    match create_scheduled_message_inner(opts) {
        Ok(id) => {
            println!("scheduled message {id}");
            0
        }
        Err(e) => {
            eprintln!("schedule error: {e}");
            1
        }
    }
}

fn create_scheduled_message_inner(opts: ScheduleMessageOptions) -> Result<String, HostError> {
    let home = opts
        .home
        .clone()
        .or_else(home_dir)
        .ok_or_else(|| HostError::Layout("HOME is not set; pass --home <path>".to_string()))?;
    let layout = InstanceLayout::derive(&home, &opts.namespace, opts.instance.as_deref())
        .map_err(|e| HostError::Layout(e.to_string()))?;
    let conn = open_central(&layout.central_db_path()).map_err(|e| HostError::Db(e.to_string()))?;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let intent = ScheduleIntent {
        created_by: "admin".to_string(),
        summary: opts.text,
        created_at: now,
    };
    // One-off by default, or a fixed-interval recurrence when --every-seconds is
    // given; default context policy. The projection row is all the live
    // scheduler's `claim_due` needs to fire it.
    let recurrence = opts.every_seconds.map(|seconds| Recurrence::Every { seconds });
    let meta = ScheduledMessageMeta::create(
        HOST_AGENT_GROUP,
        intent,
        now + opts.in_seconds,
        recurrence,
        ContextPolicy::default(),
    )
    .map_err(|e| HostError::Layout(e.to_string()))?;
    upsert_item(&conn, &meta, Some(&opts.session)).map_err(|e| HostError::Db(e.to_string()))?;
    Ok(meta.scheduled_item_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use claw_db::{apply, baseline_migrations, baseline_owner_modules};
    use claw_permissions::{resolve_user_by_dm, UnknownPolicy};
    use std::path::Path;

    /// A temp home with the instance layout's central DB migrated to the baseline,
    /// as the bootstrap's MigrateDb step would leave it. Returns the home dir
    /// (kept alive) and the derived layout.
    fn prepared() -> (tempfile::TempDir, InstanceLayout) {
        let home = tempfile::tempdir().unwrap();
        let layout = InstanceLayout::derive(home.path(), "assistant", Some("test")).unwrap();
        for dir in layout.managed_dirs() {
            std::fs::create_dir_all(&dir).unwrap();
        }
        let order: Vec<String> = baseline_owner_modules()
            .into_iter()
            .map(str::to_string)
            .collect();
        let mut conn = open_central(&layout.central_db_path()).unwrap();
        apply(&mut conn, &baseline_migrations(order)).unwrap();
        (home, layout)
    }

    fn opts(home: &Path, handle: &str, address: &str, owner: bool) -> RegisterUserOptions {
        RegisterUserOptions {
            namespace: "assistant".to_string(),
            instance: Some("test".to_string()),
            home: Some(home.to_path_buf()),
            handle: handle.to_string(),
            display_name: None,
            channel: "slack".to_string(),
            address: address.to_string(),
            owner,
        }
    }

    #[test]
    fn registering_binds_a_dm_route_that_resolves() {
        let (home, layout) = prepared();
        let id = register_user_inner(opts(home.path(), "rob", "U_ROB", true)).unwrap();

        let conn = open_central(&layout.central_db_path()).unwrap();
        assert_eq!(resolve_user_by_dm(&conn, "slack", "U_ROB").unwrap(), Some(id));
        // The bound sender now passes the deny-by-default gate.
        assert!(
            claw_permissions::evaluate_sender(&conn, "slack", "U_ROB", UnknownPolicy::Strict)
                .unwrap()
                .is_allow()
        );
    }

    #[test]
    fn rerun_is_idempotent_and_reuses_the_handle() {
        let (home, layout) = prepared();
        let first = register_user_inner(opts(home.path(), "rob", "U_ROB", true)).unwrap();
        // A second run with the same handle reuses the user (no unique-handle
        // failure) and refreshes the DM route to the new address.
        let second = register_user_inner(opts(home.path(), "rob", "U_ROB_2", false)).unwrap();
        assert_eq!(first, second);

        let conn = open_central(&layout.central_db_path()).unwrap();
        assert_eq!(resolve_user_by_dm(&conn, "slack", "U_ROB_2").unwrap(), Some(first));
    }
}
