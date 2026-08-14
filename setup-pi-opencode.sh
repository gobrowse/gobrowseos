#!/usr/bin/env bash
# Minimal global Pi MCP setup. Requires Pi and Node.js/npx.
set -euo pipefail

rm -f ~/.pi/agent/mcp.json ./plan-mode.md ./.pirc ./setup-pi-opencode.sh
mkdir -p ~/.pi/agent/
pi install npm:pi-mcp-adapter@2.25.0

cat > ~/.pi/agent/mcp.json <<'JSON'
{
  "settings": {
    "directTools": true
  },
  "mcpServers": {
    "sequential-thinking": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-sequential-thinking"]
    }
  }
}
JSON

printf '%s\n' "Setup complete. Use '[PLAN] your request' to trigger Plan Mode."
