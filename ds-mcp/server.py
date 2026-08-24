#!/usr/bin/env python3
"""DirectShell MCP server — smart context-aware edition.

Every action tool automatically:
  - detects dialogs / new windows that appeared after the action
  - reports what changed in the UI (new elements, value changes, enabled state)
  - returns a concise UI context so the AI always knows what state it's in

New tools vs the original:
  get_ui_state    — full smart snapshot (title, inputs, buttons, dialogs)
  perform         — action + wait for UI to settle + return what changed
  wait_for_change — block until something in the UI changes or timeout
  snap_dialog     — auto-detect and snap to a dialog that just appeared

Speaks newline-delimited JSON-RPC 2.0 (MCP) on stdio. Stdlib only.
"""

import json
import os
import sqlite3
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


# ── Profile directory resolution ─────────────────────────────────────────────

def _find_profiles():
    """Locate the ds_profiles dir used by the running daemon.
    Checks target/release, target/debug, then project root.
    """
    candidates = [
        os.path.join(ROOT, "target", "release", "ds_profiles"),
        os.path.join(ROOT, "target", "debug",   "ds_profiles"),
        os.path.join(ROOT, "ds_profiles"),
    ]
    for p in candidates:
        lock = os.path.join(p, "directshell.lock")
        try:
            pid = int(open(lock).read().strip())
            if os.path.exists(f"/proc/{pid}"):
                return p
        except (OSError, ValueError):
            pass
    return candidates[0]

PROFILES     = _find_profiles()
WINDOWS_FILE = os.path.join(PROFILES, "windows.json")
SNAP_REQUEST = os.path.join(PROFILES, "snap_request")
SNAP_RESULT  = os.path.join(PROFILES, "snap_result")
ACTIVE_FILE  = os.path.join(PROFILES, "is_active")

SERVER_INFO = {"name": "directshell", "version": "2.0.0"}


# ── Tool definitions ──────────────────────────────────────────────────────────

