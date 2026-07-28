# Instance Enablement

Instance enablement configures modules already included by a product version.

Allowed actions:

- write runtime config;
- create runtime state;
- store setup/readiness state;
- configure channel credentials and pairing;
- configure OneCLI references;
- register config-referenced specialists via `[[specialists]]` — each entry names a
  reviewed spec bundle (dropped under `<root>/specialists/`) plus capacity/pinning
  overrides (`image_digest`, `max_specialists`, `max_concurrent_jobs`,
  `max_artifact_bytes`, `max_turns`). The specialist's container image must be
  published separately; the bundle carries the reviewed `system_prompt` /
  `allowed_tools`, which are never authored in TOML;
- enable or disable included modules where the product policy permits it.

Disallowed actions:

- editing Cargo manifests;
- adding module dependencies;
- copying capability code;
- installing runtime packages;
- adding MCP servers dynamically;
- editing checked-in prompts or profile code.

