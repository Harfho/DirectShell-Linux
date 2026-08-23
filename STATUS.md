# DirectShell Linux — Implementation Status

**Status: WORKING — live-tested end-to-end on X11 (Linux Mint 22.3 / xfwm4).**

## Verified working (live tests)

| Feature | Evidence |
|---|---|
| Window enumeration | `ds_profiles/windows.json` rewritten every 2 s from `_NET_CLIENT_LIST` |
| Snap to window | overlay shapes itself over target, tracks move/resize (`SYNC` loop) |
| A11y tree dump | real elements streamed into `<app>.db` every 500 ms (e.g. 618 rows for xed) |
| `.snap` interactive subset | role-mapped, offscreen/zero-size filtered |
| inject `text` | AT-SPI EditableText `SetTextContents` — text appeared in the live document |
| inject `click` | element lookup by name → UIA-style **role fallback**, clicks via XTEST |
| inject `type` | re-click LAST_CLICK then XTEST chars — landed in the focused document |
| inject `key` | `ctrl+s` delivered via XTEST — xed saved the file to disk |
| inject `scroll` | button 4/5 events at pointer |

End-to-end proof: after queueing `text`/`click`/`type`/`key(ctrl+s)` rows in
`inject`, `~/Documents/prove.txt` contained the injected strings on disk.

## Key implementation notes (Linux-specific)

- **Overlay must never take keyboard focus.** It is created with
  `WM_HINTS.input = False`; `activate_target()` additionally calls
  `SetInputFocus` on the target so XTEST keys land there.
- **XTEST event types differ per device**: KeyPress=2, KeyRelease=3,
  ButtonPress=4, ButtonRelease=5, Motion=6. Using the button constants for
  keyboard events makes the server silently drop them (this was a real bug).
- **AT-SPI2 quirks**:
  - Never use zbus property cache (`GetAll`) — the registry replies with an
    empty body; call `org.freedesktop.DBus.Properties.Get` explicitly.
  - `GetState` returns an `au` array; bit 30 is VISIBLE (25 = SHOWING,
    7 = EDITABLE, 8 = ENABLED, 24 = SENSITIVE). Hidden views report
    `i32::MIN` extents.
  - GTK apps expose many unnamed widgets: element lookup order is
    exact-name (menus excluded) → role ("Edit", "Button", …) → menus as last
    resort. For ambiguous text roles the *largest visible* candidate wins
    (that's the content area).
- **Concurrency**: never hold the X mutex across blocking calls (event loop
  polls with `poll_for_event`); never hold the A11Y mutex across D-Bus calls;
  never call `lx!` from inside an `lx!` closure (non-reentrant mutex).
- **Inject purge race**: pending `inject` rows are deleted only once per snap,
  not on every tree-dump tick.

## Runtime layout

- `~/.cargo/bin/directshell-linux` — installed binary
- Launch: `DISPLAY=:0.0 setsid nohup directshell-linux &`
- `directshell.log` is a 100-line ring buffer rewritten wholesale; stdout
  (nohup.out) keeps full history.
- Agent protocol files under `ds_profiles/`: `windows.json`, `snap_request`,
  `snap_result`, `is_active`, `<app>.db` (+`.a11y`, `.snap`), `inject` table.
