# DirectShell MCP Server

A stdio MCP (Model Context Protocol) server that exposes DirectShell's window
snapping, accessibility tree, and input injection to any MCP client (Claude
Desktop, opencode, custom agents). Pure Python 3 stdlib — no dependencies.

## How it works

The daemon owns the X11 overlay and injection pipeline. This server is a thin
bridge over its file protocol in `ds_profiles/`:

| File | Direction | Purpose |
|---|---|---|
| `windows.json`   | daemon → us | enumerated windows (`app` keys, titles) |
| `snap_request`   | us → daemon | app key to snap, or `!unsnap` |
| `snap_result`    | daemon → us | `{"status":"ok",...}` or error |
| `is_active`      | daemon → us | first line = active db name when snapped |
| `<app>.db`       | daemon → us | SQLite `elements` table for the snapped window |
| `inject`         | us → daemon | rows `id\|action\|text\|target` (click/type/key/scroll/text) |

## Tools

- `list_windows` — enumerate top-level windows
- `snap_window {app}` — snap + wait for the first AT-SPI tree dump
- `unsnap` — detach from the current window
- `active_status` — what is snapped right now
- `find_elements {name?, role?, visible_only?}` — query the element table
- `invoke_element {element}` — activate an element's default action (press
  button, open menu, follow link) via AT-SPI — no coordinates needed;
  elements that support it have `actions > 0`
- `click_element {element}` — XTEST click at an element's center
- `type_text {text, element?}` — focus target then type raw keystrokes
- `set_clipboard {text}` / `get_clipboard` — native X11 CLIPBOARD access
- `paste_text {text, element?}` — clipboard + ctrl+v (fast bulk input)
- `press_key {combo}` — e.g. `ctrl+s`, `Return`, `alt+F4`
- `scroll {direction, amount?}` — `up`/`down`
- `get_notes` / `append_note {app?, situation, do}` — shared AI notes file
  (`~/.config/directshell/AI_NOTES.md`, falls back to this version's
  `ds_profiles/`): lessons other AI sessions logged — check first when
  something misbehaves, append when you discover a gotcha

## Design decisions

**Why `get_notes`/`append_note` instead of `ds_learn` + `tip_engine`?**

The upstream Windows version (IamLumae/DirectShell) uses `ds_learn()` which
persists to `ds_profiles/learnings/` and auto-injects lessons into tool
responses via `tip_engine` + `tip_miner.py`. That system is tightly coupled
to the Windows daemon's action logs and the tip_miner's pattern mining —
neither of which exist in this Linux port.

This port keeps a simpler model: a single shared notes file
(`~/.config/directshell/AI_NOTES.md`), read via `get_notes` at session start
and appended via `append_note` when a new gotcha is discovered. It works
without mining, without per-app indexing, and without requiring the daemon to
emit structured action logs. The tradeoff is that lessons are manual (the
model has to call `get_notes`) rather than auto-injected — but in practice the
model reads it reliably on startup.

If the Linux daemon later emits structured action logs, porting
`tip_miner.py` + `tip_engine` is straightforward and would replace this file
with automatic per-app injection. Until then, the notes file is the simplest
thing that works.

## Usage

Start the daemon first (see ../BUILD.md), then register the server with your
MCP client. Typical client config:

```json
{
  "mcpServers": {
    "directshell": {
      "command": "python3",
      "args": ["/home/%user%/DirectShell-Linux/ds-mcp/server.py"]
    }
  }
}
```

Or drive it manually over stdio — newline-delimited JSON-RPC 2.0:

```bash
echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}' | python3 server.py
```

Typical agent flow: `list_windows` → `snap_window {app}` → `find_elements` →
`click_element` / `type_text` / `press_key` → `unsnap`.

Notes:
- Only one window can be snapped at a time; snapping again re-targets.
- `type_text` sends real keyboard events to the focused widget — click an
  editable element first (or pass `element`) so the caret is where you expect.
  URL-shaped text without `element` is guarded: if the focused widget isn't
  editable, it's delivered via clipboard paste instead of keystrokes.
