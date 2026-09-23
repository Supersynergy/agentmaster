# agentmaster × Cursor SDK

Turn Cursor 3's **Agents Window** into the GUI for the agentmaster fleet —
without building a frontend. Ponytail: ~80 lines of TypeScript, zero framework
beyond the official Cursor + MCP SDKs.

## What you get

- **`.cursor/agents/agentmaster.md`** — a Cursor subagent that orchestrates the
  external fleet. Shows up in Cursor's Agents Window alongside local agents.
- **`.cursor/skills/am/SKILL.md`** — auto-loaded skill doc for the `am` router.
- **`.cursor/hooks.json`** — bridges Cursor's hook lifecycle into agentmaster's
  audit log (`agentmaster events` becomes a cross-tool timeline).
- **`.cursor/mcp.json`** — exposes `am_ls`, `am_peek`, `am_assign`, `am_handoff`,
  `am_skill`, `am_doctor` as MCP tools, so any MCP-compatible host (Cursor,
  Cline, Claude Code, Devin Desktop via ACP) can steer the fleet.
- **`src/index.ts`** — TypeScript SDK adapter: thin async wrappers around `am`
  subcommands. Use programmatically from Cursor SDK agents.
- **`src/mcp-server.ts`** — stdio MCP server (~60 lines). Run via
  `agentmaster-mcp` bin entry.

## Install

### 1. Build the adapter

```bash
cd agentmaster/cursor-sdk
npm install
npm run build
```

This produces `dist/` with `index.js`, `mcp-server.js`, `demo.js`.

### 2. Install `am` on PATH (if not already)

```bash
ln -sf ~/superskills/universal/skills/agentmaster/am.sh ~/.local/bin/am
```

### 3. Drop the `.cursor/` files into your project

```bash
cp -r .cursor /path/to/your/project/
```

Cursor auto-discovers `.cursor/agents/*.md`, `.cursor/skills/*/SKILL.md`,
`.cursor/hooks.json`, `.cursor/mcp.json` on next session start.

### 4. (Optional) Install the MCP server globally

```bash
npm install -g @agentmaster/cursor-sdk
```

Then `.cursor/mcp.json`'s `"command": "agentmaster-mcp"` resolves from PATH.

## Verify

```bash
# 1. am is on PATH
am doctor --costs

# 2. MCP server starts
node dist/mcp-server.js &
# should print nothing and wait for stdio

# 3. Demo smoke
node dist/demo.js
# prints am_ls + am_doctor + am_skill + registered tools
```

## Architecture

```
Cursor 3 Agents Window
   │
   ├── .cursor/agents/agentmaster.md   ← subagent definition
   ├── .cursor/skills/am/SKILL.md      ← auto-loaded skill
   ├── .cursor/hooks.json              ← audit-log bridge
   └── .cursor/mcp.json                ← MCP server config
              │
              ▼
   agentmaster-mcp (stdio MCP server)
              │
              ▼
   am  (ponytail bash router, ~80 lines)
              │
              ▼
   agentmaster CLI  (Rust binary)
              │
              ▼
   cmux workspaces + tmux panes  (the actual fleet)
```

Each layer is thin and replaceable. The `am` CLI stays the source of truth;
the SDK adapter and MCP server are just call sites.

## Why this design

- **Ponytail**: no frontend, no framework, no state in the adapter. ~80 lines
  of TypeScript + ~80 lines of bash. The Rust CLI does the work.
- **Zero token tax**: `am_peek` reads transcript JSONL directly, never asks
  agents to report. The supervisor subagent gets fleet state for free.
- **Failsafe**: missing `am` → clear error; missing skill → "no control skill
  registered"; MCP server down → Cursor falls back to Bash.
- **Portable**: MCP server works in Cursor, Cline, Claude Code, Devin Desktop
  (via ACP). The `.cursor/` files are Cursor-specific; the MCP server isn't.

## Cursor 3 Agents Window = gratis Kanban GUI

Once `.cursor/agents/agentmaster.md` is in place, every `am_assign` spawn
shows up as a card in Cursor's Agents Window. Every `am_handoff` becomes a
synchronous conversation you can watch in real time. You get the polished
multi-workspace GUI without writing a single line of frontend code.

## Files

```
cursor-sdk/
├── package.json
├── tsconfig.json
├── README.md
├── src/
│   ├── index.ts          # SDK adapter: amLs, amPeek, amAssign, amHandoff, amSkill, amDoctor
│   ├── mcp-server.ts     # stdio MCP server
│   └── demo.ts           # smoke test
└── .cursor/              # drop-in for any Cursor project
    ├── agents/
    │   └── agentmaster.md
    ├── skills/
    │   └── am/
    │       └── SKILL.md
    ├── hooks.json
    └── mcp.json
```
