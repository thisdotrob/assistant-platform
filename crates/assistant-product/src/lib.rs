//! The shared product runner. Both product binaries (`assistant-v2`,
//! `cleoclaw-v2`) are identical except for a handful of config values and the
//! specialists they register, so the entire CLI arg-parse + dispatch lives here
//! once. A product's `main` builds a [`Product`] and calls [`run`].

use std::path::PathBuf;

use assistant_core::{platform_metadata, ProductMetadata};
use assistant_host::SpecialistSpec;

/// Everything a product varies from the shared dispatch. `product_id` and
/// `product_version`/`product_root` must be supplied by the product (so
/// `env!("CARGO_PKG_VERSION")`/`env!("CARGO_MANIFEST_DIR")` resolve at the
/// product's compile site, not this crate's).
pub struct Product {
    /// Product identifier; also used as the `namespace` (equal today).
    pub product_id: &'static str,
    pub product_version: &'static str,
    pub profile_id: &'static str,
    pub profile_version: &'static str,
    pub product_root: PathBuf,
    /// The specialists this product registers (e.g. the browser specialist).
    pub specialists: Vec<SpecialistSpec>,
    /// The product's memory categories. Not yet consumed by `run()` — kept so
    /// the per-product taxonomy decision survives until memory scaffolding wires
    /// it up. See <ticket>.
    pub memory_taxonomy: Vec<&'static str>,
}