TOOLS = [
    {
        "name": "list_windows",
        "description":
            "List all open desktop windows. Each entry has title, app key "
            "(use this for snap_window), exe and hwnd. "
            "Call this first to find what app to snap to.",
        "inputSchema": {"type": "object", "properties": {}},
    },
    {
        "name": "snap_window",
        "description":
            "Attach DirectShell to a window by its app key (from list_windows). "
            "Returns a full UI state snapshot so you know exactly what's on screen. "
            "Must be called before any action tool.",
        "inputSchema": {
            "type": "object",
            "properties": {"app": {"type": "string", "description": "app key from list_windows"}},
            "required": ["app"],
        },
    },
    {
        "name": "snap_dialog",
        "description":
            "After an action triggers a popup or dialog (Save As, Open File, "
            "confirmation, error), call this to automatically detect and snap to it. "
            "Waits up to timeout_s seconds for a new window to appear. "
            "Returns full UI state of the dialog once snapped. "
            "Call snap_window(original_app) to return to the main window afterwards.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "timeout_s": {"type": "number", "description": "seconds to wait (default 4)"},
                "hint":      {"type": "string", "description": "substring to match in dialog title (optional)"},
            },
        },
    },
    {
        "name": "unsnap",
        "description": "Detach from the currently snapped window.",
        "inputSchema": {"type": "object", "properties": {}},
    },
    {
        "name": "get_ui_state",
        "description":
            "Return a concise, human-readable snapshot of the current UI: "
            "window title, all visible input fields with their current values, "
            "all visible buttons, any dialogs detected, and element counts. "
            "Call this after snapping, after any action, or any time you need "
            "to know what's currently on screen.",
        "inputSchema": {"type": "object", "properties": {}},
    },
    {
        "name": "active_status",
        "description":
            "Return raw snapped app info: app name, db path, element counts. "
            "Use get_ui_state for a richer view.",
        "inputSchema": {"type": "object", "properties": {}},
    },
    {
        "name": "find_elements",
        "description":
            "Search the accessibility tree with filters. Combines with AND. "
            "Returns id, role, name, value, position, enabled, offscreen. "
            "offscreen=true to include hidden elements.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "role":          {"type": "string",  "description": "UIA role e.g. Edit, Button, CheckBox"},
                "name":          {"type": "string",  "description": "exact accessible name"},
                "name_contains": {"type": "string",  "description": "substring of name"},
                "value_contains":{"type": "string",  "description": "substring of value"},
                "offscreen":     {"type": "boolean", "description": "include hidden elements"},
                "limit":         {"type": "integer", "description": "max results (default 50)"},
            },
        },
    },
    {
        "name": "perform",
        "description":
            "Execute an action and wait for the UI to settle, then return what changed. "
            "This is the PREFERRED way to do any action because it automatically: "
            "  - runs the action (click, type, key, invoke, scroll, paste) "
            "  - waits up to settle_s seconds for the UI to stop changing "
            "  - detects any new dialogs that appeared "
            "  - returns a before/after diff of what changed "
            "action values: click | type | key | invoke | scroll | paste | text "
            "(URL-shaped text typed without target is auto-guarded: pasted "
            "instead of keystroked when no editable field has focus) "
            "Examples: "
            "  perform(action='click', target='Save') "
            "  perform(action='type', text='hello', target='Name') "
            "  perform(action='key', text='ctrl+s') "
            "  perform(action='invoke', target='File > Save') ",
        "inputSchema": {
            "type": "object",
            "properties": {
                "action":    {"type": "string",  "description": "click|type|key|invoke|scroll|paste|text"},
                "text":      {"type": "string",  "description": "text to type/paste, or key combo"},
                "target":    {"type": "string",  "description": "element name for click/invoke/text"},
                "settle_s":  {"type": "number",  "description": "seconds to wait for UI to settle (default 1.5)"},
                "watch_dialogs": {"type": "boolean", "description": "watch for new dialogs (default true)"},
            },
            "required": ["action"],
        },
    },
    {
        "name": "wait_for_change",
        "description":
            "Wait until something changes in the UI or timeout. "
            "Returns what changed (new elements, value changes, dialogs). "
            "Useful after triggering async operations like file loads, "
            "network requests, or animations.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "timeout_s":    {"type": "number",  "description": "max seconds to wait (default 5)"},
                "watch_dialogs":{"type": "boolean", "description": "also watch for new windows (default true)"},
            },
        },
    },
    {
        "name": "click_element",
        "description":
            "Click an element by name or role. "
            "After clicking, automatically checks for new dialogs and returns UI context. "
            "Prefer perform(action='click') for full change detection.",
        "inputSchema": {
            "type": "object",
            "properties": {"element": {"type": "string"}},
            "required": ["element"],
        },
    },
    {
        "name": "type_text",
        "description":
            "Type text. With element: AT-SPI EditableText (reliable). "
            "Without element: XTEST keystrokes into focused widget. "
            "URL-shaped text typed without an element is guarded: the daemon "
            "checks the focused widget and delivers via clipboard paste if "
            "nothing editable has focus (avoids browser Quick Find on '/'). "
            "Returns UI context after typing.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "text":    {"type": "string"},
                "element": {"type": "string"},
            },
            "required": ["text"],
        },
    },
    {
        "name": "press_key",
        "description":
            "Send a key combo e.g. 'ctrl+s', 'enter', 'alt+F4'. "
            "After the key, checks for dialogs and returns UI context.",
        "inputSchema": {
            "type": "object",
            "properties": {"combo": {"type": "string"}},
            "required": ["combo"],
        },
    },
    {
        "name": "invoke_element",
        "description":
            "Trigger an element's AT-SPI default action (button press, menu open, "
            "link follow) without coordinates. Only works on elements with actions > 0.",
        "inputSchema": {
            "type": "object",
            "properties": {"element": {"type": "string"}},
            "required": ["element"],
        },
    },
    {
        "name": "set_clipboard",
        "description": "Put text on the X11 CLIPBOARD.",
        "inputSchema": {
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"],
        },
    },
    {
        "name": "paste_text",
        "description":
            "Paste text via clipboard + ctrl+v. Fast, works where keystroke-typing fails. "
            "Optionally clicks an element first to place the caret.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "text":    {"type": "string"},
                "element": {"type": "string"},
            },
            "required": ["text"],
        },
    },
    {
        "name": "get_clipboard",
        "description": "Read current X11 CLIPBOARD contents.",
        "inputSchema": {"type": "object", "properties": {}},
    },
    {
        "name": "scroll",
        "description": "Scroll the snapped window: 'up', 'down', 'left', 'right'.",
        "inputSchema": {
            "type": "object",
            "properties": {"direction": {"type": "string"}},
            "required": ["direction"],
        },
    },
    {
        "name": "get_notes",
        "description":
            "Read shared AI notes: lessons other AI sessions logged while "
            "driving DirectShell (gotchas, workarounds, what to do in tricky "
            "situations). Check here FIRST when something misbehaves or feels "
            "off — another AI may have already solved it.",
        "inputSchema": {"type": "object", "properties": {}},
    },
    {
        "name": "append_note",
        "description":
            "Log a lesson to the shared AI notes so future AI sessions can "
            "act fast. Use whenever you discover a gotcha or workaround "
            "(e.g. 'typing a URL without an element opens Firefox Quick Find' "
            "-> 'pass element=address bar or use paste_text').",
        "inputSchema": {
            "type": "object",
            "properties": {
                "app":       {"type": "string", "description": "app/context e.g. 'firefox', 'thunar'"},
                "situation": {"type": "string", "description": "what went wrong / the trap"},
                "do":        {"type": "string", "description": "what to do instead"},
            },
            "required": ["situation", "do"],
        },
    },
]


