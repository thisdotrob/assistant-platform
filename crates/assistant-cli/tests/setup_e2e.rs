//! End-to-end proof of the product setup entry point: the foundational
//! bootstrap runs first (creating the instance under a temp HOME), then the
//! product-supplied `SetupStep`s run as a resumable pipeline. A failing step
//! stops the run and yields a non-zero exit code.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use assistant_cli::{setup, BootstrapRequest, FnStep, SetupError};

fn request(home: &std::path::Path) -> BootstrapRequest {
    BootstrapRequest {
        namespace: "testns".to_string(),
        product_id: "testprod".to_string(),
        product_version: "0.1.0".to_string(),
        instance: None,
        enabled_modules: Vec::new(),
        home: Some(home.to_path_buf()),
        protected_roots: Vec::new(),
        dry_run: false,
    }
}

fn instance_root(home: &std::path::Path) -> PathBuf {
    home.join(".testns")
}

#[test]
fn setup_with_no_steps_bootstraps_the_instance_and_succeeds() {
    let home = tempfile::tempdir().unwrap();

    let code = setup(request(home.path()), Vec::new());

    assert_eq!(code, 0);
    let root = instance_root(home.path());
    assert!(root.join("config.toml").exists(), "config.toml written");
    assert!(root.join("main.db").exists(), "central db migrated");
}

#[test]
fn setup_runs_product_steps_after_bootstrap() {
    let home = tempfile::tempdir().unwrap();

    static RAN: AtomicUsize = AtomicUsize::new(0);
    let step = FnStep::new("product_step", "a product setup step", |ctx| {
        RAN.fetch_add(1, Ordering::SeqCst);
        // The step sees the bootstrapped instance layout.
        assert!(ctx.layout().config_path().exists());
        Ok("did the thing".to_string())
    })
    .boxed();

    let code = setup(request(home.path()), vec![step]);

    assert_eq!(code, 0);
    assert_eq!(RAN.load(Ordering::SeqCst), 1, "the product step ran once");
}

#[test]
fn a_failing_product_step_yields_a_nonzero_exit_code() {
    let home = tempfile::tempdir().unwrap();

    let step = FnStep::new("doomed_step", "a step that fails", |_ctx| {
        Err(SetupError::Gate {
            id: "doomed_step".to_string(),
            detail: "not satisfied".to_string(),
        })
    })
    .boxed();

    let code = setup(request(home.path()), vec![step]);

    assert_eq!(code, 1);
    // The foundational bootstrap still ran before the failing product step.
    assert!(instance_root(home.path()).join("config.toml").exists());
}
