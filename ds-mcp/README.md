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

## Usage

Start the daemon first (see ../BUILD.md), then register the server with your
MCP client. Typical client config:

```json
{
  "mcpServers": {
    "directshell": {
      "command": "python3",
      "args": ["/home/harfho/DirectShell-Linux/ds-mcp/server.py"]
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
