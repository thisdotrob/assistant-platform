//! OneCLI gateway integration: per-installation credential injection.
//!
//! OneCLI (the Agent Vault) is a local HTTPS MITM proxy that injects credentials
//! into a container's outbound traffic — the container never sees a raw token. On
//! the Claude OAuth path the container is given only
//! `CLAUDE_CODE_OAUTH_TOKEN=placeholder`; the gateway rewrites that placeholder to
//! the real Anthropic OAuth secret on outbound `api.anthropic.com` traffic.
//!
//! Each claw installation runs its **own** OneCLI gateway (its own stack on its
//! own port), so personal and work credential stores never mix. The gateway base
//! URL is configured per-instance via [`ONECLI_URL_ENV`]; the host queries
//! `GET <url>/api/container-config?agent=<agent>` and applies the returned proxy
//! env + CA trust anchor to the spawn (the model proven in the v1 host). The
//! per-installation OneCLI *agent* identifier is the instance directory name, so
//! credentials stay scoped to this installation.
//!
//! Security: this module never reads, logs, or echoes the Anthropic secret. The
//! secret lives only in the OneCLI store (registered at setup) and, optionally, a
//! 0600 file outside the repo whose *presence* (not contents) this module probes.
//! The CA certificate it writes is a public trust anchor, not a secret.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use assistant_config::InstanceLayout;
use assistant_runtime_docker::{Mount, OneCliReadiness, SpawnSpec};
use serde::Deserialize;

/// Base URL of this installation's OneCLI gateway (e.g.
/// `http://127.0.0.1:10254`). The host queries its `/api/container-config`
/// endpoint; the proxy URL the container uses is returned in that response, not
/// configured directly.
pub const ONECLI_URL_ENV: &str = "CLAW_ONECLI_URL";

/// Path to a 0600 file holding the Anthropic OAuth secret, kept outside the
/// repo. Only its presence/size is inspected here; the secret is registered into
/// the OneCLI store at setup, never read at run time.
pub const ANTHROPIC_SECRET_FILE_ENV: &str = "CLAW_ANTHROPIC_SECRET_FILE";

/// Base port for a OneCLI gateway. Per-installation gateways offset off this so
/// personal and work stacks don't collide on one host.
pub const ONECLI_BASE_PORT: u16 = 10254;

/// How long the host waits on a single gateway request before giving up.
const GATEWAY_TIMEOUT_SECS: u32 = 5;

/// Container config the gateway returns for an agent: the proxy env to inject and
/// the CA trust anchor to mount. Mirrors the upstream OneCLI
/// `/api/container-config` shape (camelCase JSON).
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContainerConfig {
    /// Proxy env vars (`HTTPS_PROXY`, `HTTP_PROXY`, `NODE_EXTRA_CA_CERTS`, …).
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// PEM of the CA the container must trust to verify the intercepted TLS.
    #[serde(default)]
    pub ca_certificate: Option<String>,
    /// Where the CA should be mounted inside the container.
    #[serde(default)]
    pub ca_certificate_container_path: Option<String>,
}

/// Errors applying the OneCLI gateway config to a spawn.
#[derive(Debug)]
pub enum OneCliError {
    /// `CLAW_ONECLI_URL` is unset; the Claude path has no gateway to query.
    GatewayUrlMissing,
    /// The agent identifier had characters outside the URL-safe set.
    InvalidAgent(String),
    /// `curl` could not be launched.
    Spawn(std::io::Error),
    /// The gateway returned a non-success status (curl exited non-zero).
    BadStatus { agent: String, detail: String },
    /// The gateway response was not valid container-config JSON.
    Parse(String),
    /// Writing the CA trust anchor to disk failed.
    Io(std::io::Error),
}