/// Parse argv and dispatch the requested command, returning the process exit
/// code. The product's `main` should `std::process::exit` with the result.
pub fn run(product: Product) -> i32 {
    let args: Vec<String> = std::env::args().collect();

    if args.get(1).map(String::as_str) == Some("doctor")
        && args.get(2).map(String::as_str) == Some("compatibility")
    {
        // Default: validate against the platform inputs compiled into this
        // binary (no `assistant-platform` checkout needed). `--platform-path`
        // overrides with an on-disk platform checkout.
        let mut platform_root: Option<PathBuf> = None;
        let mut product_root: PathBuf = product.product_root.clone();

        let rest = &args[3..];
        let mut i = 0;
        while i < rest.len() {
            match rest[i].as_str() {
                "--platform-path" => {
                    if let Some(value) = rest.get(i + 1) {
                        platform_root = Some(PathBuf::from(value));
                        i += 2;
                        continue;
                    }
                }
                "--product-path" => {
                    if let Some(value) = rest.get(i + 1) {
                        product_root = PathBuf::from(value);
                        i += 2;
                        continue;
                    }
                }
                _ => {}
            }
            i += 1;
        }

        let code = assistant_cli::doctor_compatibility(platform_root.as_deref(), &product_root);
        return code;
    }

    if args.get(1).map(String::as_str) == Some("bootstrap")
        && args.get(2).map(String::as_str) == Some("init")
    {
        let mut dry_run = false;
        let mut instance: Option<String> = None;
        let mut home: Option<PathBuf> = None;

        let rest = &args[3..];
        let mut i = 0;
        while i < rest.len() {
            match rest[i].as_str() {
                "--dry-run" => {
                    dry_run = true;
                    i += 1;
                    continue;
                }
                "--instance" => {
                    if let Some(value) = rest.get(i + 1) {
                        instance = Some(value.clone());
                        i += 2;
                        continue;
                    }
                }
                "--home" => {
                    if let Some(value) = rest.get(i + 1) {
                        home = Some(PathBuf::from(value));
                        i += 2;
                        continue;
                    }
                }
                _ => {}
            }
            i += 1;
        }

        let product_root = product.product_root.clone();

        let code = assistant_cli::bootstrap(assistant_cli::BootstrapRequest {
            namespace: product.product_id.to_string(),
            product_id: product.product_id.to_string(),
            product_version: product.product_version.to_string(),
            instance,
            enabled_modules: platform_metadata()
                .module_ids
                .iter()
                .map(|s| s.to_string())
                .collect(),
            home,
            protected_roots: vec![product_root],
            dry_run,
        });
        return code;
    }

    if args.get(1).map(String::as_str) == Some("setup") {
        let mut dry_run = false;
        let mut instance: Option<String> = None;
        let mut home: Option<PathBuf> = None;

        let rest = &args[2..];
        let mut i = 0;
        while i < rest.len() {
            match rest[i].as_str() {
                "--dry-run" => {
                    dry_run = true;
                    i += 1;
                    continue;
                }
                "--instance" => {
                    if let Some(value) = rest.get(i + 1) {
                        instance = Some(value.clone());
                        i += 2;
                        continue;
                    }
                }
                "--home" => {
                    if let Some(value) = rest.get(i + 1) {
                        home = Some(PathBuf::from(value));
                        i += 2;
                        continue;
                    }
                }
                _ => {}
            }
            i += 1;
        }

        let product_root = product.product_root.clone();

        // Inherit the shared host setup pipeline (build the agent image,
        // validate the OneCLI Claude path) so a later `run` has what it needs.
        let code = assistant_cli::setup(
            assistant_cli::BootstrapRequest {
                namespace: product.product_id.to_string(),
                product_id: product.product_id.to_string(),
                product_version: product.product_version.to_string(),
                instance,
                enabled_modules: platform_metadata()
                    .module_ids
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
                home,
                protected_roots: vec![product_root],
                dry_run,
            },
            assistant_host::setup_steps(),
        );
        return code;
    }

    if args.get(1).map(String::as_str) == Some("upgrade") {
        let mut dry_run = false;
        let mut instance: Option<String> = None;
        let mut home: Option<PathBuf> = None;

        let rest = &args[2..];
        let mut i = 0;
        while i < rest.len() {
            match rest[i].as_str() {
                "--dry-run" => {
                    dry_run = true;
                    i += 1;
                    continue;
                }
                "--instance" => {
                    if let Some(value) = rest.get(i + 1) {
                        instance = Some(value.clone());
                        i += 2;
                        continue;
                    }
                }
                "--home" => {
                    if let Some(value) = rest.get(i + 1) {
                        home = Some(PathBuf::from(value));
                        i += 2;
                        continue;
                    }
                }
                _ => {}
            }
            i += 1;
        }

        let product_root = product.product_root.clone();

        let code = assistant_cli::upgrade(assistant_cli::BootstrapRequest {
            namespace: product.product_id.to_string(),
            product_id: product.product_id.to_string(),
            product_version: product.product_version.to_string(),
            instance,
            enabled_modules: platform_metadata()
                .module_ids
                .iter()
                .map(|s| s.to_string())
                .collect(),
            home,
            protected_roots: vec![product_root],
            dry_run,
        });
        return code;
    }

    if args.get(1).map(String::as_str) == Some("conformance") {
        let mut instance: Option<String> = None;
        let mut home: Option<PathBuf> = None;
        // Default: conform against the platform inputs compiled into this binary.
        // `--platform-path` overrides with an on-disk platform checkout.
        let mut platform_root: Option<PathBuf> = None;
        let mut product_root: PathBuf = product.product_root.clone();

        let rest = &args[2..];
        let mut i = 0;
        while i < rest.len() {
            match rest[i].as_str() {
                "--instance" => {
                    if let Some(value) = rest.get(i + 1) {
                        instance = Some(value.clone());
                        i += 2;
                        continue;
                    }
                }
                "--home" => {
                    if let Some(value) = rest.get(i + 1) {
                        home = Some(PathBuf::from(value));
                        i += 2;
                        continue;
                    }
                }
                "--platform-path" => {
                    if let Some(value) = rest.get(i + 1) {
                        platform_root = Some(PathBuf::from(value));
                        i += 2;
                        continue;
                    }
                }
                "--product-path" => {
                    if let Some(value) = rest.get(i + 1) {
                        product_root = PathBuf::from(value);
                        i += 2;
                        continue;
                    }
                }
                _ => {}
            }
            i += 1;
        }

        let platform = platform_metadata();
        let image_contract_result = match platform_root.as_deref() {
            Some(root) => assistant_core::compat::load_platform_manifest(root),
            None => assistant_core::compat::embedded_platform_manifest(),
        };
        let image_contract = match image_contract_result {
            Ok(manifest) => manifest.base_container_image_contract_version,
            Err(e) => {
                eprintln!("conformance error: {e}");
                return 1;
            }
        };

        let code = assistant_cli::conformance(
            assistant_cli::BootstrapRequest {
                namespace: product.product_id.to_string(),
                product_id: product.product_id.to_string(),
                product_version: product.product_version.to_string(),
                instance,
                enabled_modules: platform
                    .module_ids
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
                home,
                protected_roots: vec![product_root.clone()],
                dry_run: false,
            },
            platform_root.as_deref(),
            &product_root,
            platform.version.to_string(),
            image_contract,
            product.profile_id.to_string(),
            product.profile_version.to_string(),
        );
        return code;
    }

    if args.get(1).map(String::as_str) == Some("serve-slack") {
        let mut instance: Option<String> = None;
        let mut home: Option<PathBuf> = None;
        let mut group = "slack".to_string();
        let mut proxy_url = "http://127.0.0.1:10355".to_string();
        let mut mode = assistant_host::RunnerAuthMode::Stub;

        let rest = &args[2..];
        let mut i = 0;
        while i < rest.len() {
            match rest[i].as_str() {
                "--claude" => {
                    mode = assistant_host::RunnerAuthMode::ClaudeOAuth;
                    i += 1;
                    continue;
                }
                "--instance" => {
                    if let Some(value) = rest.get(i + 1) {
                        instance = Some(value.clone());
                        i += 2;
                        continue;
                    }
                }
                "--home" => {
                    if let Some(value) = rest.get(i + 1) {
                        home = Some(PathBuf::from(value));
                        i += 2;
                        continue;
                    }
                }
                "--group" => {
                    if let Some(value) = rest.get(i + 1) {
                        group = value.clone();
                        i += 2;
                        continue;
                    }
                }
                "--proxy-url" => {
                    if let Some(value) = rest.get(i + 1) {
                        proxy_url = value.clone();
                        i += 2;
                        continue;
                    }
                }
                _ => {}
            }
            i += 1;
        }

        let code = assistant_host::run_slack(assistant_host::SlackRunOptions {
            namespace: product.product_id.to_string(),
            instance,
            home,
            group,
            mode,
            proxy_url,
            specialists: product.specialists,
        });
        return code;
    }

    if args.get(1).map(String::as_str) == Some("run") {
        let mut instance: Option<String> = None;
        let mut home: Option<PathBuf> = None;
        let mut group = "orchestrator".to_string();
        let mut session = "default".to_string();
        let mut once = false;
        let mut mode = assistant_host::RunnerAuthMode::Stub;

        let rest = &args[2..];
        let mut i = 0;
        while i < rest.len() {
            match rest[i].as_str() {
                "--once" => {
                    once = true;
                    i += 1;
                    continue;
                }
                "--claude" => {
                    mode = assistant_host::RunnerAuthMode::ClaudeOAuth;
                    i += 1;
                    continue;
                }
                "--instance" => {
                    if let Some(value) = rest.get(i + 1) {
                        instance = Some(value.clone());
                        i += 2;
                        continue;
                    }
                }
                "--home" => {
                    if let Some(value) = rest.get(i + 1) {
                        home = Some(PathBuf::from(value));
                        i += 2;
                        continue;
                    }
                }
                "--group" => {
                    if let Some(value) = rest.get(i + 1) {
                        group = value.clone();
                        i += 2;
                        continue;
                    }
                }
                "--session" => {
                    if let Some(value) = rest.get(i + 1) {
                        session = value.clone();
                        i += 2;
                        continue;
                    }
                }
                _ => {}
            }
            i += 1;
        }

        let code = assistant_host::run(assistant_host::RunOptions {
            namespace: product.product_id.to_string(),
            instance,
            home,
            group,
            session,
            once,
            mode,
        });
        return code;
    }

    if args.get(1).map(String::as_str) == Some("register-user") {
        let mut instance: Option<String> = None;
        let mut home: Option<PathBuf> = None;
        let mut handle: Option<String> = None;
        let mut display_name: Option<String> = None;
        let mut channel = "slack".to_string();
        let mut address: Option<String> = None;
        let mut owner = false;

        let rest = &args[2..];
        let mut i = 0;
        while i < rest.len() {
            match rest[i].as_str() {
                "--owner" => {
                    owner = true;
                    i += 1;
                    continue;
                }
                "--instance" => {
                    if let Some(value) = rest.get(i + 1) {
                        instance = Some(value.clone());
                        i += 2;
                        continue;
                    }
                }
                "--home" => {
                    if let Some(value) = rest.get(i + 1) {
                        home = Some(PathBuf::from(value));
                        i += 2;
                        continue;
                    }
                }
                "--handle" => {
                    if let Some(value) = rest.get(i + 1) {
                        handle = Some(value.clone());
                        i += 2;
                        continue;
                    }
                }
                "--display-name" => {
                    if let Some(value) = rest.get(i + 1) {
                        display_name = Some(value.clone());
                        i += 2;
                        continue;
                    }
                }
                "--channel" => {
                    if let Some(value) = rest.get(i + 1) {
                        channel = value.clone();
                        i += 2;
                        continue;
                    }
                }
                "--address" => {
                    if let Some(value) = rest.get(i + 1) {
                        address = Some(value.clone());
                        i += 2;
                        continue;
                    }
                }
                _ => {}
            }
            i += 1;
        }

        let (Some(handle), Some(address)) = (handle, address) else {
            eprintln!("register-user requires --handle <handle> and --address <address>");
            return 2;
        };

        let code = assistant_host::register_user(assistant_host::RegisterUserOptions {
            namespace: product.product_id.to_string(),
            instance,
            home,
            handle,
            display_name,
            channel,
            address,
            owner,
        });
        return code;
    }

    if args.get(1).map(String::as_str) == Some("schedule") {
        let mut instance: Option<String> = None;
        let mut home: Option<PathBuf> = None;
        let mut session: Option<String> = None;
        let mut in_seconds: Option<i64> = None;
        let mut every_seconds: Option<i64> = None;
        let mut text: Option<String> = None;

        let rest = &args[2..];
        let mut i = 0;
        while i < rest.len() {
            match rest[i].as_str() {
                "--instance" => {
                    if let Some(value) = rest.get(i + 1) {
                        instance = Some(value.clone());
                        i += 2;
                        continue;
                    }
                }
                "--home" => {
                    if let Some(value) = rest.get(i + 1) {
                        home = Some(PathBuf::from(value));
                        i += 2;
                        continue;
                    }
                }
                "--session" => {
                    if let Some(value) = rest.get(i + 1) {
                        session = Some(value.clone());
                        i += 2;
                        continue;
                    }
                }
                "--in-seconds" => {
                    if let Some(value) = rest.get(i + 1) {
                        match value.parse::<i64>() {
                            Ok(n) => in_seconds = Some(n),
                            Err(_) => {
                                eprintln!("schedule: --in-seconds must be an integer");
                                return 2;
                            }
                        }
                        i += 2;
                        continue;
                    }
                }
                "--every-seconds" => {
                    if let Some(value) = rest.get(i + 1) {
                        match value.parse::<i64>() {
                            Ok(n) => every_seconds = Some(n),
                            Err(_) => {
                                eprintln!("schedule: --every-seconds must be an integer");
                                return 2;
                            }
                        }
                        i += 2;
                        continue;
                    }
                }
                "--text" => {
                    if let Some(value) = rest.get(i + 1) {
                        text = Some(value.clone());
                        i += 2;
                        continue;
                    }
                }
                _ => {}
            }
            i += 1;
        }

        let (Some(session), Some(in_seconds), Some(text)) = (session, in_seconds, text) else {
            eprintln!("schedule requires --session <id>, --in-seconds <n>, and --text <msg>");
            return 2;
        };

        let code = assistant_host::create_scheduled_message(assistant_host::ScheduleMessageOptions {
            namespace: product.product_id.to_string(),
            instance,
            home,
            session,
            in_seconds,
            every_seconds,
            text,
        });
        return code;
    }

    let platform = platform_metadata();
    let product_meta = ProductMetadata {
        id: product.product_id,
        version: product.product_version,
        compatible_platform_version: platform.version,
    };
    let profile_kind = "orchestrator";

    println!("product.id={}", product_meta.id);
    println!("product.version={}", product_meta.version);
    println!(
        "product.compatible_platform_version={}",
        product_meta.compatible_platform_version
    );
    println!("platform.id={}", platform.id);
    println!("platform.version={}", platform.version);
    println!("platform.module_count={}", platform.module_ids.len());
    println!("profile.id={}", product.profile_id);
    println!("profile.version={}", product.profile_version);
    println!("profile.kind={profile_kind}");

    0
}
