#!/usr/bin/env bash
# install-cursor-sdk.sh — drop the agentmaster .cursor/ files into a project.
# Usage: ./install-cursor-sdk.sh /path/to/project
set -eu

src_dir="$(cd "$(dirname "$0")" && pwd)/.cursor"
dest="${1:-$PWD}"

if [[ ! -d "$dest" ]]; then
  echo "target directory does not exist: $dest" >&2
  exit 1
fi

target_cursor="$dest/.cursor"
mkdir -p "$target_cursor/agents" "$target_cursor/skills/am"

cp "$src_dir/agents/agentmaster.md" "$target_cursor/agents/"
cp "$src_dir/skills/am/SKILL.md" "$target_cursor/skills/am/"
cp "$src_dir/hooks.json" "$target_cursor/"
cp "$src_dir/mcp.json" "$target_cursor/"

echo "installed agentmaster .cursor/ files → $target_cursor"
echo "next: open Cursor in $dest — the agentmaster subagent + am skill auto-load"
