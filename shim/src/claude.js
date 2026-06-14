// Claude path for `ASSISTANT_RUNNER_MODE=claude_oauth`: run one Claude turn via the
// Agent SDK and return its final text plus any schedules it requested.
//
// Auth is handled entirely out-of-process: the container holds only
// `CLAUDE_CODE_OAUTH_TOKEN=placeholder` and routes outbound api.anthropic.com
// traffic through the OneCLI forward proxy (`HTTPS_PROXY`) that swaps the
// placeholder for the real token. The SDK/CLI read both from the environment,
// so we pass `process.env` through and never touch the credential here.
//
// The tools exposed are in-process SDK MCP tools (`schedule_message`,
// `save_memory`): each handler records the request rather than emitting, so the
// runner can write the side-effect actions and the reply text as one atomic
// outbound batch after the turn (see index.js) — a host poll never observes a
// partial turn. The host intercepts those rows (records a scheduled item /
// writes a memory note) and does not post them to the channel (the run's text is
// the user-facing confirmation).
//
// NOTE: the SDK package/version and the exact message-stream shape are confirmed
// during the live smoke; this path is never exercised by the offline gate.

import { query, tool, createSdkMcpServer } from '@anthropic-ai/claude-agent-sdk';
import { z } from 'zod';

// Parse the host-supplied specialist menu from ASSISTANT_SPECIALISTS: a JSON array of
// `{ name, description }` entries (the projection of the registered
// `SpecialistSpec`s). Tolerates absence and malformed values by returning an
// empty list — with no specialists the orchestrator simply omits the `delegate`
// tool. Pure and SDK-free so it is unit-testable. Entries missing a string name
// or description are dropped.
export function specialistsFromEnv(env) {
  const raw = env.ASSISTANT_SPECIALISTS;
  if (!raw) return [];
  let parsed;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return [];
  }
  if (!Array.isArray(parsed)) return [];
  return parsed.filter(
    (e) => e && typeof e.name === 'string' && typeof e.description === 'string',
  );
}

// Persona for the orchestrator turn, built from the available specialists.
// Without a persona the SDK has no identity for the agent and — combined with
// the default built-in toolset — the model describes itself with capabilities it
// does not have (Bash, file edits, web browsing). Pairing this with `tools: []`
// (below) keeps the model's self-description aligned with what it can actually
// do: converse, the two MCP tools, plus delegating to any registered specialist.
// (Organization-level instructions tied to the authenticated account are layered
// in by the harness regardless of this.)
//
// The delegate capability is data-driven: when specialists are registered, the
// prompt lists each by name + description and frames delegation as asynchronous
// work the bot presents as its own; when none are, it omits delegation entirely
// and points the user at setting one up for capabilities the bot lacks.
export function buildSystemPrompt(specialists) {
  const hasSpecialists = specialists.length > 0;
  const lines = [
    'You are a helpful assistant operating inside a Slack workspace. You reply to people in the channel or thread where they message you.',
    '',
    'Your own tools are:',
    '- schedule_message: set a one-off reminder or a recurring check-in, processed as a fresh turn when it fires.',
    '- save_memory: remember a durable fact, preference, or piece of context to recall in future turns.',
  ];
  if (hasSpecialists) {
    lines.push(
      '- delegate: hand a task to a specialist that can do work you cannot do yourself. The work runs separately and its result comes back to you as a follow-up.',
    );
  }
  lines.push(
    '',
    'Beyond those tools you converse: answer questions, summarise, and help think things through using what the user tells you and what you already know.',
    '',
  );
  if (hasSpecialists) {
    const menu = specialists.map((s) => `- ${s.name}: ${s.description}`).join('\n');
    lines.push(
      'Specialists you can delegate to:',
      menu,
      '',
      'Delegation is asynchronous: when you call delegate, the work runs separately and its result comes back to you as a fresh follow-up turn — you do not get the answer within the same reply. So when you start one, briefly tell the user you are looking into it, then share what you found when the result arrives. Present delegated work as your own: never say "delegate", "specialist", or "sub-agent" to the user — describe what you are doing in plain terms (e.g. that you are looking something up).',
      '',
    );
  }
  const uncovered = hasSpecialists
    ? 'Some requests need a capability none of your specialists cover — reading or editing files, running code or shell commands, or querying other external systems or APIs.'
    : 'You cannot directly read or edit files, run code or shell commands, or query other external systems or APIs.';
  lines.push(
    `${uncovered} When a request needs a capability like that, do not offer to do it yourself and do not ask the user to paste in the content for you. Instead, explain that this kind of work is handled by a specialist sub-agent that has not been set up yet, and that one can be added for that task so you can delegate to it.`,
    '',
    `When asked what you can do, describe your abilities honestly — conversation, reminders, durable memory${hasSpecialists ? ', and the specialists you can delegate to' : ''} — and do not claim capabilities you do not have.`,
    '',
    'Replies are delivered to Slack. Keep them concise; standard Markdown (bold, bullets, links, headings, code) is fine and is converted to Slack formatting for you. Do not use horizontal rules (lines of ---).',
  );
  return lines.join('\n');
}

