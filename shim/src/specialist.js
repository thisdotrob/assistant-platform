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

import { query, createSdkMcpServer } from '@anthropic-ai/claude-agent-sdk';

import { buildAssistantTools } from './assistant-tools.js';

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

// Parse a JSON array of `{ name, description }` destinations from an env var,
// tolerating absence/malformed values by returning an empty list (the agent then
// gets no `send_message` tool).
function jsonDestinations(raw) {
  if (!raw) return [];
  try {
    const parsed = JSON.parse(raw);
    if (Array.isArray(parsed)) {
      return parsed.filter(
        (d) => d && typeof d.name === 'string' && typeof d.description === 'string',
      );
    }
  } catch {
    // fall through to empty
  }
  return [];
}

// Derive the turn's options from the host-supplied environment. Pure and
// SDK-free so it is unit-testable without spawning a Claude turn.
//
// `mcpTools` names the assistant MCP tools this specialist may use
// (schedule_message, send_message, …) from ASSISTANT_SPECIALIST_MCP_TOOLS, and
// `destinations` is the `send_message` recipient menu from
// ASSISTANT_SPECIALIST_DESTINATIONS. Both default empty, so a specialist that
// declares neither behaves exactly as before (built-in tools only, no
// scheduling, no messaging).
export function specialistOptionsFromEnv(env) {
  const systemPrompt = env.ASSISTANT_SPECIALIST_SYSTEM_PROMPT ?? '';
  const tools = jsonStringArray(env.ASSISTANT_SPECIALIST_TOOLS, []);
  const allowedTools = jsonStringArray(env.ASSISTANT_SPECIALIST_ALLOWED_TOOLS, []);
  const mcpTools = jsonStringArray(env.ASSISTANT_SPECIALIST_MCP_TOOLS, []);
  const destinations = jsonDestinations(env.ASSISTANT_SPECIALIST_DESTINATIONS);
  const parsedMaxTurns = Number.parseInt(env.ASSISTANT_SPECIALIST_MAX_TURNS ?? '', 10);
  const maxTurns = Number.isInteger(parsedMaxTurns) && parsedMaxTurns > 0
    ? parsedMaxTurns
    : DEFAULT_MAX_TURNS;
  return { systemPrompt, tools, allowedTools, mcpTools, destinations, maxTurns };
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

// Format prior turns as a plain-text transcript for the prompt (mirrors the
// orchestrator path): user turns labelled "User:", assistant turns "You:".
function formatHistory(messages) {
  if (!messages || messages.length === 0) return '';
  return messages
    .map(({ role, content }) => `${role === 'assistant' ? 'You' : 'User'}: ${content}`)
    .join('\n\n');
}

// Run one specialist turn. `goal` is the inbound content; `memory` is the host's
// retrieved-memories block (or null); `messages` is the prior conversation
// history. Returns `{ text, scheduled, cancellations, pauses, resumes, memories,
// messages }` — the same shape the orchestrator responder returns, so a
// schedule-capable specialist's requests flow through the runner unchanged. A
// specialist that declares no assistant MCP tools gets empty side-effect arrays.
export async function runSpecialistTurn(goal, memory, history) {
  const { systemPrompt, tools, allowedTools, mcpTools, destinations, maxTurns } =
    specialistOptionsFromEnv(process.env);
  const fileServers = mcpServersFromConfig(process.env);

  // Assistant MCP tools (scheduling / send_message) the spec opted into, plus
  // the buffers their handlers record into for the runner to serialize.
  const { tools: assistantTools, allowedToolNames, buffers } = buildAssistantTools({
    enabled: mcpTools,
    destinations,
  });
  const mcpServers = { ...fileServers };
  if (assistantTools.length > 0) {
    mcpServers.assistant = createSdkMcpServer({
      name: 'assistant',
      version: '0.1.0',
      tools: assistantTools,
    });
  }

  // Build the prompt like the orchestrator: optional memory preamble, then the
  // prior transcript, then the current message.
  const transcript = formatHistory(history);
  const parts = [];
  if (memory && memory.length > 0) parts.push(memory);
  if (transcript.length > 0) {
    parts.push(
      `Here is the conversation so far, for context:\n\n${transcript}`,
      `The new message is:\n\n${goal}`,
    );
  } else {
    parts.push(goal);
  }
  const prompt = parts.join('\n\n');

  const q = query({
    prompt,
    options: {
      systemPrompt,
      // MCP servers from the image's mcp.json (credentials injected by the
      // OneCLI proxy, not carried here) merged with the in-process `assistant`
      // server carrying the scheduling / send_message tools the spec enabled.
      mcpServers,
      // Enable only the host-declared built-in tools and auto-approve the
      // host-declared patterns plus the enabled assistant tools. With dontAsk the
      // turn never hangs on a permission prompt; anything else is denied.
      tools,
      allowedTools: [...allowedTools, ...allowedToolNames],
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
  return {
    text: segments.join('\n\n'),
    scheduled: buffers.scheduled,
    cancellations: buffers.cancellations,
    pauses: buffers.pauses,
    resumes: buffers.resumes,
    memories: buffers.memories,
    messages: buffers.messages,
  };
}