# ── Low-level helpers ─────────────────────────────────────────────────────────

def _read(path):
    try:
        with open(path, "r") as f:
            return f.read()
    except OSError:
        return None


def _arg(args, key, default=""):
    v = args.get(key)
    return default if v is None else str(v).strip()


def active_db():
    """Absolute path of the currently snapped app's .db, or None."""
    content = _read(ACTIVE_FILE)
    if not content:
        return None
    first = content.strip().splitlines()[0].strip() if content.strip() else ""
    if not first or first == "none":
        return None
    db = (first + ".db") if os.path.isabs(first) else os.path.join(PROFILES, first + ".db")
    return db


# ── Shared AI notes ───────────────────────────────────────────────────────────

NOTES_HEADER = (
    "# DirectShell AI Notes\n"
    "Shared lessons from AI sessions driving DirectShell — newest first.\n"
    "Read with get_notes, append with append_note. Check here first when\n"
    "something misbehaves: another AI may have already solved it.\n"
)

# Knowledge shipped with this build — new installs start from it.
SEED_NOTES = os.path.join(os.path.dirname(os.path.abspath(__file__)), "AI_NOTES.md")


def _notes_path():
    """Primary: ~/.config/directshell/AI_NOTES.md (shared across versions),
    auto-seeded on first run from the AI_NOTES.md bundled with this build.
    Fallback: this version's ds_profiles/ dir if the config dir is unusable."""
    cfg = os.path.expanduser("~/.config/directshell")
    try:
        os.makedirs(cfg, exist_ok=True)
        p = os.path.join(cfg, "AI_NOTES.md")
        if not os.path.exists(p):
            seed = _read(SEED_NOTES)
            if seed:
                with open(p, "w") as f:
                    f.write(seed)
            else:
                with open(p, "a"):
                    pass
        return p
    except OSError:
        pass
    return os.path.join(PROFILES, "AI_NOTES.md")


def _prepend_note(entry):
    path = _notes_path()
    try:
        existing = _read(path) or ""
        marker = "\n## "
        idx = existing.find(marker)
        if existing.startswith("#") and idx != -1:
            new = existing[:idx + 1] + entry + existing[idx + 1:]
        else:
            new = NOTES_HEADER + "\n" + entry + (existing if not existing else "")
        with open(path, "w") as f:
            f.write(new)
        return path
    except OSError:
        return None


def _current_windows():
    """Return dict of hwnd → window_info from windows.json."""
    raw = _read(WINDOWS_FILE)
    if not raw:
        return {}
    try:
        return {w["hwnd"]: w for w in json.loads(raw).get("windows", [])}
    except (json.JSONDecodeError, KeyError):
        return {}


def _db_snapshot(db):
    """Return a lightweight snapshot of the elements table for diffing.

    Returns dict: {(name, role, depth): {"value": ..., "enabled": ..., "offscreen": ...}}
    Only includes named, on-screen elements.
    """
    snap = {}
    try:
        conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True, timeout=3)
        rows = conn.execute(
            "SELECT name, role, depth, value, enabled, offscreen "
            "FROM elements WHERE name IS NOT NULL AND name != '' "
            "AND offscreen=0 AND w>0 AND h>0"
        ).fetchall()
        conn.close()
        for name, role, depth, value, enabled, offscreen in rows:
            snap[(name, role, depth)] = {"value": value, "enabled": enabled, "offscreen": offscreen}
    except sqlite3.Error:
        pass
    return snap


