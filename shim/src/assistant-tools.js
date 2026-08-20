// Shared in-process SDK-MCP tools for every agent turn (orchestrator and
// specialist alike). Each tool's handler records the request into a buffer
// rather than acting, so the runner (index.js) can emit the side-effect rows and
// the reply text as one atomic outbound batch after the turn. The host
// intercepts those rows and performs the side effect; the text row is the only
// user-visible output.
//
// Which tools an agent gets is data-driven: `buildAssistantTools({ enabled,
// destinations })` includes only the tools named in `enabled`, so a leaf
// specialist can be given, say, just `schedule_message`, while the orchestrator
// gets the full set. This is the single source of truth for the tool schemas so
// the orchestrator and specialist paths never drift.

import { tool } from '@anthropic-ai/claude-agent-sdk';
import { z } from 'zod';

// A short human phrase for a schedule request, used only in the tool's
// confirmation text (the host owns the real timing).
export function describeSchedule(args) {
  const cal = args.calendar;
  if (cal != null) {
    if (cal.kind === 'weekly') {
      const days = (cal.days ?? []).join(', ');
      return `weekly on ${days} at ${cal.at} ${cal.tz}`;
    }
    if (cal.kind === 'monthly') {
      return `monthly on day ${cal.day} at ${cal.at} ${cal.tz}`;
    }
    return `daily at ${cal.at} ${cal.tz}`;
  }
  const start = args.after_seconds ?? 0;
  return args.every_seconds != null
    ? `every ${args.every_seconds}s, starting in ${start}s`
    : `in ${start}s`;
}

// The complete set of assistant tool names. `buildAssistantTools` includes the
// subset named in its `enabled` set (order here is the emitted order).
export const ALL_ASSISTANT_TOOLS = [
  'schedule_message',
  'cancel_schedule',
  'pause_schedule',
  'resume_schedule',
  'save_memory',
  'send_message',
];

