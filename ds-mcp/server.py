#!/usr/bin/env python3
"""DirectShell MCP server.

Bridges AI agents to the DirectShell Linux daemon via its file/DB protocol:

  ds_profiles/windows.json   window enumeration (rewritten every 2s)
  ds_profiles/snap_request   write an app key (or "!unsnap"); daemon consumes it
  ds_profiles/snap_result    {"status":"ok"|"error", ...}
  ds_profiles/is_active      "none" or "app\\n<db>.a11y\\n<db>.snap\\n"
  <app>.db                   SQLite: elements + inject tables
                             inject(action,text,target,done) rows are the
                             input pipeline: text|type|key|click|scroll

Speaks newline-delimited JSON-RPC 2.0 (MCP) on stdio. Stdlib only.
"""

import json
import os
import sqlite3
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
PROFILES = os.path.join(ROOT, "ds_profiles")

WINDOWS_FILE = os.path.join(PROFILES, "windows.json")
SNAP_REQUEST = os.path.join(PROFILES, "snap_request")
SNAP_RESULT = os.path.join(PROFILES, "snap_result")
ACTIVE_FILE = os.path.join(PROFILES, "is_active")

SERVER_INFO = {"name": "directshell", "version": "1.0.0"}
TOOLS = [
    {
        "name": "list_windows",
        "description": "List open desktop windows. Each entry has title, app "
        "(the key used for snap_window), exe and hwnd.",
        "inputSchema": {"type": "object", "properties": {}},
    },
    {
        "name": "snap_window",
        "description": "Attach DirectShell to a window by its app key "
        "(from list_windows). Enables the element tree and input injection "
        "for that window.",
        "inputSchema": {
            "type": "object",
            "properties": {"app": {"type": "string"}},
            "required": ["app"],
        },
    },
    {
        "name": "unsnap",
        "description": "Detach from the currently snapped window.",
        "inputSchema": {"type": "object", "properties": {}},
    },
    {
        "name": "active_status",
        "description": "Return the currently snapped app, its db/a11y/snap file "
        "paths and whether a tree dump is flowing.",
        "inputSchema": {"type": "object", "properties": {}},
    },
    {
        "name": "find_elements",
        "description": "Search elements of the snapped window's accessibility "
        "tree. Filters combine with AND. By default only visible elements are "
        "returned; set offscreen=true to include hidden ones.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "role": {"type": "string", "description": "UIA-style role, e.g. Edit, Button"},
                "name": {"type": "string", "description": "exact accessible name"},
                "name_contains": {"type": "string", "description": "substring match on name"},
                "value_contains": {"type": "string", "description": "substring match on value"},
                "offscreen": {"type": "boolean", "description": "include offscreen elements"},
                "limit": {"type": "integer", "description": "max rows (default 50)"},
            },
        },
    },
    {
        "name": "click_element",
        "description": "Click an element of the snapped window by exact name or "
        "UIA-style role (falls back to role if no name matches).",
        "inputSchema": {
            "type": "object",
            "properties": {"element": {"type": "string"}},
            "required": ["element"],
        },
    },
    {
        "name": "type_text",
        "description": "Type text into the snapped window. If element is given, "
        "try AT-SPI EditableText first, then focus+XTEST typing. Without "
        "element, re-click the last clicked point and type.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "text": {"type": "string"},
                "element": {"type": "string"},
            },
            "required": ["text"],
        },
    },
    {
        "name": "press_key",
        "description": "Send a key combo to the snapped window, e.g. 'ctrl+s', "
        "'alt+F4', 'enter', 'ctrl+a'.",
        "inputSchema": {
            "type": "object",
            "properties": {"combo": {"type": "string"}},
            "required": ["combo"],
        },
    },
    {
        "name": "set_clipboard",
        "description": "Put text on the X11 CLIPBOARD selection "
        "(owned natively by the daemon).",
        "inputSchema": {
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"],
        },
    },
    {
        "name": "paste_text",
        "description": "Paste text into the snapped window via clipboard + "
        "ctrl+v (fast; works where per-key typing fails). Optionally clicks "
        "an element first to place the caret.",
        "inputSchema": {
            "type": "object",
            "properties": {"text": {"type": "string"}, "element": {"type": "string"}},
            "required": ["text"],
        },
    },
    {
        "name": "get_clipboard",
        "description": "Read current X11 CLIPBOARD contents (e.g. after "
        "select-all + copy in the target app).",
        "inputSchema": {"type": "object", "properties": {}},
    },
    {
        "name": "scroll",
        "description": "Scroll the snapped window. Direction is 'up' or 'down', "
        "optionally with a count, e.g. 'down x3'.",
        "inputSchema": {
            "type": "object",
            "properties": {"direction": {"type": "string"}},
            "required": ["direction"],
        },
    },
]