// `memory` is the host's `<retrieved_memories>` block (or null/empty). When
// present it is prepended as a context preamble ahead of the user's message,
// mirroring the v1 pre-reply RAG layout (stored context first, then the turn).
//
// Returns `{ text, scheduled, memories, delegations }`: the assistant's final
// text, a list of `{ text, after_seconds, every_seconds? }` schedule requests, a
// list of `{ content, title? }` memory-save requests, and a list of
// `{ specialist, goal, facts?, constraints? }` delegation requests, all collected
// from tool calls during the turn.
export async function runClaudeTurn(userText, memory) {
  const prompt = memory && memory.length > 0 ? `${memory}\n\n${userText}` : userText;

  const specialists = specialistsFromEnv(process.env);
  const hasSpecialists = specialists.length > 0;

  const scheduled = [];
  const memories = [];
  const delegations = [];

  const tools = [
    tool(
      'schedule_message',
      'Schedule a message to be processed in this channel later — use for reminders or recurring check-ins. The scheduled text is processed as a fresh turn when it fires.',
      {
        text: z.string().describe('The message/instruction to process when the schedule fires.'),
        after_seconds: z
          .number()
          .int()
          .describe('Seconds from now until the first (or only) firing.'),
        every_seconds: z
          .number()
          .int()
          .optional()
          .describe('Optional fixed recurrence interval in seconds; omit for a one-time reminder.'),
      },
      async (args) => {
        const entry = { text: args.text, after_seconds: args.after_seconds };
        if (args.every_seconds != null) entry.every_seconds = args.every_seconds;
        scheduled.push(entry);
        const when =
          args.every_seconds != null
            ? `every ${args.every_seconds}s, starting in ${args.after_seconds}s`
            : `in ${args.after_seconds}s`;
        return { content: [{ type: 'text', text: `Scheduled "${args.text}" ${when}.` }] };
      },
    ),
    tool(
      'save_memory',
      'Remember a durable fact, preference, or piece of context for future turns — use when the user states something worth recalling later. The note is stored and may be surfaced as context in later turns across this agent.',
      {
        content: z.string().describe('The fact or context to remember, in your own words.'),
        title: z
          .string()
          .optional()
          .describe('Optional short human-readable label for the memory.'),
      },
      async (args) => {
        const entry = { content: args.content };
        if (args.title != null) entry.title = args.title;
        memories.push(entry);
        return { content: [{ type: 'text', text: `Saved a memory: "${args.content}".` }] };
      },
    ),
  ];

  // The `delegate` tool only exists when specialists are registered. Its
  // `specialist` enum and description are built from the host-supplied menu, so a
  // new specialist becomes routable with no shim change. With no specialists the
  // tool is omitted entirely — the prompt then steers the bot to explain one can
  // be set up rather than offering to do the work itself.
  if (hasSpecialists) {
    const roster = specialists.map((s) => `"${s.name}" — ${s.description}`).join('; ');
    tools.push(
      tool(
        'delegate',
        `Hand a task to a specialist that can do work you cannot do yourself. The work runs separately and its result returns as a fresh follow-up turn, not within this reply, so briefly acknowledge (as your own work, without mentioning delegation or a specialist) and share what you found when it arrives. Available specialists: ${roster}.`,
        {
          specialist: z
            .enum(specialists.map((s) => s.name))
            .describe(`Which specialist to delegate to. Options: ${roster}.`),
          goal: z
            .string()
            .describe('What the specialist should accomplish, in a clear self-contained instruction.'),
          facts: z
            .array(z.string())
            .optional()
            .describe('Relevant context the specialist needs (e.g. URLs, prior findings).'),
          constraints: z
            .array(z.string())
            .optional()
            .describe('Optional guardrails or limits the specialist must respect.'),
        },
        async (args) => {
          const entry = { specialist: args.specialist, goal: args.goal };
          if (args.facts != null) entry.facts = args.facts;
          if (args.constraints != null) entry.constraints = args.constraints;
          delegations.push(entry);
          return {
            content: [
              {
                type: 'text',
                text: `Started working on: "${args.goal}". The result will arrive as a follow-up.`,
              },
            ],
          };
        },
      ),
    );
  }

  const scheduler = createSdkMcpServer({ name: 'assistant', version: '0.1.0', tools });

  const allowedTools = ['mcp__assistant__schedule_message', 'mcp__assistant__save_memory'];
  if (hasSpecialists) allowedTools.push('mcp__assistant__delegate');

  const q = query({
    prompt,
    options: {
      systemPrompt: buildSystemPrompt(specialists),
      mcpServers: { assistant: scheduler },
      // Disable every built-in Claude Code tool (Bash/Read/Edit/WebSearch/…) so
      // the only tools in context are our MCP tools — the model can't claim or
      // attempt abilities it doesn't have. `tools` restricts the available set;
      // `allowedTools` only auto-approves without prompting.
      tools: [],
      // Auto-allow our tools; deny anything else without prompting (headless),
      // so the turn never hangs on a permission request or touches the filesystem.
      allowedTools,
      permissionMode: 'dontAsk',
      env: { ...process.env },
    },
  });

  // The model can emit text in more than one assistant message per turn — e.g.
  // a line before a tool call and another after the tool result. Each message's
  // blocks are one contiguous utterance, but separate messages are distinct, so
  // join messages with a paragraph break (concatenating them directly would run
  // sentences together, dropping the space at the seam).
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
  return { text: segments.join('\n\n'), scheduled, memories, delegations };
}
