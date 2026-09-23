#!/usr/bin/env node
// agentmaster MCP server — exposes the `am` fleet as MCP tools so any
// MCP-compatible host (Cursor, Cline, Claude Code, Devin Desktop via ACP)
// can steer the agentmaster fleet.
//
// Ponytail: stdio MCP server, ~80 lines, no framework beyond the official
// @modelcontextprotocol/sdk. Each tool is a thin wrapper around `am <cmd>`.
//
// Run:  node dist/mcp-server.js
// Or:   agentmaster-mcp  (via the bin entry in package.json)
//
// Wire into Cursor via .cursor/mcp.json:
//   { "mcpServers": { "agentmaster": { "command": "agentmaster-mcp" } } }

import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import {
  CallToolRequestSchema,
  ListToolsRequestSchema,
} from "@modelcontextprotocol/sdk/types.js";
import { createAgentmasterTools } from "./index.js";

const tools = createAgentmasterTools();

const server = new Server(
  { name: "agentmaster", version: "0.1.0" },
  { capabilities: { tools: {} } },
);

server.setRequestHandler(ListToolsRequestSchema, async () => ({
  tools: tools.map((t) => ({
    name: t.name,
    description: t.description,
    inputSchema: { type: "object" as const, ...t.parameters },
  })),
}));

server.setRequestHandler(CallToolRequestSchema, async (req) => {
  const tool = tools.find((t) => t.name === req.params.name);
  if (!tool) {
    return {
      content: [{ type: "text", text: `unknown tool: ${req.params.name}` }],
      isError: true,
    };
  }
  try {
    const result = await tool.handler(req.params.arguments ?? {});
    return { content: [{ type: "text", text: result }] };
  } catch (err: any) {
    return {
      content: [{ type: "text", text: String(err?.message ?? err) }],
      isError: true,
    };
  }
});

const transport = new StdioServerTransport();
await server.connect(transport);
