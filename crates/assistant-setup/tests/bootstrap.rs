//! Bootstrap acceptance tests: dry-run writes nothing, real init creates the
//! instance + main.db, rerun is idempotent, state resumes after a simulated
//! crash, checksum drift refuses, and writes never escape the instance root.

use std::path::{Path, PathBuf};

use assistant_config::{Config, ModulesConfig, ProductConfig, WebConfig};
use assistant_db::{baseline_migrations, baseline_owner_modules, Migration, MigrationSet, VersionRecord};
use assistant_setup::{run, BootstrapInput, BootstrapOptions, SetupError, StepId};

fn module_order() -> Vec<String> {
    baseline_owner_modules()
        .into_iter()
        .map(str::to_string)
        .collect()
}

fn sample_config() -> Config {
    Config {
        product: ProductConfig {
            namespace: "assistant".to_string(),
            product_id: "assistant".to_string(),
            product_version: "0.1.0".to_string(),
            platform_version: "0.1.0".to_string(),
            instance: Some("test".to_string()),
            owner_handle: None,
        },
        modules: ModulesConfig {
            enabled: vec!["assistant-core".to_string()],
        },
        web: WebConfig::default(),
    }
}

fn version_record() -> VersionRecord {
    VersionRecord {
        product_id: "assistant".to_string(),
        product_version: "0.1.0".to_string(),
        platform_version: "0.1.0".to_string(),
        modules: vec![("assistant-core".to_string(), "0.1.0".to_string())],
    }
}

fn input(home: &Path, migrations: MigrationSet, protected: Vec<PathBuf>) -> BootstrapInput {
    BootstrapInput {
        config: sample_config(),
        home: home.to_path_buf(),
        migrations,
        version_record: version_record(),
        protected_roots: protected,
    }
}

fn instance_root(home: &Path) -> PathBuf {
    home.join(".assistant-test")
}

#[test]
fn dry_run_writes_nothing() {
    let home = tempfile::tempdir().unwrap();
    let inp = input(home.path(), baseline_migrations(module_order()), vec![]);
    let outcome = run(
        &inp,
        &BootstrapOptions {
            dry_run: true,
            stop_before: None,
        },
    )
    .unwrap();

    assert!(outcome.dry_run);
    assert_eq!(outcome.plan.steps.len(), 4);
    // The plan reports paths, but nothing is created.
    assert!(!instance_root(home.path()).exists());
}

#[test]
fn real_init_creates_instance_and_db() {
    let home = tempfile::tempdir().unwrap();
    let inp = input(home.path(), baseline_migrations(module_order()), vec![]);
    let outcome = run(&inp, &BootstrapOptions::default()).unwrap();

    assert_eq!(outcome.executed.len(), 4);
    let root = instance_root(home.path());
    assert!(root.join("config.toml").exists());
    assert!(root.join("main.db").exists());
    assert!(root.join("sessions").is_dir());
    assert!(root.join("logs").is_dir());
    assert!(root.join("setup/state.json").exists());
    assert!(root.join("setup/readiness.json").exists());
    assert!(root.join("logs/setup.log").exists());
}

#[test]
fn rerun_is_idempotent() {
    let home = tempfile::tempdir().unwrap();
    let inp = input(home.path(), baseline_migrations(module_order()), vec![]);
    run(&inp, &BootstrapOptions::default()).unwrap();

    let inp2 = input(home.path(), baseline_migrations(module_order()), vec![]);
    let second = run(&inp2, &BootstrapOptions::default()).unwrap();
    assert!(second.executed.is_empty());
    assert_eq!(second.skipped.len(), 4);
}

#[test]
fn resumes_after_simulated_crash() {
    let home = tempfile::tempdir().unwrap();

    // First run stops before MigrateDb (as if the process died).
    let inp = input(home.path(), baseline_migrations(module_order()), vec![]);
    let first = run(
        &inp,
        &BootstrapOptions {
            dry_run: false,
            stop_before: Some(StepId::MigrateDb),
        },
    )
    .unwrap();
    assert_eq!(first.executed, vec![StepId::CreateDirectories, StepId::WriteConfig]);
    assert!(!instance_root(home.path()).join("main.db").exists());

    // Resume: only the remaining step runs.
    let inp2 = input(home.path(), baseline_migrations(module_order()), vec![]);
    let resumed = run(&inp2, &BootstrapOptions::default()).unwrap();
    assert_eq!(resumed.executed, vec![StepId::MigrateDb, StepId::WriteReadiness]);
    assert_eq!(resumed.skipped.len(), 2);
    assert!(instance_root(home.path()).join("main.db").exists());
    assert!(instance_root(home.path()).join("setup/readiness.json").exists());
}

#[test]
fn checksum_mismatch_refuses_on_resume() {
    let home = tempfile::tempdir().unwrap();

    // Initial migration set defines assistant-core v1.
    let mut original = MigrationSet::new(vec!["assistant-core".to_string()]);
    original.add(Migration::new(
        "assistant-core",
        1,
        "thing",
        "CREATE TABLE thing (id INTEGER);",
    ));
    let inp = input(home.path(), original, vec![]);
    run(&inp, &BootstrapOptions::default()).unwrap();

    // Tamper with the same migration, then force the migrate step to rerun by
    // clearing it from completed state.
    let state_path = instance_root(home.path()).join("setup/state.json");
    let raw = std::fs::read_to_string(&state_path).unwrap();
    let cleaned = raw.replace("\"migrate_db\"", "\"migrate_db_done_elsewhere\"");
    std::fs::write(&state_path, cleaned).unwrap();

    let mut drifted = MigrationSet::new(vec!["assistant-core".to_string()]);
    drifted.add(Migration::new(
        "assistant-core",
        1,
        "thing",
        "CREATE TABLE thing (id INTEGER, extra TEXT);",
    ));
    let inp2 = input(home.path(), drifted, vec![]);
    let err = run(&inp2, &BootstrapOptions::default()).unwrap_err();
    assert!(matches!(err, SetupError::Db(_)));
}

#[test]
fn refuses_instance_inside_protected_source_repo() {
    // home itself is "inside" a protected root => instance root would be too.
    let protected = tempfile::tempdir().unwrap();
    let home = protected.path().join("nested-home");
    std::fs::create_dir_all(&home).unwrap();

    let inp = input(
        &home,
        baseline_migrations(module_order()),
        vec![protected.path().to_path_buf()],
    );
    let err = run(&inp, &BootstrapOptions::default()).unwrap_err();
    assert!(matches!(err, SetupError::SourceMutation { .. }));
}

#[test]
fn protected_source_repo_is_unchanged() {
    let home = tempfile::tempdir().unwrap();
    // A separate "source repo" with a tracked file.
    let source = tempfile::tempdir().unwrap();
    let tracked = source.path().join("lib.rs");
    std::fs::write(&tracked, "pub fn untouched() {}").unwrap();
    let before = std::fs::read_to_string(&tracked).unwrap();

    let inp = input(
        home.path(),
        baseline_migrations(module_order()),
        vec![source.path().to_path_buf()],
    );
    run(&inp, &BootstrapOptions::default()).unwrap();

    let after = std::fs::read_to_string(&tracked).unwrap();
    assert_eq!(before, after);
    // Nothing new appeared in the source repo.
    let entries: Vec<_> = std::fs::read_dir(source.path()).unwrap().collect();
    assert_eq!(entries.len(), 1);
}