impl std::fmt::Display for OneCliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::GatewayUrlMissing => write!(
                f,
                "{ONECLI_URL_ENV} is not set; cannot reach the OneCLI gateway for the Claude path"
            ),
            Self::InvalidAgent(a) => write!(f, "invalid OneCLI agent identifier: {a:?}"),
            Self::Spawn(e) => write!(f, "failed to launch curl for the OneCLI gateway: {e}"),
            Self::BadStatus { agent, detail } => {
                write!(f, "OneCLI gateway rejected container-config for agent {agent:?}: {detail}")
            }
            Self::Parse(e) => write!(f, "OneCLI gateway returned malformed container-config: {e}"),
            Self::Io(e) => write!(f, "failed to write the OneCLI CA trust anchor: {e}"),
        }
    }
}

impl std::error::Error for OneCliError {}

/// On-disk conventions for this installation's OneCLI material.
pub struct OneCliPaths {
    /// The OneCLI material directory under the instance setup dir.
    pub dir: PathBuf,
    /// Locally trusted CA the container is given; written from the gateway's
    /// `caCertificate` on first fetch and mounted read-only into the container.
    pub ca_cert: PathBuf,
    /// Marker written by setup once the gateway returned a usable
    /// container-config (proxy env + CA) for this agent — i.e. the gateway is
    /// configured to intercept and inject for this installation.
    pub injection_ok_marker: PathBuf,
}

impl OneCliPaths {
    pub fn for_instance(layout: &InstanceLayout) -> Self {
        let dir = layout.setup_dir().join("onecli");
        Self {
            ca_cert: dir.join("ca.pem"),
            injection_ok_marker: dir.join("injection-ok"),
            dir,
        }
    }
}

/// The OneCLI agent identifier for an installation: the instance directory name
/// (e.g. `assistant`, `cleoclaw-work`). Credentials in the gateway are scoped to
/// this agent, so distinct installations never share secrets. Falls back to
/// `default` only if the layout root has no usable file name.
pub fn agent_identifier(layout: &InstanceLayout) -> String {
    layout
        .root
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.trim_start_matches('.').to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "default".to_string())
}

/// The gateway base URL configured for this installation, trailing slash
/// trimmed.
pub fn gateway_url() -> Option<String> {
    std::env::var(ONECLI_URL_ENV)
        .ok()
        .map(|u| u.trim_end_matches('/').to_string())
        .filter(|u| !u.is_empty())
}

/// A deterministic, per-installation gateway port so personal and work stacks
/// pick distinct ports off [`ONECLI_BASE_PORT`]. Advisory: the setup step that
/// installs a stack uses it; the host reads the resulting URL, not this.
pub fn default_gateway_port(layout: &InstanceLayout) -> u16 {
    let agent = agent_identifier(layout);
    // Small FNV-1a hash over the agent id, folded into a 100-port window so the
    // ports stay human-readable (10254..=10353) and collisions are rare.
    let mut hash: u32 = 2166136261;
    for byte in agent.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(16777619);
    }
    ONECLI_BASE_PORT + (hash % 100) as u16
}

