// Generic specialist turn for `ASSISTANT_RUNNER_MODE=specialist`.
//
// A specialist runs in its own job-keyed container, separate from the
// orchestrator, but is credentialed the same way (placeholder OAuth token routed
// through the OneCLI proxy) so it can run a real Claude turn. The host hands it a
// goal (the inbound message content, which already carries any delegated
// facts/URLs); this turn runs one Claude turn with a restricted toolset and
// returns its findings as text. The host collects that text exactly like an
// orchestrator reply and re-injects it as a follow-up orchestrator turn.
//
// This harness is specialist-agnostic: the persona, the enabled tools, the
// auto-approve patterns, and the step ceiling are all supplied by the host as
// environment variables, derived from the registered `SpecialistSpec`. A new
// specialist needs only its own image (carrying its binaries) and a spec — no
// new shim JS. The browser specialist, for example, ships a system prompt that
// drives `agent-browser` with `tools: ["Bash"]` and
// `allowedTools: ["Bash(agent-browser:*)"]`; the harness here never mentions it.
//
// Env contract (set by the host in `run_specialist_turn`):
//   - ASSISTANT_SPECIALIST_SYSTEM_PROMPT : the complete system prompt (guardrails
//                                     already folded in host-side).
//   - ASSISTANT_SPECIALIST_TOOLS         : JSON array of SDK built-in tools to enable
//                                     (e.g. ["Bash"]).
//   - ASSISTANT_SPECIALIST_ALLOWED_TOOLS : JSON array of auto-approve patterns
//                                     (e.g. ["Bash(agent-browser:*)"]).
//   - ASSISTANT_SPECIALIST_MAX_TURNS     : integer per-turn step ceiling.
//
// The result shape matches the orchestrator responder (`{ text, scheduled,
// cancellations, memories }`) so the runner loop emits it with no special-casing.
// A specialist requests no schedules, cancels nothing, and saves no memories.

import { readFileSync } from 'node:fs';

import { query } from '@anthropic-ai/claude-agent-sdk';

// Bound a multi-step turn so a stuck or looping specialist can't burn unbounded
// API calls; the host's turn timeout is the wall-clock backstop on top of this.
const DEFAULT_MAX_TURNS = 40;

// Parse a JSON array of strings from an env var, tolerating absence and malformed
// values by returning the fallback — the harness must never crash on a bad spec
// projection; an empty toolset is a safe (if inert) default.
function jsonStringArray(raw, fallback) {
  if (!raw) return fallback;
  try {
    const parsed = JSON.parse(raw);
    if (Array.isArray(parsed) && parsed.every((t) => typeof t === 'string')) return parsed;
  } catch {
    // fall through to fallback
  }
  return fallback;
}

// Derive the turn's options from the host-supplied environment. Pure and
// SDK-free so it is unit-testable without spawning a Claude turn.
export function specialistOptionsFromEnv(env) {
  const systemPrompt = env.ASSISTANT_SPECIALIST_SYSTEM_PROMPT ?? '';
  const tools = jsonStringArray(env.ASSISTANT_SPECIALIST_TOOLS, []);
  const allowedTools = jsonStringArray(env.ASSISTANT_SPECIALIST_ALLOWED_TOOLS, []);
  const parsedMaxTurns = Number.parseInt(env.ASSISTANT_SPECIALIST_MAX_TURNS ?? '', 10);
  const maxTurns = Number.isInteger(parsedMaxTurns) && parsedMaxTurns > 0
    ? parsedMaxTurns
    : DEFAULT_MAX_TURNS;
  return { systemPrompt, tools, allowedTools, maxTurns };
}

// Load the turn's MCP servers from the specialist image's Claude config
// (`$CLAUDE_CONFIG_DIR/mcp.json`, default `/etc/claude/mcp.json`, baked in by the
// image — see cleoclaw-specialist-ax/image `COPY mcp.json /etc/claude/mcp.json`).
// The Agent SDK is hermetic: it connects only servers passed in
// `options.mcpServers` and does NOT auto-load filesystem MCP config, so the
// harness must read the file and pass it through explicitly. Credentials are not
// in the file — outbound MCP traffic egresses via the OneCLI proxy (`HTTPS_PROXY`),
// which injects the real per-host token by pattern match; the file carries only a
// placeholder auth header the proxy overwrites. A missing or malformed file
// yields no servers (the browser specialist ships none), so the turn runs with
// just its built-in tools rather than failing.
export function mcpServersFromConfig(env) {
  const dir = env.CLAUDE_CONFIG_DIR ?? '/etc/claude';
  try {
    const parsed = JSON.parse(readFileSync(`${dir}/mcp.json`, 'utf8'));
    const servers = parsed?.mcpServers;
    if (servers && typeof servers === 'object') return servers;
  } catch {
    // no file / unreadable / bad JSON → no MCP servers
  }
  return {};
}

// Run one specialist turn over `goal`. Returns `{ text, scheduled, cancellations,
// memories }` like the orchestrator responder; a specialist never schedules,
// cancels, or saves memory, so those are always empty.
export async function runSpecialistTurn(goal) {
  const { systemPrompt, tools, allowedTools, maxTurns } = specialistOptionsFromEnv(process.env);
  const mcpServers = mcpServersFromConfig(process.env);

  const q = query({
    prompt: goal,
    options: {
      systemPrompt,
      // MCP servers from the image's mcp.json (credentials injected by the
      // OneCLI proxy, not carried here). The `mcp__<server>__*` patterns in
      // `allowedTools` (from the specialist spec) auto-approve their tools.
      mcpServers,
      // Enable only the host-declared built-in tools and auto-approve only the
      // host-declared patterns. With dontAsk the turn never hangs on a permission
      // prompt; anything outside the allowlist is denied rather than prompted.
      tools,
      allowedTools,
      permissionMode: 'dontAsk',
      maxTurns,
      env: { ...process.env },
    },
  });

  // Collect the model's text the same way the orchestrator path does: one turn
  // can emit several assistant messages (text before a tool call, text after the
  // result); join distinct messages with a paragraph break so the seam keeps a
  // space. Tool-use/tool-result output is not assistant text and is skipped.
  const segments = [];
  for await (const message of q) {
    if (message.type === 'assistant') {
      let segment = '';
      for (const block of message.message?.content ?? []) {
        if (block.type === 'text') segment += block.text;
      }
      if (segment.trim().length > 0) segments.push(segment.trim());
    }
  }
  return { text: segments.join('\n\n'), scheduled: [], cancellations: [], memories: [] };
}