// Build the assistant MCP tool array for a turn plus the buffers their handlers
// record into. `enabled` is an iterable of tool names to include; `destinations`
// is an array of `{ name, description }` the `send_message` tool may target (its
// `to` becomes an enum over these names). Returns `{ tools, allowedToolNames,
// buffers }`: `tools` for `createSdkMcpServer`, `allowedToolNames` for the
// query's `allowedTools`, and `buffers` (arrays the runner serializes to
// outbound rows). Unknown names in `enabled` are ignored.
export function buildAssistantTools({ enabled, destinations = [], freeformTo = false } = {}) {
  const want = enabled instanceof Set ? enabled : new Set(enabled ?? []);
  const buffers = {
    scheduled: [],
    cancellations: [],
    pauses: [],
    resumes: [],
    memories: [],
    messages: [],
  };
  const tools = [];

  if (want.has('schedule_message')) {
    tools.push(
      tool(
        'schedule_message',
        'Schedule a message to be processed later — use for reminders, recurring check-ins, or to queue your own follow-up work. The scheduled text is processed as a fresh turn when it fires. Choose exactly one timing form: after_seconds (a one-off, or with every_seconds a fixed interval), or calendar (a recurring local wall-clock time such as every weekday at 9am).',
        {
          text: z.string().describe('The message/instruction to process when the schedule fires.'),
          after_seconds: z
            .number()
            .int()
            .optional()
            .describe(
              'Seconds from now until the first (or only) firing. Use for a one-off reminder, or with every_seconds for a fixed interval. Omit when using calendar.',
            ),
          every_seconds: z
            .number()
            .int()
            .optional()
            .describe(
              'Optional fixed recurrence interval in seconds, paired with after_seconds; omit for a one-time reminder or when using calendar.',
            ),
          calendar: z
            .object({
              kind: z
                .enum(['daily', 'weekly', 'monthly'])
                .describe('Recurrence shape: daily, weekly (on given weekdays), or monthly.'),
              at: z.string().describe('Local wall-clock time as "HH:MM" (24-hour), e.g. "09:00".'),
              tz: z
                .string()
                .describe('IANA timezone name for the local time, e.g. "Europe/London". Ask the user if unknown.'),
              days: z
                .array(z.enum(['mon', 'tue', 'wed', 'thu', 'fri', 'sat', 'sun']))
                .optional()
                .describe('For kind=weekly: the weekdays it fires on.'),
              day: z
                .number()
                .int()
                .optional()
                .describe('For kind=monthly: day of the month 1-31 (clamped to the month length).'),
            })
            .optional()
            .describe(
              'Calendar-style recurrence at a local wall-clock time (DST-safe). Provide this instead of after_seconds/every_seconds. Always include the user\'s timezone.',
            ),
        },
        async (args) => {
          const entry = { text: args.text };
          if (args.after_seconds != null) entry.after_seconds = args.after_seconds;
          if (args.every_seconds != null) entry.every_seconds = args.every_seconds;
          if (args.calendar != null) entry.calendar = args.calendar;
          buffers.scheduled.push(entry);
          return {
            content: [{ type: 'text', text: `Scheduled "${args.text}" ${describeSchedule(args)}.` }],
          };
        },
      ),
    );
  }

  if (want.has('cancel_schedule')) {
    tools.push(
      tool(
        'cancel_schedule',
        'Cancel one of your existing scheduled items so it stops firing for good. Pass the id from the <schedules> block; cancelling an unknown or already-finished item is a harmless no-op.',
        {
          scheduled_item_id: z
            .string()
            .describe('The id of the scheduled item to cancel, taken from the <schedules> block.'),
        },
        async (args) => {
          buffers.cancellations.push({ scheduled_item_id: args.scheduled_item_id });
          return {
            content: [{ type: 'text', text: `Cancelled scheduled item ${args.scheduled_item_id}.` }],
          };
        },
      ),
    );
  }

  if (want.has('pause_schedule')) {
    tools.push(
      tool(
        'pause_schedule',
        'Temporarily suspend one of your active scheduled items so it stops firing until you resume it. Pass the id from the <schedules> block; pausing an unknown or non-active item is a harmless no-op.',
        {
          scheduled_item_id: z
            .string()
            .describe('The id of the scheduled item to pause, taken from the <schedules> block.'),
        },
        async (args) => {
          buffers.pauses.push({ scheduled_item_id: args.scheduled_item_id });
          return {
            content: [{ type: 'text', text: `Paused scheduled item ${args.scheduled_item_id}.` }],
          };
        },
      ),
    );
  }

  if (want.has('resume_schedule')) {
    tools.push(
      tool(
        'resume_schedule',
        'Resume one of your paused scheduled items so it fires again. Pass the id from the <schedules> block (look for the "paused" marker); resuming an unknown or non-paused item is a harmless no-op.',
        {
          scheduled_item_id: z
            .string()
            .describe('The id of the paused scheduled item to resume, taken from the <schedules> block.'),
        },
        async (args) => {
          buffers.resumes.push({ scheduled_item_id: args.scheduled_item_id });
          return {
            content: [{ type: 'text', text: `Resumed scheduled item ${args.scheduled_item_id}.` }],
          };
        },
      ),
    );
  }

  if (want.has('save_memory')) {
    tools.push(
      tool(
        'save_memory',
        'Remember a durable fact, preference, or piece of context for future turns — use when something worth recalling later comes up. The note is stored and may be surfaced as context in later turns across this agent.',
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
          buffers.memories.push(entry);
          return { content: [{ type: 'text', text: `Saved a memory: "${args.content}".` }] };
        },
      ),
    );
  }

  // `send_message` is included when enabled and either the agent has a fixed
  // destination menu (specialist) or it is the Slack-wired orchestrator with
  // free-form addressing (`freeformTo`), which can also target a channel id.
  if (want.has('send_message') && (freeformTo || destinations.length > 0)) {
    const roster = destinations.length
      ? destinations.map((d) => `"${d.name}" — ${d.description}`).join('; ')
      : '(no agents registered)';
    const description = freeformTo
      ? `Send a message: to a Slack channel id (the human sees it there) or to another agent by name. You decide whether to post to Slack at all and, if so, which channel. Known agents: ${roster}.`
      : `Send a message to another agent — the recipient receives it as a fresh turn and may reply back as a follow-up. Use it to hand off work or report a result. Available recipients: ${roster}.`;
    const toSchema = freeformTo
      ? z
          .string()
          .describe(`Recipient: a Slack channel id to post to, or an agent name. Known agents: ${roster}.`)
      : z.enum(destinations.map((d) => d.name)).describe(`Which recipient to send to. Options: ${roster}.`);
    tools.push(
      tool(
        'send_message',
        description,
        {
          to: toSchema,
          text: z
            .string()
            .describe('The message to send — a clear, self-contained instruction or report.'),
        },
        async (args) => {
          buffers.messages.push({ to: args.to, text: args.text });
          return {
            content: [{ type: 'text', text: `Sent a message to ${args.to}.` }],
          };
        },
      ),
    );
  }

  // Derive allowed-tool names from the enabled set (not from the `tool()`
  // objects, whose shape is not a public SDK contract), applying the same
  // send_message-needs-destinations condition used above.
  const included = ALL_ASSISTANT_TOOLS.filter(
    (name) => want.has(name) && (name !== 'send_message' || freeformTo || destinations.length > 0),
  );
  const allowedToolNames = included.map((name) => `mcp__assistant__${name}`);
  return { tools, allowedToolNames, buffers };
}