def _diff_snapshots(before, after):
    """Compare two snapshots and return human-readable change lines."""
    changes = []
    before_keys = set(before)
    after_keys  = set(after)

    appeared    = after_keys - before_keys
    disappeared = before_keys - after_keys
    common      = before_keys & after_keys

    for (name, role, _) in sorted(appeared, key=lambda x: x[0]):
        changes.append(f"+ {role} \"{name}\" appeared")
    for (name, role, _) in sorted(disappeared, key=lambda x: x[0]):
        changes.append(f"- {role} \"{name}\" disappeared")
    for key in sorted(common, key=lambda x: x[0]):
        b = before[key]
        a = after[key]
        name, role, _ = key
        if b["enabled"] != a["enabled"]:
            state = "enabled" if a["enabled"] else "disabled"
            changes.append(f"~ {role} \"{name}\" became {state}")
        if (b["value"] or "") != (a["value"] or ""):
            bv = (b["value"] or "")[:40]
            av = (a["value"] or "")[:40]
            changes.append(f"~ {role} \"{name}\" value: \"{bv}\" → \"{av}\"")

    return changes


def _wait_for_settle(db, settle_s=1.0):
    """Wait until the elements table stops changing, up to settle_s seconds.
    Returns (before_snap, after_snap, change_lines).
    Exits as soon as stable for STABLE_WINDOW — doesn't wait the full settle_s.
    """
    before = _db_snapshot(db)
    deadline = time.time() + settle_s
    last_snap = before
    stable_since = time.time()
    STABLE_WINDOW = 0.25  # 250ms stable = settled

    while time.time() < deadline:
        time.sleep(0.15)
        current = _db_snapshot(db)
        if current != last_snap:
            last_snap = current
            stable_since = time.time()
        elif time.time() - stable_since >= STABLE_WINDOW:
            break  # stable — exit early

    after   = last_snap
    changes = _diff_snapshots(before, after)
    return before, after, changes


def _detect_new_dialog(windows_before, timeout_s=1.5):
    """Watch for new windows appearing within timeout_s seconds.
    Returns the first new WinInfo dict or None.
    """
    deadline = time.time() + timeout_s
    while time.time() < deadline:
        time.sleep(0.2)
        current = _current_windows()
        new_hwnds = set(current) - set(windows_before)
        if new_hwnds:
            # prefer the one with smallest hwnd (appeared first)
            hwnd = min(new_hwnds)
            return current[hwnd]
    return None


def _build_ui_state(db=None, extra_context=None):
    """Build a concise AI-friendly UI state dict."""
    if db is None:
        db = active_db()
    if db is None:
        return {"snapped": False}

    app = os.path.basename(db)[:-3]
    state = {"snapped": True, "app": app}

    # window title from meta table
    try:
        conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True, timeout=3)
        conn.row_factory = sqlite3.Row

        title = conn.execute(
            "SELECT value FROM meta WHERE key='window'"
        ).fetchone()
        state["window_title"] = title[0] if title else app

        # visible input fields with current values
        inputs = conn.execute(
            "SELECT role, name, value, enabled FROM elements "
            "WHERE offscreen=0 AND w>0 AND h>0 "
            "AND role IN ('Edit','Document','ComboBox','Spinner','CheckBox','RadioButton') "
            "ORDER BY y, x LIMIT 30"
        ).fetchall()
        state["inputs"] = [
            {"role": r["role"], "name": r["name"],
             "value": (r["value"] or "")[:80], "enabled": bool(r["enabled"])}
            for r in inputs
        ]

        # visible buttons and interactive elements
        buttons = conn.execute(
            "SELECT role, name, enabled FROM elements "
            "WHERE offscreen=0 AND w>0 AND h>0 "
            "AND role IN ('Button','MenuItem','Hyperlink','TabItem') "
            "AND name IS NOT NULL AND name != '' "
            "ORDER BY y, x LIMIT 40"
        ).fetchall()
        state["buttons"] = [
            {"role": r["role"], "name": r["name"], "enabled": bool(r["enabled"])}
            for r in buttons
        ]

        # element counts
        total   = conn.execute("SELECT COUNT(*) FROM elements").fetchone()[0]
        visible = conn.execute(
            "SELECT COUNT(*) FROM elements WHERE offscreen=0 AND w>0 AND h>0"
        ).fetchone()[0]
        state["counts"] = {"total": total, "visible": visible}

        conn.close()
    except sqlite3.Error as e:
        state["db_error"] = str(e)

    # dialogs: look for any new window vs the main app
    wins = _current_windows()
    dialogs = [
        w["title"] for w in wins.values()
        if w.get("app") != app and w.get("exe") and
           any(k in w.get("title", "").lower()
               for k in ("save", "open", "dialog", "alert", "confirm",
                         "error", "warning", "password", "auth"))
    ]
    if dialogs:
        state["dialogs_detected"] = dialogs

    if extra_context:
        state.update(extra_context)

    return state