def _read(path):
    try:
        with open(path, "r") as f:
            return f.read()
    except OSError:
        return None


def active_db():
    """Path of the currently snapped app's db, or None."""
    content = _read(ACTIVE_FILE)
    if not content:
        return None
    first = content.strip().splitlines()[0].strip() if content.strip() else ""
    if not first or first == "none":
        return None
    return os.path.join(PROFILES, first + ".db")


def tool_list_windows(_args):
    raw = _read(WINDOWS_FILE)
    if not raw:
        return {"content": [{"type": "text", "text": "windows.json not found — daemon running?"}]}
    data = json.loads(raw)
    wins = [
        {k: w.get(k) for k in ("title", "app", "exe", "hwnd")}
        for w in data.get("windows", [])
    ]
    age = int(time.time()) - int(data.get("timestamp", 0))
    return {
        "content": [
            {
                "type": "text",
                "text": json.dumps({"age_seconds": age, "windows": wins}, indent=1),
            }
        ]
    }


def _arg(args, key, default=""):
    v = args.get(key)
    return default if v is None else str(v).strip()


def tool_snap_window(args):
    app = _arg(args, "app")
    if not app:
        raise ValueError("snap_window needs an app key from list_windows")
    # clear stale result, then request
    try:
        os.remove(SNAP_RESULT)
    except OSError:
        pass
    with open(SNAP_REQUEST, "w") as f:
        f.write(app)
    deadline = time.time() + 6.0
    while time.time() < deadline:
        res = _read(SNAP_RESULT)
        if res:
            try:
                out = json.loads(res)
            except json.JSONDecodeError:
                out = {"status": "error", "reason": f"unparseable snap_result: {res!r}"}
            if out.get("status") == "ok":
                # wait until the first tree dump lands (is_active written by
                # the dump thread AFTER streaming — can take ~0.5-1s)
                deadline2 = time.time() + 3.0
                while time.time() < deadline2 and active_db() is None:
                    time.sleep(0.1)
                status = tool_active_status({})
                return {"content": [{"type": "text", "text": json.dumps(out) + "\n" + status["content"][0]["text"]}]}
            return {"content": [{"type": "text", "text": json.dumps(out)}]}
        time.sleep(0.15)
    return {
        "content": [{"type": "text", "text": json.dumps({"status": "error", "reason": "timeout waiting for snap_result"})}]
    }


def tool_unsnap(_args):
    if not _read(ACTIVE_FILE) or active_db() is None:
        return {"content": [{"type": "text", "text": '{"status":"ok","note":"nothing snapped"}'}]}
    try:
        os.remove(SNAP_RESULT)
    except OSError:
        pass
    with open(SNAP_REQUEST, "w") as f:
        f.write("!unsnap")
    deadline = time.time() + 4.0
    while time.time() < deadline:
        if _read(SNAP_RESULT):
            break
        time.sleep(0.1)
    return {"content": [{"type": "text", "text": json.dumps({"status": "ok"})}]}


def tool_active_status(_args):
    db = active_db()
    if db is None:
        return {"content": [{"type": "text", "text": '{"snapped": false}'}]}
    app = os.path.basename(db)[: -len(".db")]
    counts = {}
    try:
        conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True, timeout=3)
        counts["elements"] = conn.execute("SELECT COUNT(*) FROM elements").fetchone()[0]
        counts["visible"] = conn.execute(
            "SELECT COUNT(*) FROM elements WHERE offscreen=0 AND w>0 AND h>0"
        ).fetchone()[0]
        pending = conn.execute("SELECT COUNT(*) FROM inject WHERE done=0").fetchone()[0]
        conn.close()
        counts["inject_pending"] = pending
    except sqlite3.Error as e:
        counts["error"] = str(e)
    return {
        "content": [
            {
                "type": "text",
                "text": json.dumps(
                    {"snapped": True, "app": app, "db": db,
                     "a11y_file": db[:-3] + ".a11y", "counts": counts},
                    indent=1,
                ),
            }
        ]
    }


