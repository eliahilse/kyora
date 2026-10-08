"""Minimal stdio MCP server for the kyora CLI tests: one echo tool.

With the arguments `linger MARKER` it ignores SIGTERM and, once stdin closes, writes
its pid to MARKER and keeps running, like a server that will not shut down.
"""
import json
import os
import signal
import sys
import time

LINGER = sys.argv[1:2] == ["linger"]
if LINGER:
    signal.signal(signal.SIGTERM, signal.SIG_IGN)


def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


while True:
    line = sys.stdin.readline()
    if not line:
        break
    message = json.loads(line)
    if "id" not in message:
        continue
    method = message.get("method")
    params = message.get("params") or {}
    if method == "initialize":
        result = {
            "protocolVersion": params["protocolVersion"],
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "py", "version": "1"},
        }
    elif method == "tools/list":
        result = {
            "tools": [
                {
                    "name": "echo",
                    "description": "Echo text.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {"text": {"type": "string"}},
                        "required": ["text"],
                    },
                }
            ]
        }
    elif method == "tools/call":
        result = {"content": [{"type": "text", "text": params["arguments"]["text"]}]}
    else:
        send({"jsonrpc": "2.0", "id": message["id"], "error": {"code": -32601, "message": "no such method"}})
        continue
    send({"jsonrpc": "2.0", "id": message["id"], "result": result})

if LINGER:
    with open(sys.argv[2], "w") as marker:
        marker.write(str(os.getpid()))
    time.sleep(60)