def _queue_inject(conn, action, text, target):
    """Insert an inject row and wait for the daemon to consume it."""
    cur = conn.cursor()
    cur.execute("SELECT name FROM sqlite_master WHERE type='table' AND name='inject'")
    if not cur.fetchone():
        raise RuntimeError("inject table missing")
    conn.execute(
        "INSERT INTO inject(action,text,target,done) VALUES(?,?,?,0)",
        (action, text, target),
    )
    conn.commit()
    # Poll at 50ms — daemon processes every 30ms so we catch it fast.
    # Hard cap at 4s; anything beyond that means the daemon is stuck.
    deadline = time.time() + 4.0
    while time.time() < deadline:
        row = conn.execute(
            "SELECT done FROM inject WHERE id=(SELECT MAX(id) FROM inject)"
        ).fetchone()
        if row and row[0] == 1:
            return {"status": "done"}
        time.sleep(0.05)
    return {"status": "queued", "note": "daemon did not consume it in time"}


def _do_inject(action, text, target):
    """Open the active db, inject, close."""
    db = active_db()
    if db is None:
        raise RuntimeError("no window snapped — call snap_window first")
    conn = sqlite3.connect(db, timeout=5)
    conn.execute("PRAGMA busy_timeout=3000")
    out = _queue_inject(conn, action, text, target)
    conn.close()
    return db, out


# ── Core tool implementations ─────────────────────────────────────────────────

def tool_list_windows(_args):
    raw = _read(WINDOWS_FILE)
    if not raw:
        return {"content": [{"type": "text", "text": "windows.json not found — is the daemon running?"}]}
    data = json.loads(raw)
    wins = [{k: w.get(k) for k in ("title", "app", "exe", "hwnd")}
            for w in data.get("windows", [])]
    age  = int(time.time()) - int(data.get("timestamp", 0))
    return {"content": [{"type": "text",
                         "text": json.dumps({"age_seconds": age, "windows": wins}, indent=1)}]}


def tool_snap_window(args):
    app = _arg(args, "app")
    if not app:
        raise ValueError("snap_window needs an app key from list_windows")
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
                out = {"status": "error", "reason": f"unparseable: {res!r}"}
            if out.get("status") == "ok":
                # wait for first tree dump
                deadline2 = time.time() + 3.0
                while time.time() < deadline2 and active_db() is None:
                    time.sleep(0.1)
                # return full UI state so the AI knows what it snapped to
                ui = _build_ui_state()
                return {"content": [{"type": "text",
                                     "text": json.dumps({"snapped": app, "ui": ui}, indent=1)}]}
            return {"content": [{"type": "text", "text": json.dumps(out)}]}
        time.sleep(0.15)
    return {"content": [{"type": "text",
                         "text": json.dumps({"status": "error", "reason": "timeout"})}]}


def tool_snap_dialog(args):
    """Detect and snap to a dialog that just appeared."""
    timeout_s = float(args.get("timeout_s", 4.0))
    hint      = _arg(args, "hint").lower()
    before    = _current_windows()
    deadline  = time.time() + timeout_s

    while time.time() < deadline:
        time.sleep(0.25)
        current    = _current_windows()
        new_hwnds  = set(current) - set(before)
        if not new_hwnds:
            continue
        for hwnd in sorted(new_hwnds):
            w = current[hwnd]
            title = w.get("title", "").lower()
            if hint and hint not in title:
                continue
            app = w.get("app", "")
            if not app:
                continue
            result = tool_snap_window({"app": app})
            snapped = {"snapped_app": app, "title": w.get("title", "")}
            return {"content": [{"type": "text",
                                  "text": json.dumps(snapped, indent=1) + "\n" +
                                          result["content"][0]["text"]}]}

    return {"content": [{"type": "text",
                          "text": json.dumps({
                              "status": "timeout",
                              "reason": f"No new window in {timeout_s}s"
                                        + (f' matching "{hint}"' if hint else ""),
                          })}]}