def tool_find_elements(args):
    db = active_db()
    if db is None:
        raise RuntimeError("no window snapped — call snap_window first")
    where = []
    params = []
    if args.get("role"):
        where.append("role = ?")
        params.append(str(args["role"]))
    if args.get("name"):
        where.append("name = ?")
        params.append(str(args["name"]))
    if args.get("name_contains"):
        where.append("name LIKE ?")
        params.append(f"%{args['name_contains']}%")
    if args.get("value_contains"):
        where.append("value LIKE ?")
        params.append(f"%{args['value_contains']}%")
    if not args.get("offscreen"):
        where.append("offscreen = 0 AND w > 0 AND h > 0")
    limit = max(1, min(int(args.get("limit", 50)), 500))
    sql = (
        "SELECT id,parent_id,depth,role,name,value,automation_id,"
        "enabled,offscreen,x,y,w,h FROM elements"
    )
    if where:
        sql += " WHERE " + " AND ".join(where)
    sql += " ORDER BY id LIMIT ?"
    params.append(limit)
    # the daemon rebuilds this table every dump cycle (~1s); ride out locks
    deadline = time.time() + 3.0
    while True:
        try:
            conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True, timeout=3)
            conn.row_factory = sqlite3.Row
            rows = [dict(r) for r in conn.execute(sql, params)]
            conn.close()
            break
        except sqlite3.Error:
            conn.close() if "conn" in dir() else None
            if time.time() >= deadline:
                raise
            time.sleep(0.2)
    return {
        "content": [
            {
                "type": "text",
                "text": json.dumps({"count": len(rows), "elements": rows}, indent=1),
            }
        ]
    }


def _queue_inject(conn, action, text, target):
    cur = conn.cursor()
    cur.execute(
        "SELECT name FROM sqlite_master WHERE type='table' AND name='inject'"
    )
    if not cur.fetchone():
        raise RuntimeError("inject table missing in " + conn.execute(
            "PRAGMA database_list").fetchone()[2])
    before = conn.execute(
        "SELECT COALESCE(MAX(id),0) FROM inject WHERE done=1"
    ).fetchone()[0]
    conn.execute(
        "INSERT INTO inject(action,text,target,done) VALUES(?,?,?,0)",
        (action, text, target),
    )
    conn.commit()
    deadline = time.time() + 8.0
    while time.time() < deadline:
        row = conn.execute(
            "SELECT done FROM inject WHERE id=(SELECT MAX(id) FROM inject)"
        ).fetchone()
        if row and row[0] == 1:
            return {"status": "done"}
        time.sleep(0.15)
    return {"status": "queued", "note": "daemon has not consumed it yet"}


def tool_click_element(args):
    db = active_db()
    if db is None:
        raise RuntimeError("no window snapped — call snap_window first")
    el = _arg(args, "element")
    if not el:
        raise ValueError("click_element needs an element name or role")
    conn = sqlite3.connect(db, timeout=5)
    conn.execute("PRAGMA busy_timeout=3000")
    out = _queue_inject(conn, "click", "", el)
    conn.close()
    return {"content": [{"type": "text", "text": json.dumps({"clicked": el, **out})}]}


def tool_type_text(args):
    db = active_db()
    if db is None:
        raise RuntimeError("no window snapped — call snap_window first")
    text = _arg(args, "text")
    el = _arg(args, "element")
    action = "text" if el else "type"
    conn = sqlite3.connect(db, timeout=5)
    conn.execute("PRAGMA busy_timeout=3000")
    out = _queue_inject(conn, action, text, el)
    conn.close()
    return {"content": [{"type": "text", "text": json.dumps({"action": action, **out})}]}


def tool_set_clipboard(args):
    db = active_db()
    if db is None:
        raise RuntimeError("no window snapped — call snap_window first")
    text = str(args.get("text", ""))
    conn = sqlite3.connect(db, timeout=5)
    conn.execute("PRAGMA busy_timeout=3000")
    out = _queue_inject(conn, "clipset", text, "")
    conn.close()
    return {"content": [{"type": "text", "text": json.dumps({"action": "clipset", **out})}]}


