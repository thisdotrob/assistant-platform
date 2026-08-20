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
// `cancel_schedule`, `pause_schedule`, `resume_schedule`, `save_memory`): each
// handler records the request rather than emitting, so the runner can write the
// side-effect actions and the reply text as one atomic outbound batch after the
// turn (see index.js) — a host poll never observes a partial turn. The host
// intercepts those rows (records a scheduled item / writes a memory note) and
// does not post them to the channel (the run's text is the user-facing
// confirmation).
//
// NOTE: the SDK package/version and the exact message-stream shape are confirmed
// during the live smoke; this path is never exercised by the offline gate.

import { query, tool, createSdkMcpServer } from '@anthropic-ai/claude-agent-sdk';
import { z } from 'zod';

import { buildAssistantTools } from './assistant-tools.js';

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
    '- schedule_message: set a one-off reminder, a fixed-interval check-in, or a calendar recurrence at a local time (e.g. every weekday at 9am), processed as a fresh turn when it fires.',
    '- cancel_schedule: cancel one of your existing scheduled items so it stops firing for good.',
    '- pause_schedule: temporarily suspend one of your scheduled items so it stops firing until you resume it.',
    '- resume_schedule: resume a paused scheduled item so it fires again.',
    '- save_memory: remember a durable fact, preference, or piece of context to recall in future turns.',
    '- send_message: post to a Slack channel (by channel id) or message another agent by name. You are the only agent that can post to Slack, so other agents send their updates to you and you decide whether to surface anything and where.',
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
    'Sometimes a message comes from another agent (the sender is "agent:...") reporting what it did or found — not from a human. When that happens, decide whether the humans need to know: if so, use send_message to post a concise, plain-language update to the right Slack channel (the report should tell you which — e.g. a channel id); if it is routine or needs no action, do nothing. Never post an agent\'s raw report verbatim; relay only what matters, in your own words.',
    '',
    'For a recurrence at a clock time (e.g. "every weekday at 9am", "the 1st of each month"), use schedule_message with the calendar option and a local time — not a raw interval. Calendar recurrence needs the user\'s timezone as an IANA name (e.g. "Europe/London"): use one they have given you, otherwise ask before scheduling. Use after_seconds (optionally with every_seconds) only for one-off reminders or plain fixed intervals.',
    '',
    'When you have scheduled items, the latest list is injected each turn as a <schedules> block, each line carrying the item id (and a "paused" marker for any you have suspended). Use those ids to answer "what reminders do I have?" and to manage them: pass the matching id to cancel_schedule to stop one for good, pause_schedule to suspend an active one, or resume_schedule to restart a paused one. Never invent an id — only act on one that appears in that block.',
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
    `When asked what you can do, describe your abilities honestly — conversation, reminders you can set, pause, resume, and cancel, durable memory${hasSpecialists ? ', and the specialists you can delegate to' : ''} — and do not claim capabilities you do not have.`,
    '',
    'Replies are delivered to Slack. Keep them concise; standard Markdown (bold, bullets, links, headings, code) is fine and is converted to Slack formatting for you. Do not use horizontal rules (lines of ---).',
  );
  return lines.join('\n');
}

// `memory` is the host's `<retrieved_memories>` block (or null/empty). When
// present it is prepended as a context preamble ahead of the user's message,
// mirroring the v1 pre-reply RAG layout (stored context first, then the turn).
//
// `messages` is the prior conversation history as an Anthropic messages array
// `[{role, content}]`, built from the session DBs by `Session.buildHistory()`.
// The Agent SDK's `query()` takes only `prompt` + `options` — it has no
// messages/history input short of full session resumption — so we deliver the
// history by formatting it into the prompt as a labelled transcript ahead of the
// current message (same approach as the `memory` preamble). When absent (first
// turn or history unavailable) the turn runs without prior context.
//
// Returns `{ text, scheduled, cancellations, pauses, resumes, memories,
// delegations }`: the assistant's final text, a list of
// `{ text, after_seconds?, every_seconds?, calendar? }` schedule requests, lists of
// `{ scheduled_item_id }` cancellation / pause / resume requests, a list of
// `{ content, title? }` memory-save requests, and a list of
// `{ specialist, goal, facts?, constraints? }` delegation requests, all collected
// from tool calls during the turn.
// Format prior turns as a plain-text transcript for the prompt. `messages` is
// `[{role, content}]` from buildHistory(); user turns are labelled "User:" and
// assistant turns "You:" (the model is the assistant). Returns '' for empty
// history so the caller can omit the block entirely.
export function formatHistory(messages) {
  if (!messages || messages.length === 0) return '';
  const lines = messages.map(({ role, content }) => {
    const label = role === 'assistant' ? 'You' : 'User';
    return `${label}: ${content}`;
  });
  return lines.join('\n\n');
}

export async function runClaudeTurn(userText, memory, messages) {
  const transcript = formatHistory(messages);
  const parts = [];
  if (memory && memory.length > 0) parts.push(memory);
  if (transcript.length > 0) {
    parts.push(
      `Here is the conversation so far, for context:\n\n${transcript}`,
      `The user's new message is:\n\n${userText}`,
    );
  } else {
    parts.push(userText);
  }
  const prompt = parts.join('\n\n');

  const specialists = specialistsFromEnv(process.env);
  const hasSpecialists = specialists.length > 0;

  // The orchestrator's own tools (scheduling + memory + send_message) come from
  // the shared builder; `delegate` is added inline below when specialists are
  // registered. As the Slack-wired agent it gets free-form `send_message`
  // addressing so it can post to any channel id it chooses, or message an agent
  // by name — its knowledge of registered specialists seeds the recipient hints.
  const { tools, allowedToolNames, buffers } = buildAssistantTools({
    enabled: [
      'schedule_message',
      'cancel_schedule',
      'pause_schedule',
      'resume_schedule',
      'save_memory',
      'send_message',
    ],
    destinations: specialists.map((s) => ({ name: s.name, description: s.description })),
    freeformTo: true,
  });
  const { scheduled, cancellations, pauses, resumes, memories } = buffers;
  const outboundMessages = buffers.messages;
  const delegations = [];

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

  const allowedTools = [...allowedToolNames];
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
  return {
    text: segments.join('\n\n'),
    scheduled,
    cancellations,
    pauses,
    resumes,
    memories,
    delegations,
    messages: outboundMessages,
  };
}