def tool_unsnap(_args):
    if active_db() is None:
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
    return {"content": [{"type": "text", "text": '{"status":"ok"}'}]}


def tool_get_ui_state(_args):
    ui = _build_ui_state()
    return {"content": [{"type": "text", "text": json.dumps(ui, indent=1)}]}


def tool_active_status(_args):
    db = active_db()
    if db is None:
        return {"content": [{"type": "text", "text": '{"snapped": false}'}]}
    app    = os.path.basename(db)[:-len(".db")]
    counts = {}
    try:
        conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True, timeout=3)
        counts["elements"]       = conn.execute("SELECT COUNT(*) FROM elements").fetchone()[0]
        counts["visible"]        = conn.execute(
            "SELECT COUNT(*) FROM elements WHERE offscreen=0 AND w>0 AND h>0"
        ).fetchone()[0]
        counts["inject_pending"] = conn.execute(
            "SELECT COUNT(*) FROM inject WHERE done=0"
        ).fetchone()[0]
        conn.close()
    except sqlite3.Error as e:
        counts["error"] = str(e)
    return {"content": [{"type": "text",
                          "text": json.dumps(
                              {"snapped": True, "app": app, "db": db,
                               "a11y_file": db[:-3] + ".a11y", "counts": counts},
                              indent=1)}]}


def tool_find_elements(args):
    db = active_db()
    if db is None:
        raise RuntimeError("no window snapped — call snap_window first")
    where, params = [], []
    if args.get("role"):
        where.append("role = ?"); params.append(str(args["role"]))
    if args.get("name"):
        where.append("name = ?"); params.append(str(args["name"]))
    if args.get("name_contains"):
        where.append("name LIKE ?"); params.append(f"%{args['name_contains']}%")
    if args.get("value_contains"):
        where.append("value LIKE ?"); params.append(f"%{args['value_contains']}%")
    if not args.get("offscreen"):
        where.append("offscreen = 0 AND w > 0 AND h > 0")
    limit = max(1, min(int(args.get("limit", 50)), 500))
    sql = ("SELECT id,parent_id,depth,role,name,value,automation_id,"
           "enabled,offscreen,x,y,w,h,actions FROM elements")
    if where:
        sql += " WHERE " + " AND ".join(where)
    sql += " ORDER BY y,x LIMIT ?"
    params.append(limit)
    deadline = time.time() + 3.0
    while True:
        try:
            conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True, timeout=3)
            conn.row_factory = sqlite3.Row
            rows = [dict(r) for r in conn.execute(sql, params)]
            conn.close()
            break
        except sqlite3.Error:
            if time.time() >= deadline:
                raise
            time.sleep(0.2)
    return {"content": [{"type": "text",
                          "text": json.dumps({"count": len(rows), "elements": rows}, indent=1)}]}


def tool_perform(args):
    """Smart action: do it, wait for UI to settle, return what changed."""
    action   = _arg(args, "action")
    text     = _arg(args, "text")
    target   = _arg(args, "target")
    settle_s = float(args.get("settle_s", 1.0))
    watch_dialogs = args.get("watch_dialogs", True)

    if not action:
        raise ValueError("perform needs an action")

    db = active_db()
    if db is None:
        raise RuntimeError("no window snapped — call snap_window first")

    before_wins = _current_windows() if watch_dialogs else {}

    conn = sqlite3.connect(db, timeout=5)
    conn.execute("PRAGMA busy_timeout=3000")
    inject_result = _queue_inject(conn, action, text, target)
    conn.close()

    if inject_result.get("status") != "done":
        return {"content": [{"type": "text",
                              "text": json.dumps({"status": "failed",
                                                  "inject": inject_result})}]}

    _, after_snap, changes = _wait_for_settle(db, settle_s)

    new_dialog = None
    if watch_dialogs:
        new_dialog = _detect_new_dialog(before_wins, timeout_s=0.3)

    # Lean response: changes + dialog hint + window title only.
    # AI can call get_ui_state() explicitly if it needs the full picture.
    result = {
        "status":  "done",
        "action":  action,
        "target":  target or None,
        "changes": changes if changes else ["(no visible changes)"],
    }
    # Add window title so AI knows if dialog appeared/closed
    try:
        conn2 = sqlite3.connect(f"file:{db}?mode=ro", uri=True, timeout=2)
        row = conn2.execute("SELECT value FROM meta WHERE key='window'").fetchone()
        conn2.close()
        if row:
            result["window"] = row[0]
    except sqlite3.Error:
        pass

    if new_dialog:
        result["new_dialog"] = {
            "title": new_dialog.get("title"),
            "app":   new_dialog.get("app"),
            "hint":  "call snap_dialog() or snap_window(app) to interact with it",
        }

    return {"content": [{"type": "text", "text": json.dumps(result, indent=1)}]}