def tool_paste_text(args):
    """Clipboard paste into the snapped window: sets CLIPBOARD then ctrl+v."""
    db = active_db()
    if db is None:
        raise RuntimeError("no window snapped — call snap_window first")
    text = str(args.get("text", ""))
    el = _arg(args, "element")
    conn = sqlite3.connect(db, timeout=5)
    conn.execute("PRAGMA busy_timeout=3000")
    if el:
        clicked = _queue_inject(conn, "click", "", el)
        if clicked.get("status") != "done":
            conn.close()
            raise RuntimeError(f"paste_text: could not click element {el!r}")
        time.sleep(0.2)
    out = _queue_inject(conn, "paste", text, "")
    conn.close()
    return {"content": [{"type": "text", "text": json.dumps({"action": "paste", "clicked": el, **out})}]}


def tool_get_clipboard(_args):
    db = active_db()
    if db is None:
        raise RuntimeError("no window snapped — call snap_window first")
    out_file = os.path.join(ROOT, "ds_profiles", "clipboard_out")
    try:
        os.remove(out_file)
    except OSError:
        pass
    conn = sqlite3.connect(db, timeout=5)
    conn.execute("PRAGMA busy_timeout=3000")
    res = _queue_inject(conn, "clipget", "", "")
    conn.close()
    data = ""
    deadline = time.time() + 2.0
    while time.time() < deadline:
        try:
            with open(out_file, "rb") as f:
                data = f.read().decode("utf-8", "replace")
            break
        except OSError:
            time.sleep(0.1)
    return {"content": [{"type": "text",
                         "text": json.dumps({"status": res.get("status"), "clipboard": data})}]}


def tool_press_key(args):
    db = active_db()
    if db is None:
        raise RuntimeError("no window snapped — call snap_window first")
    combo = _arg(args, "combo")
    if not combo:
        raise ValueError("press_key needs a combo like 'ctrl+s'")
    conn = sqlite3.connect(db, timeout=5)
    conn.execute("PRAGMA busy_timeout=3000")
    out = _queue_inject(conn, "key", combo, "")
    conn.close()
    return {"content": [{"type": "text", "text": json.dumps({"key": combo, **out})}]}


def tool_scroll(args):
    db = active_db()
    if db is None:
        raise RuntimeError("no window snapped — call snap_window first")
    direction = _arg(args, "direction", "down")
    conn = sqlite3.connect(db, timeout=5)
    conn.execute("PRAGMA busy_timeout=3000")
    out = _queue_inject(conn, "scroll", direction, "")
    conn.close()
    return {"content": [{"type": "text", "text": json.dumps({"scrolled": direction, **out})}]}


HANDLERS = {
    "list_windows": tool_list_windows,
    "snap_window": tool_snap_window,
    "unsnap": tool_unsnap,
    "active_status": tool_active_status,
    "find_elements": tool_find_elements,
    "click_element": tool_click_element,
    "type_text": tool_type_text,
    "press_key": tool_press_key,
    "scroll": tool_scroll,
    "set_clipboard": tool_set_clipboard,
    "get_clipboard": tool_get_clipboard,
    "paste_text": tool_paste_text,
}


def dispatch(name, args):
    handler = HANDLERS.get(name)
    if handler is None:
        raise ValueError(f"unknown tool: {name}")
    return handler(args or {})


def reply(msg_id, result=None, error=None):
    msg = {"jsonrpc": "2.0", "id": msg_id}
    if error is not None:
        msg["error"] = error
    else:
        msg["result"] = result
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()


def handle(req):
    method = req.get("method", "")
    msg_id = req.get("id")
    if method == "initialize":
        reply(msg_id, {
            "protocolVersion": req.get("protocolVersion", "2024-11-05"),
            "capabilities": {"tools": {}},
            "serverInfo": SERVER_INFO,
        })
    elif method == "notifications/initialized":
        pass  # notification: no response
    elif method == "ping":
        reply(msg_id, {})
    elif method == "tools/list":
        reply(msg_id, {"tools": TOOLS})
    elif method == "tools/call":
        params = req.get("params", {})
        name = params.get("name", "")
        try:
            reply(msg_id, dispatch(name, params.get("arguments")))
        except Exception as e:  # noqa: BLE001 - report as tool error
            reply(msg_id, {
                "content": [{"type": "text", "text": f"{type(e).__name__}: {e}"}],
                "isError": True,
            })
    elif method.startswith("notifications/"):
        pass
    elif msg_id is not None:
        reply(msg_id, error={"code": -32601, "message": f"unknown method {method}"})


def main():
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
        except json.JSONDecodeError:
            continue
        handle(req)


if __name__ == "__main__":
    main()
