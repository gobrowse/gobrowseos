#!/usr/bin/env python3
"""Raw MCP stdio server (no SDK) for Gobrowse M7 transport validation."""
import sys, json

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method = msg.get("method", "")
    mid = msg.get("id")
    params = msg.get("params", {})

    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "protocolVersion": "2026-07-28",
            "capabilities": {"tools": {"listChanged": False}},
            "serverInfo": {"name": "gobrowse-m7-raw", "version": "1.0.0"}
        }})
    elif method == "notifications/initialized":
        pass  # no response for notifications
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "tools": [{
                "name": "echo",
                "description": "Echo back the input message",
                "inputSchema": {
                    "type": "object",
                    "properties": {"message": {"type": "string"}},
                    "required": ["message"]
                }
            }]
        }})
    elif method == "tools/call":
        name = params.get("name", "")
        args = params.get("arguments", {})
        if name == "echo":
            send({"jsonrpc": "2.0", "id": mid, "result": {
                "content": [{"type": "text", "text": f"echo: {args.get('message', '')}"}]
            }})
        else:
            send({"jsonrpc": "2.0", "id": mid, "error": {
                "code": -32601, "message": f"Unknown tool: {name}"
            }})
    else:
        if mid is not None:
            send({"jsonrpc": "2.0", "id": mid, "error": {
                "code": -32601, "message": f"Method not found: {method}"
            }})