def tool_wait_for_change(args):
    """Block until the UI changes or timeout, then return what changed."""
    timeout_s     = float(args.get("timeout_s", 5.0))
    watch_dialogs = args.get("watch_dialogs", True)

    db = active_db()
    if db is None:
        raise RuntimeError("no window snapped — call snap_window first")

    before_wins = _current_windows() if watch_dialogs else {}
    before_snap = _db_snapshot(db)
    deadline    = time.time() + timeout_s
    changed     = False

    while time.time() < deadline:
        time.sleep(0.3)
        current = _db_snapshot(db)
        if current != before_snap:
            changed = True
            break
        if watch_dialogs:
            if set(_current_windows()) - set(before_wins):
                changed = True
                break

    after_snap = _db_snapshot(db)
    changes    = _diff_snapshots(before_snap, after_snap)
    new_dialog = None
    if watch_dialogs:
        new_dialog = _detect_new_dialog(before_wins, timeout_s=0.0)

    result = {
        "changed":  changed,
        "changes":  changes if changes else ["(nothing changed)"],
        "ui":       _build_ui_state(db),
    }
    if new_dialog:
        result["new_dialog"] = {
            "title": new_dialog.get("title"),
            "app":   new_dialog.get("app"),
            "hint":  "call snap_dialog() or snap_window(app) to interact with it",
        }
    return {"content": [{"type": "text", "text": json.dumps(result, indent=1)}]}


def _action_with_context(action, text, target, settle_s=0.5):
    """Run inject then return lean context. Used by the simple action tools."""
    db, out = _do_inject(action, text, target)

    if out.get("status") != "done":
        return {"content": [{"type": "text", "text": json.dumps(out)}]}

    time.sleep(settle_s)

    # Check for new dialogs quickly
    wins = _current_windows()
    app  = os.path.basename(db)[:-3]

    result = {"status": "done"}

    # Window title — cheapest signal of what happened
    try:
        conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True, timeout=2)
        row  = conn.execute("SELECT value FROM meta WHERE key='window'").fetchone()
        conn.close()
        if row:
            result["window"] = row[0]
    except sqlite3.Error:
        pass

    # Surface suspicious new windows
    suspicious = [
        w for w in wins.values()
        if w.get("app") != app and
           any(k in w.get("title", "").lower()
               for k in ("save", "open", "dialog", "confirm", "error",
                         "warning", "password", "alert", "auth"))
    ]
    if suspicious:
        result["new_dialog"] = {
            "title": suspicious[0].get("title"),
            "app":   suspicious[0].get("app"),
            "hint":  "call snap_dialog() or snap_window(app) to interact with it",
        }

    return {"content": [{"type": "text", "text": json.dumps(result)}]}


def tool_click_element(args):
    el = _arg(args, "element")
    if not el:
        raise ValueError("click_element needs an element name")
    return _action_with_context("click", "", el)


def tool_type_text(args):
    text = _arg(args, "text")
    el   = _arg(args, "element")
    action = "text" if el else "type"
    return _action_with_context(action, text, el, settle_s=0.4)


def tool_press_key(args):
    combo = _arg(args, "combo")
    if not combo:
        raise ValueError("press_key needs a combo like 'ctrl+s'")
    return _action_with_context("key", combo, "", settle_s=0.6)


def tool_invoke_element(args):
    el = _arg(args, "element")
    if not el:
        raise ValueError("invoke_element needs an element name")
    return _action_with_context("invoke", "", el)


