#!/usr/bin/env python3
"""End-to-end smoke test of the ue2-mcp server over stdio (docs/status/mcp.md).

Drives target/release/ue2-mcp like an MCP client: initialize, tools/list, emu_start (net), emu_expect console,
emu_button, emu_expect screen, emu_screenshot, emu_rest GET /v1/info, emu_key, emu_console, emu_stop.
Prints the JSON-RPC transcript (image data elided) and exits non-zero when a step fails.

    scripts/mcp-smoke.py [--server PATH] [--firmware PATH] [--shot PATH]

The server takes its paths from the environment (UE2EMU_BIN, UE2_FIRMWARE_TREE, UE2_MCP_RUN, ...).
"""
import argparse
import base64
import json
import subprocess
import sys
import time


def clip(s, n=1500):
    return s if len(s) <= n else s[:n] + f" ... [{len(s)} chars]"


def elide(msg):
    msg = json.loads(json.dumps(msg))
    for block in (msg.get("result") or {}).get("content") or []:
        if block.get("type") == "image":
            block["data"] = f"<{len(block['data'])} base64 chars>"
    return msg


class Client:
    def __init__(self, argv):
        self.proc = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, bufsize=1)
        self.last_id = 0
        self.failures = []

    def send(self, obj):
        line = json.dumps(obj)
        print(">>> " + clip(line), flush=True)
        self.proc.stdin.write(line + "\n")
        self.proc.stdin.flush()

    def request(self, method, params):
        self.last_id += 1
        rid = self.last_id
        self.send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
        while True:
            line = self.proc.stdout.readline()
            if not line:
                sys.exit("server closed stdout")
            msg = json.loads(line)
            if msg.get("id") != rid:
                print("<<< (other) " + clip(line.strip()), flush=True)
                continue
            print("<<< " + clip(json.dumps(elide(msg))), flush=True)
            if "error" in msg:
                sys.exit(f"JSON-RPC error: {msg['error']}")
            return msg["result"]

    def notify(self, method):
        self.send({"jsonrpc": "2.0", "method": method})

    def call(self, name, args, check=None):
        t0 = time.time()
        res = self.request("tools/call", {"name": name, "arguments": args})
        texts = [b["text"] for b in res.get("content", []) if b.get("type") == "text"]
        print(f"--- {name} -> isError={res.get('isError')} in {time.time() - t0:.2f} s", flush=True)
        for t in texts:
            print(t, flush=True)
        ok = not res.get("isError") and (check is None or check(res, texts))
        if not ok:
            self.failures.append(name)
            print(f"!!! step {name} failed", flush=True)
        return res, texts


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--server", default="target/release/ue2-mcp")
    ap.add_argument("--firmware")
    ap.add_argument("--shot", default="run/mcp/smoke-menu.png")
    args = ap.parse_args()

    c = Client([args.server])
    init = c.request("initialize", {
        "protocolVersion": "2025-06-18",
        "capabilities": {},
        "clientInfo": {"name": "mcp-smoke", "version": "1"},
    })
    print("server protocol", init.get("protocolVersion"), "info", init.get("serverInfo"))
    c.notify("notifications/initialized")
    tools = c.request("tools/list", {})["tools"]
    print("tools:", ", ".join(t["name"] for t in tools))

    start = {"net": True}
    if args.firmware:
        start["firmware"] = args.firmware
    _, texts = c.call("emu_start", start, check=lambda r, t: t[0].startswith("STARTED"))
    if c.failures:
        sys.exit(1)
    iid = json.loads(texts[1])["id"]
    try:
        c.call("emu_expect", {"id": iid, "source": "console", "text": "All linked modules", "timeout_ms": 30000},
               check=lambda r, t: t[0].startswith("PASS"))
        c.call("emu_button", {"id": iid})
        c.call("emu_expect", {"id": iid, "text": "Flash Disk", "timeout_ms": 10000, "require_visible": True},
               check=lambda r, t: t[0].startswith("PASS"))

        def png_ok(res, texts):
            images = [b for b in res["content"] if b.get("type") == "image"]
            return len(images) == 1 and base64.b64decode(images[0]["data"])[:8] == b"\x89PNG\r\n\x1a\n"
        c.call("emu_screenshot", {"id": iid, "save_to": args.shot}, check=png_ok)
        c.call("emu_rest", {"id": iid, "method": "GET", "path": "/v1/info", "timeout_ms": 60000},
               check=lambda r, t: t[0].startswith("HTTP 200"))
        c.call("emu_key", {"id": iid, "keys": ["down", "down"]})
        c.call("emu_console", {"id": iid, "tail_lines": 5})
    finally:
        c.call("emu_stop", {"id": iid}, check=lambda r, t: '"graceful_quit": true' in t[1])
    c.proc.stdin.close()
    c.proc.wait(timeout=30)
    print("server exit code", c.proc.returncode)
    if c.failures:
        print("FAILED steps:", ", ".join(c.failures))
        sys.exit(1)
    print("ALL STEPS PASSED")


if __name__ == "__main__":
    main()
