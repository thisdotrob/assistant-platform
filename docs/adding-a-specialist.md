# Adding a Specialist

A specialist is a sub-agent the orchestrator can `delegate` to: it runs one
authed Claude turn in its own job-keyed container on its own custom image, with a
restricted toolset, and its result is re-injected so the orchestrator replies in
its own voice. See `contracts/assistant-host.md` for the delegation lifecycle.

There are two ways to add one:

- **Compiled baseline** — a product imports a specialist crate and registers it in
  `assistant_product::Product.specialists` (this is how `cleoclaw-v2` ships the
  `browser` specialist). Requires a product code change and rebuild.
- **Config-referenced** — an instance drops a reviewed spec bundle and a
  `[[specialists]]` block into its config. No fork, no rebuild. This is the
  fork-free customisation path and the subject of this guide.

Config-resolved specialists are appended to the compiled baseline. A `route_name`
that collides across the two sources is a hard error, since the route name is the
`delegate` menu's enum value and must be unique.

The one thing config **cannot** do is produce an image, so the specialist's image
is always built and published separately, then referenced by digest.

---

## Worked example: a `calendar` specialist

Scenario: the Payments squad runs a `cleoclaw` instance named `payments` (root
`~/.cleoclaw-payments`, already shipping the compiled `browser` specialist) and
wants it to answer scheduling questions. The `cal` CLI, the GHCR org, and the
digest below are illustrative placeholders — substitute your real tool, registry,
and published digest.

### 1. Prerequisites

- A OneCLI gateway reachable from the host (credentials the Claude turns). Note its
  proxy URL, e.g. `http://127.0.0.1:10355`.
- A Slack app for the instance (bot token + channel pairing).

### 2. Bootstrap and set up the instance

```sh
cleoclaw bootstrap init --instance payments   # add --dry-run first to preview
cleoclaw setup --instance payments
```

This writes `~/.cleoclaw-payments/{config.toml,main.db,sessions/,logs/,setup/}`.
Then edit `config.toml` to add the Slack credentials/pairing and OneCLI references
(the enablement actions in `docs/instance-enablement.md`).

### 3. Author the specialist crate

A standalone repo laid out like `assistant-specialist-browser`: it git-deps the
platform crates and provides `src/spec.rs` (the `SpecialistSpec` builder),
`src/bin/emit-spec.rs`, `image/Dockerfile`, and a `publish-image.yml` workflow.

`src/spec.rs`:

```rust
use assistant_specialist_spec::SpecialistSpec;

pub const CALENDAR_ROUTE_NAME: &str = "calendar";
pub const CALENDAR_IMAGE_REPOSITORY: &str =
    "ghcr.io/payments-squad/assistant-specialist-calendar";
// Bumped on every republish; None falls back to repository:tag.
pub const CALENDAR_IMAGE_DIGEST: Option<&str> = None;

const CALENDAR_SYSTEM_PROMPT: &str = "You are a calendar specialist. You have a \
`cal` command available through the Bash tool that reads the team calendar. Use \
it to answer scheduling and availability questions.

Core workflow:
- `cal agenda <start> <end>` to list events in a window.
- `cal freebusy <person> <date>` to check availability.
Write a clear, factual answer in plain prose — exact dates, times, and event \
titles. Your answer is relayed to the person who asked, so state only the \
findings: never mention the tool, the commands, or that you are a specialist.";

pub fn calendar_specialist_spec() -> SpecialistSpec {
    SpecialistSpec {
        route_name: CALENDAR_ROUTE_NAME.to_string(),
        description: "reads the team calendar — for scheduling, availability, and \
            event lookups".to_string(),
        profile_id: "calendar-specialist".to_string(),
        profile_version: "0.1.0".to_string(),
        group_slug: "calendar-1".to_string(),
        image_repository: CALENDAR_IMAGE_REPOSITORY.to_string(),
        image_tag: "0.1.0".to_string(),
        image_digest: CALENDAR_IMAGE_DIGEST.map(str::to_string),
        max_specialists: 1,
        max_concurrent_jobs: 8,
        max_artifact_bytes: 1024 * 1024,
        system_prompt: CALENDAR_SYSTEM_PROMPT.to_string(),
        tools: vec!["Bash".to_string()],
        allowed_tools: vec!["Bash(cal:*)".to_string()],
        max_turns: 30,
        extra_env: Vec::new(),
    }
}
```

`src/bin/emit-spec.rs`:

```rust
use assistant_specialist_calendar::calendar_specialist_spec;

fn main() {
    let spec = calendar_specialist_spec();
    println!("{}", serde_json::to_string_pretty(&spec).unwrap());
}
```

`image/Dockerfile` — the slim base plus only the tool this specialist needs:

```dockerfile
ARG BASE_IMAGE=ghcr.io/thisdotrob/assistant-base:0.1.0
FROM ${BASE_IMAGE}
# The one reason this specialist ships its own image: the `cal` CLI + creds.
RUN apt-get update && apt-get install -y python3-pip && rm -rf /var/lib/apt/lists/* \
    && pip install gcalcli
COPY cal /usr/local/bin/cal
# ENTRYPOINT + ASSISTANT_SESSION_DIR inherited from assistant-base.
```

### 4. Publish the image and note its digest

Config can reference an image but cannot produce one, so publish it first (built
outside the sandbox — apt/pip need network). Tagging `v0.1.0` triggers the repo's
`publish-image.yml`, which pushes multi-arch to GHCR:

```sh
git tag v0.1.0 && git push origin v0.1.0
docker buildx imagetools inspect \
  ghcr.io/payments-squad/assistant-specialist-calendar:0.1.0   # -> sha256:abc123...
```

### 5. Emit the bundle into the instance

```sh
cargo run --bin emit-spec > ~/.cleoclaw-payments/specialists/calendar.json
```

### 6. Register it in config

Append to `~/.cleoclaw-payments/config.toml`:

```toml
[[specialists]]
bundle = "calendar.json"            # relative to specialists/, no .. or absolute
# enabled = true                    # default

[specialists.overrides]
image_digest = "sha256:abc123..."    # pin the exact published bytes
max_concurrent_jobs = 4              # capacity/pinning only
```

Overrides are capacity/pinning only (`image_digest`, `max_specialists`,
`max_concurrent_jobs`, `max_artifact_bytes`, `max_turns`). The security-bearing
fields — `system_prompt`, `tools`, `allowed_tools` — come from the reviewed bundle
and are never hand-authored in TOML.

### 7. Serve and verify

```sh
cleoclaw serve-slack --claude --instance payments --proxy-url http://127.0.0.1:10355
```

The orchestrator's `delegate` menu now lists both `browser` (compiled) and
`calendar` (config-registered). In Slack:

> **@assistant** am I free Thursday afternoon?

The orchestrator emits `delegate(calendar)`; a `calendar-{job_id}` container runs
on the pinned image, drives `cal freebusy` in one authed Claude turn, and the
finding is re-injected so the orchestrator answers in its own voice, threaded
under the trigger.

---

## Security note

A bundle carries the specialist's security contract: its system prompt, the tools
it may call, and the auto-approve patterns (`allowed_tools`). Treat the bundle as
reviewed code — it should come from the specialist crate's build, not be
hand-edited in the instance. Config overrides deliberately cannot widen what a
specialist may execute; they only tune capacity and pin the image.
