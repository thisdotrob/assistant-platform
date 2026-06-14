//! Shared filesystem-guard and logging helpers used by both the M1 bootstrap
//! and the M12 setup pipeline, so the two phases enforce identical write
//! confinement and write logs the same way.

use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use claw_config::InstanceLayout;

use crate::error::SetupError;
use crate::state::{save_state, SetupState};

/// Refuse to write a path that escapes the instance root or lands inside a
/// protected source repo. This is the single confinement rule every setup step
/// goes through before touching the disk.
pub(crate) fn guard_writable(
    instance_root: &Path,
    path: &Path,
    protected_roots: &[PathBuf],
) -> Result<(), SetupError> {
    // `starts_with` is lexical, so a `..` segment could prefix-match the root
    // yet resolve outside it. Reject any parent-dir component so the prefix
    // check below is sound without canonicalizing (which would touch the FS).
    if path.components().any(|c| c == Component::ParentDir) {
        return Err(SetupError::SourceMutation {
            path: path.to_path_buf(),
        });
    }
    if !path.starts_with(instance_root) {
        return Err(SetupError::SourceMutation {
            path: path.to_path_buf(),
        });
    }
    for protected in protected_roots {
        if path.starts_with(protected) {
            return Err(SetupError::SourceMutation {
                path: path.to_path_buf(),
            });
        }
    }
    Ok(())
}

/// Persist setup state only if the setup dir already exists (it is created by
/// the first bootstrap step, so a pre-dir failure leaves nothing to write).
pub(crate) fn persist_state_if_possible(
    layout: &InstanceLayout,
    state: &SetupState,
) -> Result<(), SetupError> {
    if layout.setup_dir().exists() {
        save_state(&layout.setup_state_path(), state)?;
    }
    Ok(())
}

/// Append one line to the human-readable `setup.log`. Best-effort: if the logs
/// dir does not exist yet (early failure) the line is dropped rather than
/// erroring the run.
pub(crate) fn append_setup_log(layout: &InstanceLayout, line: &str) {
    if !layout.logs_dir().exists() {
        return;
    }
    if let Ok(mut file) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(layout.setup_log_path())
    {
        let _ = writeln!(file, "{}", sanitize_log_line(line));
    }
}

/// Collapse control characters (newlines especially) to spaces so a step's
/// detail or error string cannot forge extra lines in the shared `setup.log`.
fn sanitize_log_line(line: &str) -> String {
    line.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Write a step's raw detail to its own `setup-<id>.log`. Best-effort.
pub(crate) fn write_step_log(layout: &InstanceLayout, step_id: &str, detail: &str) {
    let dir = layout.logs_dir();
    if !dir.exists() {
        return;
    }
    let _ = fs::write(dir.join(format!("setup-{step_id}.log")), detail);
}
