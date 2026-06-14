//! Integration test that validates the actual checked-in assistant-platform
//! artifacts: the platform release manifest, the prompt-fragment manifest, and
//! the module contracts. It proves the platform's own files are internally
//! consistent (acyclic graph, valid fragment ordering, full contract coverage).
//!
//! The product-vs-platform compatibility path (which needs a product manifest)
//! is exercised by running the product binaries during verification, not here.
//!
//! assistant-core lives at `<platform_root>/crates/assistant-core`, so the platform root
//! is two directories up from `CARGO_MANIFEST_DIR`.

use std::collections::BTreeSet;
use std::path::PathBuf;

use assistant_core::compat::{load_platform_manifest, load_prompt_fragment_manifest, scan_contracts};
use assistant_core::validate::{
    check_contract_presence, validate_module_graph, validate_prompt_fragments,
};

fn platform_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
}

#[test]
fn real_platform_manifests_and_contracts_validate_clean() {
    let root = platform_root();

    let platform = load_platform_manifest(&root).expect("platform manifest loads");
    let fragments = load_prompt_fragment_manifest(&root).expect("prompt-fragment manifest loads");
    let contracts = scan_contracts(&root).expect("contracts directory scans");

    let mut diags = Vec::new();
    validate_module_graph(&platform, &mut diags);

    let known_modules: BTreeSet<&str> = platform.modules.iter().map(|m| m.id.as_str()).collect();
    validate_prompt_fragments(&fragments, &known_modules, &mut diags);

    let module_ids: Vec<&str> = platform.modules.iter().map(|m| m.id.as_str()).collect();
    check_contract_presence(&module_ids, &contracts, &mut diags);

    assert!(
        diags.is_empty(),
        "real platform manifests/contracts produced diagnostics: {diags:#?}"
    );
}
