// agentmaster Cursor SDK adapter
// Exposes the `am` ponytail router as Cursor subagent tools:
//   am_ls, am_peek, am_assign, am_handoff, am_skill, am_doctor
//
// Ponytail: thin shell wrappers around the `am` CLI — no reimplementation,
// no state, no framework. The CLI stays the source of truth; this file just
// makes it callable from Cursor's agent runtime.
//
// Usage from a Cursor subagent (.cursor/agents/agentmaster.md):
//   import { createAgentmasterTools } from "@agentmaster/cursor-sdk";
//   const tools = createAgentmasterTools();
//   // Cursor's harness picks them up via the Agent tool.
//
// Or run as an MCP server (see mcp-server.ts) for non-Cursor hosts.

import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { setTimeout as sleep } from "node:timers/promises";

const execFileAsync = promisify(execFile);

export interface AmResult {
  stdout: string;
  stderr: string;
  exitCode: number;
}

async function runAm(args: string[], opts: { timeoutMs?: number } = {}): Promise<AmResult> {
  const timeoutMs = opts.timeoutMs ?? 60_000;
  try {
    const { stdout, stderr } = await execFileAsync("am", args, {
      timeout: timeoutMs,
      maxBuffer: 10 * 1024 * 1024,
    });
    return { stdout, stderr, exitCode: 0 };
  } catch (err: any) {
    return {
      stdout: err.stdout ?? "",
      stderr: err.stderr ?? err.message ?? "",
      exitCode: err.code ?? 1,
    };
  }
}

export interface AmLsArgs {
  json?: boolean;
}

export async function amLs(args: AmLsArgs = {}): Promise<string> {
  const flags = args.json ? ["ls", "--json"] : ["ls"];
  const r = await runAm(flags);
  return r.stdout || r.stderr;
}

export interface AmPeekArgs {
  ref: string;
  tail?: number;
}

export async function amPeek(args: AmPeekArgs): Promise<string> {
  const flags = ["peek", args.ref];
  if (args.tail) flags.push("--tail", String(args.tail));
  const r = await runAm(flags);
  return r.stdout || r.stderr;
}

export interface AmAssignArgs {
  runtime: string;
  task: string;
  name?: string;
}

export async function amAssign(args: AmAssignArgs): Promise<string> {
  const flags = ["assign", args.runtime, args.task];
  if (args.name) flags.push("--name", args.name);
  const r = await runAm(flags, { timeoutMs: 30_000 });
  return r.stdout || r.stderr;
}

export interface AmHandoffArgs {
  ref: string;
  message: string;
  waitSecs?: number;
}

export async function amHandoff(args: AmHandoffArgs): Promise<string> {
  const wait = args.waitSecs ?? 120;
  const r = await runAm(["handoff", args.ref, args.message, "--wait", String(wait)], {
    timeoutMs: (wait + 30) * 1000,
  });
  return r.stdout || r.stderr;
}

export interface AmSkillArgs {
  runtime: string;
}

export async function amSkill(args: AmSkillArgs): Promise<string> {
  const r = await runAm(["skill", args.runtime]);
  return r.stdout || r.stderr;
}

export async function amDoctor(args: { costs?: boolean; verbose?: boolean } = {}): Promise<string> {
  const flags = ["doctor"];
  if (args.costs) flags.push("--costs");
  if (args.verbose) flags.push("--verbose");
  const r = await runAm(flags);
  return r.stdout || r.stderr;
}

// Cursor SDK tool descriptors — the shape Cursor's Agent tool expects.
// Each tool is a thin wrapper that calls the `am` CLI and returns the output
// as a string. No token-costly processing; the CLI does the work.
export interface CursorTool {
  name: string;
  description: string;
  parameters: Record<string, any>;
  handler: (args: any) => Promise<string>;
}

export function createAgentmasterTools(): CursorTool[] {
  return [
    {
      name: "am_ls",
      description:
        "List every discoverable live agent (cmux workspaces + tmux panes) with status glyphs. Zero token cost — reads the fleet snapshot directly.",
      parameters: {
        type: "object",
        properties: {
          json: { type: "boolean", description: "emit JSON instead of glyph text" },
        },
      },
      handler: (a: AmLsArgs) => amLs(a),
    },
    {
      name: "am_peek",
      description:
        "Zero-tax peek: read an agent's last user/assistant/next action off its transcript. Never asks the agent to report — reads the JSONL directly.",
      parameters: {
        type: "object",
        properties: {
          ref: { type: "string", description: "agent ref: workspace:NN or session:window.pane" },
          tail: { type: "number", description: "print last N raw transcript events instead of digest" },
        },
        required: ["ref"],
      },
      handler: (a: AmPeekArgs) => amPeek(a),
    },
    {
      name: "am_assign",
      description:
        "Async fan-out: spawn a fresh cmux workspace seeded with the task and return immediately. Fire-and-forget. Logs the callback-ref so the supervisor can peek it later.",
      parameters: {
        type: "object",
        properties: {
          runtime: {
            type: "string",
            description: "claude, codex, hermes, ggcoder, aider, opencode, gemini, cline, shell",
          },
          task: { type: "string", description: "the task text" },
          name: { type: "string", description: "explicit workspace name (default: slug from task)" },
        },
        required: ["runtime", "task"],
      },
      handler: (a: AmAssignArgs) => amAssign(a),
    },
    {
      name: "am_handoff",
      description:
        "Synchronous handoff: send a message to a live agent and wait for its next assistant reply (polls transcript, zero token tax). Returns the reply or a 'still running' notice on timeout.",
      parameters: {
        type: "object",
        properties: {
          ref: { type: "string", description: "agent ref: workspace:NN or session:window.pane" },
          message: { type: "string", description: "the message to send" },
          waitSecs: { type: "number", description: "timeout in seconds (default 120)" },
        },
        required: ["ref", "message"],
      },
      handler: (a: AmHandoffArgs) => amHandoff(a),
    },
    {
      name: "am_skill",
      description:
        "Print the control-skill SKILL.md path for a runtime + its first heading. Use to discover what a runtime's skill does without a second round-trip.",
      parameters: {
        type: "object",
        properties: {
          runtime: { type: "string", description: "claude, codex, hermes, opencode, …" },
        },
        required: ["runtime"],
      },
      handler: (a: AmSkillArgs) => amSkill(a),
    },
    {
      name: "am_doctor",
      description:
        "Health check: data dir, sqlite, pty, runtimes. --verbose probes each runtime's --version. --costs reads the audit log and shows per-runtime send/assign counts.",
      parameters: {
        type: "object",
        properties: {
          costs: { type: "boolean", description: "show per-runtime activity from audit log" },
          verbose: { type: "boolean", description: "probe each runtime's --version" },
        },
      },
      handler: (a: { costs?: boolean; verbose?: boolean }) => amDoctor(a),
    },
  ];
}

// Convenience: wait for an agent to reach a status by polling `am ls`.
// Used by long-running supervisor loops that want to gate on fleet state.
export async function waitForStatus(
  ref: string,
  predicate: (line: string) => boolean,
  opts: { timeoutMs?: number; intervalMs?: number } = {},
): Promise<boolean> {
  const timeoutMs = opts.timeoutMs ?? 120_000;
  const intervalMs = opts.intervalMs ?? 5_000;
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const out = await amLs({});
    for (const line of out.split("\n")) {
      if (line.includes(ref) && predicate(line)) return true;
    }
    await sleep(intervalMs);
  }
  return false;
}