def tool_scroll(args):
    direction = _arg(args, "direction", "down")
    return _action_with_context("scroll", direction, "", settle_s=0.5)


def tool_set_clipboard(args):
    db, out = _do_inject("clipset", str(args.get("text", "")), "")
    return {"content": [{"type": "text", "text": json.dumps({"action": "clipset", **out})}]}


def tool_paste_text(args):
    db = active_db()
    if db is None:
        raise RuntimeError("no window snapped — call snap_window first")
    text = str(args.get("text", ""))
    el   = _arg(args, "element")
    conn = sqlite3.connect(db, timeout=5)
    conn.execute("PRAGMA busy_timeout=3000")
    if el:
        # Click to focus the field
        clicked = _queue_inject(conn, "click", "", el)
        if clicked.get("status") != "done":
            conn.close()
            raise RuntimeError(f"paste_text: could not click {el!r}")
        time.sleep(0.15)
        # Select-all so paste replaces existing content instead of appending
        _queue_inject(conn, "key", "ctrl+a", "")
        time.sleep(0.1)
    out = _queue_inject(conn, "paste", text, "")
    conn.close()
    return {"content": [{"type": "text",
                         "text": json.dumps({"action": "paste", "element": el, **out})}]}


def tool_get_clipboard(_args):
    db = active_db()
    if db is None:
        raise RuntimeError("no window snapped — call snap_window first")
    out_file = os.path.join(PROFILES, "clipboard_out")
    try:
        os.remove(out_file)
    except OSError:
        pass
    db, res = _do_inject("clipget", "", "")
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


def tool_get_notes(_args):
    path = _notes_path()
    content = _read(path)
    if content is None:
        return {"content": [{"type": "text", "text": f"notes file unreadable at {path}"}]}
    if not content.strip():
        return {"content": [{"type": "text",
                             "text": f"(no notes yet — {path} is empty)"}]}
    return {"content": [{"type": "text", "text": f"# from {path}\n\n{content}"}]}


def tool_append_note(args):
    situation = _arg(args, "situation")
    do        = _arg(args, "do")
    app       = _arg(args, "app") or "general"
    if not situation or not do:
        raise ValueError("append_note needs 'situation' and 'do'")
    stamp = time.strftime("%Y-%m-%d %H:%M")
    entry = (f"## [{stamp}] {app}\n"
             f"- Symptom: {situation}\n"
             f"- Instead: {do}\n")
    path = _prepend_note(entry)
    if path is None:
        raise RuntimeError("append_note: could not write notes file")
    return {"content": [{"type": "text",
                          "text": json.dumps({"status": "done", "path": path})}]}


# ── Dispatch ──────────────────────────────────────────────────────────────────

HANDLERS = {
    "list_windows":   tool_list_windows,
    "snap_window":    tool_snap_window,
    "snap_dialog":    tool_snap_dialog,
    "unsnap":         tool_unsnap,
    "get_ui_state":   tool_get_ui_state,
    "active_status":  tool_active_status,
    "find_elements":  tool_find_elements,
    "perform":        tool_perform,
    "wait_for_change":tool_wait_for_change,
    "click_element":  tool_click_element,
    "type_text":      tool_type_text,
    "press_key":      tool_press_key,
    "invoke_element": tool_invoke_element,
    "set_clipboard":  tool_set_clipboard,
    "paste_text":     tool_paste_text,
    "get_clipboard":  tool_get_clipboard,
    "scroll":         tool_scroll,
    "get_notes":      tool_get_notes,
    "append_note":    tool_append_note,
}


def dispatch(name, args):
    handler = HANDLERS.get(name)
    if handler is None:
        raise ValueError(f"unknown tool: {name!r}")
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
    elif method in ("notifications/initialized", "notifications/cancelled"):
        pass
    elif method == "ping":
        reply(msg_id, {})
    elif method == "tools/list":
        reply(msg_id, {"tools": TOOLS})
    elif method == "tools/call":
        params = req.get("params", {})
        name   = params.get("name", "")
        try:
            reply(msg_id, dispatch(name, params.get("arguments")))
        except Exception as e:
            reply(msg_id, {
                "content": [{"type": "text", "text": f"{type(e).__name__}: {e}"}],
                "isError": True,
            })
    elif method.startswith("notifications/"):
        pass
    elif msg_id is not None:
        reply(msg_id, error={"code": -32601, "message": f"unknown method {method!r}"})


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
