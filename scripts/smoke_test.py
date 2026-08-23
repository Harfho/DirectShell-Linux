#!/usr/bin/env python3
"""DirectShell end-to-end smoke test.

Requires the daemon to be running (see ../BUILD.md) and some GUI apps open.

Usage:
    python3 smoke_test.py                 # list windows, then exit
    python3 smoke_test.py <substring>     # full cycle against matching window

Example:
    printf 'hello\\n' > /tmp/prove.txt && xed --new-window /tmp/prove.txt &
    python3 smoke_test.py prove.txt
"""
import json, os, subprocess, sys, time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SERVER = os.path.join(ROOT, "ds-mcp", "server.py")


class Client:
    def __init__(self):
        self.proc = subprocess.Popen(
            [sys.executable, SERVER],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
        self._id = 0

    def rpc(self, method, params=None):
        self._id += 1
        self.proc.stdin.write(json.dumps(
            {"jsonrpc": "2.0", "id": self._id,
             "method": method, "params": params or {}}) + "\n")
        self.proc.stdin.flush()
        deadline = time.time() + 15
        while time.time() < deadline:
            msg = json.loads(self.proc.stdout.readline())
            if msg.get("id") == self._id:
                return msg["result"]
        raise TimeoutError(method)

    def call(self, name, args=None):
        res = self.rpc("tools/call", {"name": name, "arguments": args or {}})
        text = res["content"][0]["text"]
        if res.get("isError"):
            print(f"  {name}: FAILED\n{text}")
            sys.exit(1)
        return text

    def close(self):
        self.proc.terminate()


def main():
    c = Client()
    c.rpc("initialize", {"protocolVersion": "2024-11-05"})
    c.proc.stdin.write(json.dumps(
        {"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n")
    c.proc.stdin.flush()

    wins = json.loads(c.call("list_windows"))["windows"]
    print("open windows:")
    for w in wins:
        print(f"  {w['app']:30s} {w['title'][:50]}")

    if len(sys.argv) < 2:
        print("\npass a window-name substring to run the full cycle")
        c.close()
        return

    needle = sys.argv[1].lower()
    matches = [w for w in wins if needle in w["title"].lower()]
    if not matches:
        sys.exit(f"no window title matches {needle!r}")

    app = matches[0]["app"]
    print(f"\nsnapping: {app}")
    print(c.call("snap_window", {"app": app}))

    elems = json.loads(c.call("find_elements", {"role": "Edit"}))["elements"]
    if not elems:  # one retry — the daemon rebuilds its table every second
        time.sleep(1.0)
        elems = json.loads(c.call("find_elements", {"role": "Edit"}))["elements"]
    print(f"editable elements found: {len(elems)}")

    if elems:
        target = elems[0].get("name") or elems[0].get("role")
        c.call("click_element", {"element": target})
        c.call("type_text", {"text": "[directshell was here] ", "element": target})
        c.call("press_key", {"combo": "ctrl+s"})
        time.sleep(0.5)
        print("typed + saved (check the file on screen/disk)")

    c.call("unsnap")
    c.close()
    print("\nSMOKE TEST PASSED")


if __name__ == "__main__":
    main()
