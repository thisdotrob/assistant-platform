# Instance Enablement

Instance enablement configures modules already included by a product version.

Allowed actions:

- write runtime config;
- create runtime state;
- store setup/readiness state;
- configure channel credentials and pairing;
- configure OneCLI references;
- enable or disable included modules where the product policy permits it.

Disallowed actions:

- editing Cargo manifests;
- adding module dependencies;
- copying capability code;
- installing runtime packages;
- adding MCP servers dynamically;
- editing checked-in prompts or profile code.

