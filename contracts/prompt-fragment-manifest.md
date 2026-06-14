# Prompt Fragment Manifest Contract

Every prompt fragment must declare:

- fragment ID;
- owner module;
- fragment version;
- target agent kind;
- parameters;
- ordering dependencies;
- override and precedence rules;
- conformance assertions.

Shared safety fragments are owned by shared platform modules. Product profile crates parameterize and assemble them, but must not copy or weaken them.

