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
// memories }`) so the runner loop emits it with no special-casing. A specialist
// requests no schedules and saves no memories.

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

// Run one specialist turn over `goal`. Returns `{ text, scheduled, memories }`
// like the orchestrator responder; a specialist never schedules or saves memory,
// so those are always empty.
export async function runSpecialistTurn(goal) {
  const { systemPrompt, tools, allowedTools, maxTurns } = specialistOptionsFromEnv(process.env);

  const q = query({
    prompt: goal,
    options: {
      systemPrompt,
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
  return { text: segments.join('\n\n'), scheduled: [], memories: [] };
}