/// Agent identifiers are interpolated into a URL query; restrict to the
/// instance-name alphabet so nothing odd reaches the gateway URL.
fn agent_is_url_safe(agent: &str) -> bool {
    !agent.is_empty()
        && agent
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Fetch the container config for `agent` from the OneCLI gateway by shelling
/// `curl` (the host has no async HTTP stack; this matches how the runtime shells
/// `docker`). Only ever runs on the Claude path, outside the offline sandbox.
pub fn fetch_container_config(
    gateway_url: &str,
    agent: &str,
) -> Result<ContainerConfig, OneCliError> {
    if !agent_is_url_safe(agent) {
        return Err(OneCliError::InvalidAgent(agent.to_string()));
    }
    let url = format!(
        "{}/api/container-config?agent={agent}",
        gateway_url.trim_end_matches('/')
    );
    let output = Command::new("curl")
        .args([
            "-sf",
            "--max-time",
            &GATEWAY_TIMEOUT_SECS.to_string(),
            &url,
        ])
        .output()
        .map_err(OneCliError::Spawn)?;
    if !output.status.success() {
        return Err(OneCliError::BadStatus {
            agent: agent.to_string(),
            detail: format!("curl exited with {}", output.status),
        });
    }
    parse_container_config(&output.stdout)
}

/// Parse a gateway `/api/container-config` body. Split out so the camelCase
/// mapping is unit-testable without a live gateway.
pub fn parse_container_config(body: &[u8]) -> Result<ContainerConfig, OneCliError> {
    serde_json::from_slice(body).map_err(|e| OneCliError::Parse(e.to_string()))
}

/// Apply a fetched container config to a spawn: inject the proxy env and, when a
/// CA is supplied, write it under `ca_dir` and mount it read-only at the
/// container path the gateway named.
///
/// The CA mount is added **after** `prepare_spawn` ran `validate_mounts`, on
/// purpose: `validate_mounts` blocks `.pem`/`.key` paths as credential material,
/// but the OneCLI CA is a host-controlled public trust anchor at a gateway-fixed
/// container path — exactly what the v1 host mounted directly. It carries no
/// secret.
pub fn apply_to_spec(
    spec: &mut SpawnSpec,
    config: &ContainerConfig,
    ca_dir: &Path,
) -> Result<(), OneCliError> {
    // Deterministic env ordering keeps the resulting docker args reproducible.
    let mut entries: Vec<(&String, &String)> = config.env.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    for (key, value) in entries {
        spec.env.push((key.clone(), value.clone()));
    }

    if let Some((ca_file, container_path)) = write_ca(config, ca_dir)? {
        spec.mounts.push(Mount::read_only(ca_file, container_path));
    }
    Ok(())
}

/// Write the gateway CA to `ca_dir/ca.pem` when the config carries one. Returns
/// the on-disk path and the container path it should be mounted at, or `None`
/// when the config has no CA. The CA is a public trust anchor, not a secret.
fn write_ca(
    config: &ContainerConfig,
    ca_dir: &Path,
) -> Result<Option<(PathBuf, PathBuf)>, OneCliError> {
    let (Some(pem), Some(container_path)) =
        (&config.ca_certificate, &config.ca_certificate_container_path)
    else {
        return Ok(None);
    };
    std::fs::create_dir_all(ca_dir).map_err(OneCliError::Io)?;
    let ca_file = ca_dir.join("ca.pem");
    std::fs::write(&ca_file, pem).map_err(OneCliError::Io)?;
    Ok(Some((ca_file, PathBuf::from(container_path))))
}

/// Fetch from this installation's gateway and apply to the spawn in one step.
/// `ca_dir` is where the gateway CA is written before being mounted; the run-loop
/// passes [`OneCliPaths::for_instance`]'s `dir`, which it derived at startup (the
/// run-loop holds no [`InstanceLayout`]).
pub fn apply_gateway_config(
    spec: &mut SpawnSpec,
    ca_dir: &Path,
    agent: &str,
) -> Result<(), OneCliError> {
    let url = gateway_url().ok_or(OneCliError::GatewayUrlMissing)?;
    let config = fetch_container_config(&url, agent)?;
    apply_to_spec(spec, &config, ca_dir)
}

/// Derive the agent-authenticated proxy URL the *host* must use for outbound
/// calls that need OneCLI injection for `agent` (e.g. the host-side Slack
/// requests). The gateway scopes injection per agent and identifies the agent by
/// a token spliced into the proxy URL's userinfo; the container receives that via
/// `HTTPS_PROXY`. The host can't reach the container-facing authority
/// (`host.docker.internal`), so this keeps the gateway's userinfo (the agent
/// token) but rebinds it onto `host_proxy`, the host-reachable proxy endpoint
/// (e.g. `http://127.0.0.1:10355`). The returned URL carries the agent token, so
/// callers must feed it to curl via stdin config — never argv or logs.
pub fn host_proxy_url(agent: &str, host_proxy: &str) -> Result<String, OneCliError> {
    let url = gateway_url().ok_or(OneCliError::GatewayUrlMissing)?;
    let config = fetch_container_config(&url, agent)?;
    let gateway_proxy = config.env.get("HTTPS_PROXY").ok_or_else(|| {
        OneCliError::Parse("container-config has no HTTPS_PROXY to authenticate against".to_string())
    })?;
    splice_proxy_userinfo(gateway_proxy, host_proxy)
}

/// Splice the userinfo (the agent token) from the gateway's container-facing
/// proxy URL onto the host-reachable proxy authority. Pure string surgery so it
/// is unit-testable without a gateway. Error messages never include either URL,
/// since the gateway one carries the agent token.
fn splice_proxy_userinfo(gateway_proxy: &str, host_proxy: &str) -> Result<String, OneCliError> {
    fn authority(u: &str) -> Option<&str> {
        u.split_once("://")
            .map(|(_, rest)| rest.split(['/', '?']).next().unwrap_or(rest))
    }
    let g_authority = authority(gateway_proxy)
        .ok_or_else(|| OneCliError::Parse("gateway proxy url is not absolute".to_string()))?;
    let (h_scheme, h_rest) = host_proxy
        .split_once("://")
        .ok_or_else(|| OneCliError::Parse("host proxy url is not absolute".to_string()))?;
    let h_authority = h_rest.split(['/', '?']).next().unwrap_or(h_rest);
    // Drop any userinfo already on the host endpoint; the gateway's wins.
    let h_host = h_authority.rsplit_once('@').map_or(h_authority, |(_, h)| h);

    match g_authority.rsplit_once('@') {
        Some((userinfo, _)) => Ok(format!("{h_scheme}://{userinfo}@{h_host}")),
        // Gateway proxy carried no token: nothing agent-scoped to splice.
        None => Ok(format!("{h_scheme}://{h_host}")),
    }
}

/// Provision this installation's OneCLI material at setup time: fetch the agent's
/// container-config from the gateway, persist the CA, and — when the gateway
/// returned both proxy env and a CA (i.e. it is set up to intercept and inject
/// for this agent) — record the injection marker. Returns the resulting
/// readiness so setup can report it.
///
/// Runs only live: a gateway URL must be configured, so offline/stub setup never
/// reaches it. Never reads, logs, or persists the Anthropic secret; the CA it
/// writes is a public trust anchor. This mirrors the v1 host model where a
/// successful container-config fetch is the evidence the proxy will inject.
pub fn provision_from_gateway(
    layout: &InstanceLayout,
    agent: &str,
) -> Result<OneCliReadiness, OneCliError> {
    let url = gateway_url().ok_or(OneCliError::GatewayUrlMissing)?;
    let config = fetch_container_config(&url, agent)?;
    persist_provisioned(layout, &config)
}

/// Persist a fetched container-config as this installation's OneCLI material:
/// write the CA and, when the gateway returned both proxy env and a CA, the
/// injection marker. Split out from [`provision_from_gateway`] so the marker
/// logic is unit-testable without a live gateway.
fn persist_provisioned(
    layout: &InstanceLayout,
    config: &ContainerConfig,
) -> Result<OneCliReadiness, OneCliError> {
    let paths = OneCliPaths::for_instance(layout);
    let ca = write_ca(config, &paths.dir)?;
    if ca.is_some() && !config.env.is_empty() {
        std::fs::write(&paths.injection_ok_marker, b"ok").map_err(OneCliError::Io)?;
    }
    Ok(probe(layout))
}

/// True when the secret-file env var points at an existing, non-empty file.
/// Never reads the file contents.
fn secret_present() -> bool {
    match std::env::var_os(ANTHROPIC_SECRET_FILE_ENV) {
        Some(path) => std::fs::metadata(path).map(|m| m.len() > 0).unwrap_or(false),
        None => false,
    }
}

/// Probe the three OneCLI readiness conditions for this installation:
/// - `proxy_configured`: the gateway URL is set and a trusted CA has been
///   fetched/written for this installation;
/// - `anthropic_secret_present`: the secret file is present and non-empty;
/// - `placeholder_injection_ok`: setup recorded that the gateway returned a
///   usable container-config (proxy env + CA) for this agent.
///
/// On the stub path readiness is ignored by `prepare_runner_env`, so an all-false
/// probe is harmless there; on the Claude path any false makes `prepare_spawn`
/// refuse the spawn.
pub fn probe(layout: &InstanceLayout) -> OneCliReadiness {
    let paths = OneCliPaths::for_instance(layout);
    let proxy_configured = gateway_url().is_some() && paths.ca_cert.exists();
    OneCliReadiness {
        proxy_configured,
        anthropic_secret_present: secret_present(),
        placeholder_injection_ok: paths.injection_ok_marker.exists(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assistant_runtime_docker::{ImageRef, MountMode};

    fn empty_spec() -> SpawnSpec {
        SpawnSpec {
            name: "sess-1".to_string(),
            image: ImageRef::new("assistant-base", "0.1.0"),
            mounts: Vec::new(),
            env: Vec::new(),
        }
    }

    #[test]
    fn missing_material_probes_not_ready() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = InstanceLayout::derive(tmp.path(), "assistant", Some("test")).unwrap();
        let readiness = probe(&layout);
        assert!(!readiness.is_ready());
        assert!(!readiness.placeholder_injection_ok);
    }

    #[test]
    fn agent_identifier_strips_dot_and_includes_instance() {
        let tmp = tempfile::tempdir().unwrap();
        let work = InstanceLayout::derive(tmp.path(), "assistant", Some("work")).unwrap();
        assert_eq!(agent_identifier(&work), "assistant-work");
        let default = InstanceLayout::derive(tmp.path(), "cleoclaw", None).unwrap();
        assert_eq!(agent_identifier(&default), "cleoclaw");
    }

    #[test]
    fn distinct_installations_get_distinct_default_ports() {
        let tmp = tempfile::tempdir().unwrap();
        let personal = InstanceLayout::derive(tmp.path(), "assistant", None).unwrap();
        let work = InstanceLayout::derive(tmp.path(), "cleoclaw", None).unwrap();
        assert_ne!(default_gateway_port(&personal), default_gateway_port(&work));
        // Stable across calls.
        assert_eq!(default_gateway_port(&work), default_gateway_port(&work));
        assert!(default_gateway_port(&work) >= ONECLI_BASE_PORT);
    }

    #[test]
    fn parses_camel_case_container_config() {
        let body = br#"{
            "env": {"HTTPS_PROXY": "http://host.docker.internal:10255", "NODE_EXTRA_CA_CERTS": "/etc/ssl/certs/onecli-ca.pem"},
            "caCertificate": "-----BEGIN CERTIFICATE-----\nx\n-----END CERTIFICATE-----",
            "caCertificateContainerPath": "/etc/ssl/certs/onecli-ca.pem"
        }"#;
        let config = parse_container_config(body).unwrap();
        assert_eq!(
            config.env.get("HTTPS_PROXY").map(String::as_str),
            Some("http://host.docker.internal:10255")
        );
        assert_eq!(
            config.ca_certificate_container_path.as_deref(),
            Some("/etc/ssl/certs/onecli-ca.pem")
        );
        assert!(config.ca_certificate.is_some());
    }

    #[test]
    fn apply_to_spec_injects_env_and_mounts_ca_readonly() {
        let tmp = tempfile::tempdir().unwrap();
        let mut spec = empty_spec();
        let mut env = HashMap::new();
        env.insert("HTTPS_PROXY".to_string(), "http://h:10255".to_string());
        env.insert("HTTP_PROXY".to_string(), "http://h:10255".to_string());
        let config = ContainerConfig {
            env,
            ca_certificate: Some("-----BEGIN CERTIFICATE-----\nx\n-----END CERTIFICATE-----".to_string()),
            ca_certificate_container_path: Some("/etc/ssl/certs/onecli-ca.pem".to_string()),
        };

        apply_to_spec(&mut spec, &config, tmp.path()).unwrap();

        assert!(spec.env.iter().any(|(k, _)| k == "HTTPS_PROXY"));
        assert!(spec.env.iter().any(|(k, _)| k == "HTTP_PROXY"));
        let ca_mount = spec
            .mounts
            .iter()
            .find(|m| m.container_path == Path::new("/etc/ssl/certs/onecli-ca.pem"))
            .expect("CA mount present");
        assert_eq!(ca_mount.mode, MountMode::ReadOnly);
        assert!(tmp.path().join("ca.pem").exists());
    }

    #[test]
    fn apply_to_spec_without_ca_only_injects_env() {
        let tmp = tempfile::tempdir().unwrap();
        let mut spec = empty_spec();
        let mut env = HashMap::new();
        env.insert("HTTPS_PROXY".to_string(), "http://h:10255".to_string());
        let config = ContainerConfig {
            env,
            ca_certificate: None,
            ca_certificate_container_path: None,
        };
        apply_to_spec(&mut spec, &config, tmp.path()).unwrap();
        assert!(spec.env.iter().any(|(k, _)| k == "HTTPS_PROXY"));
        assert!(spec.mounts.is_empty());
        assert!(!tmp.path().join("ca.pem").exists());
    }

    #[test]
    fn rejects_url_unsafe_agent() {
        let err = fetch_container_config("http://127.0.0.1:10254", "bad agent/../x").unwrap_err();
        assert!(matches!(err, OneCliError::InvalidAgent(_)));
    }

    #[test]
    fn splice_rebinds_agent_token_onto_host_endpoint() {
        let gateway = "http://x:aoc_tok@host.docker.internal:10355";
        let got = splice_proxy_userinfo(gateway, "http://127.0.0.1:10355").unwrap();
        assert_eq!(got, "http://x:aoc_tok@127.0.0.1:10355");
    }

    #[test]
    fn splice_drops_any_userinfo_already_on_host_endpoint() {
        let gateway = "http://x:aoc_tok@host.docker.internal:10355";
        let got = splice_proxy_userinfo(gateway, "http://stale:creds@127.0.0.1:10355").unwrap();
        assert_eq!(got, "http://x:aoc_tok@127.0.0.1:10355");
    }

    #[test]
    fn splice_without_gateway_token_yields_bare_host_endpoint() {
        let got = splice_proxy_userinfo("http://host.docker.internal:10355", "http://127.0.0.1:10355")
            .unwrap();
        assert_eq!(got, "http://127.0.0.1:10355");
    }

    #[test]
    fn splice_rejects_non_absolute_urls() {
        assert!(matches!(
            splice_proxy_userinfo("127.0.0.1:10355", "http://127.0.0.1:10355"),
            Err(OneCliError::Parse(_))
        ));
        assert!(matches!(
            splice_proxy_userinfo("http://x:t@h:1", "127.0.0.1:10355"),
            Err(OneCliError::Parse(_))
        ));
    }

    #[test]
    fn provisioning_persists_ca_and_injection_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = InstanceLayout::derive(tmp.path(), "assistant", Some("work")).unwrap();
        std::fs::create_dir_all(layout.setup_dir()).unwrap();
        let paths = OneCliPaths::for_instance(&layout);

        let mut env = HashMap::new();
        env.insert("HTTPS_PROXY".to_string(), "http://h:10255".to_string());
        let config = ContainerConfig {
            env,
            ca_certificate: Some("-----BEGIN CERTIFICATE-----\nx\n-----END CERTIFICATE-----".to_string()),
            ca_certificate_container_path: Some("/etc/ssl/certs/onecli-ca.pem".to_string()),
        };

        persist_provisioned(&layout, &config).unwrap();
        assert!(paths.ca_cert.exists(), "CA persisted");
        assert!(paths.injection_ok_marker.exists(), "injection marker written");
    }

    #[test]
    fn provisioning_without_ca_writes_no_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = InstanceLayout::derive(tmp.path(), "cleoclaw", None).unwrap();
        std::fs::create_dir_all(layout.setup_dir()).unwrap();
        let paths = OneCliPaths::for_instance(&layout);

        let mut env = HashMap::new();
        env.insert("HTTPS_PROXY".to_string(), "http://h:10255".to_string());
        let config = ContainerConfig {
            env,
            ca_certificate: None,
            ca_certificate_container_path: None,
        };

        persist_provisioned(&layout, &config).unwrap();
        assert!(!paths.ca_cert.exists(), "no CA without one supplied");
        assert!(!paths.injection_ok_marker.exists(), "no marker without a CA");
    }
}
