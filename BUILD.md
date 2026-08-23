# DirectShell Linux — Build

## Requirements

- Rust (stable, user-local install is fine: `~/.cargo`)
- Linux with X11 (X11RB talks to the server directly; no X dev headers needed)
- AT-SPI2 runtime (present on all GTK desktops; no dev libraries required)

## Dependencies (Cargo.toml)

- `rusqlite 0.31` (bundled SQLite)
- `serde 1`
- `x11rb 0.14` with features `shape`, `xtest`
- `zbus 5` with `default-features = false`, features `async-io`, `blocking-api`
- `libc 0.2`

## Build & install

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cd DirectShell-Linux
cargo build --release
cargo install --path .     # installs to ~/.cargo/bin/directshell-linux
```

## Run

```bash
DISPLAY=:0.0 setsid nohup ~/.cargo/bin/directshell-linux \
    </dev/null >>ds_profiles/nohup.out 2>&1 &
```

Use `setsid` so a terminal timeout cannot kill the daemon.

Stop it:

```bash
kill $(cat ds_profiles/directshell.lock)
```

(`pkill -x` does not work — the name exceeds 15 characters.)

## Notes

- On first start the daemon enables toolkit accessibility
  (`org.gnome.Accessibility` gsetting) and connects to the AT-SPI2 bus.
  Applications launched **after** that point expose accessibility trees.
- The daemon must run on the machine with the target X display.

## Test

Start the daemon, open a GUI app (e.g. `xed --new-window /tmp/prove.txt`),
then run the smoke test:

```bash
python3 scripts/smoke_test.py              # lists windows
python3 scripts/smoke_test.py prove.txt    # snap → click → type → ctrl+s → unsnap
```

It drives the MCP server exactly like an external agent would. A passing run
ends with the typed text visible in the target app and saved to disk.

## Portability (other distros)

Nothing is Mint-specific. It should work on any mainstream distro given:

- **X11 session** — Xorg or XWayland. The overlay, XTEST injection and window
  enumeration are all X11; pure-Wayland sessions will not work.
- **AT-SPI2 + D-Bus** — standard on GNOME/KDE/Cinnamon/XFCE/MATE.
  GTK, Mozilla and Electron apps expose trees out of the box; Qt apps need
  `qt-at-spi`; GNOME sometimes needs
  `gsettings set org.gnome.desktop.interface toolkit-accessibility true`.
- **EWMH-compliant window manager** — nearly all are (`_NET_ACTIVE_WINDOW`,
  client lists).
- **Build deps** — cargo + a C compiler for bundled SQLite; the MCP side is
  stdlib-only Python 3.

## What to expect on other setups

Verified behavior (on Mint 22.3 / Cinnamon / X11): snap overlay, full AT-SPI
tree dumps to SQLite (~450–550 ms for ~500 elements), AT-SPI `EditableText`
writes into GTK text views, XTEST click/type/key/scroll, ctrl+s actually
saving files, and the MCP tools end-to-end.

Expect variation elsewhere:

- **Dump latency scales with app size** — Firefox/Chromium produce thousands
  of elements; first dump after a snap can take seconds. The MCP server waits
  up to 3 s for readiness.
- **Only one window is snapped at a time** — snapping again re-targets;
  `!unsnap` detaches. Another process writing `snap_request` supersedes you.
- **Widget focus matters for typing** — XTEST keystrokes go to the focused
  widget; click an editable element first (or pass `element:` to type_text).
- **Window title collisions** — two windows with identical titles map to the
  same db key; the daemon snaps whichever it enumerates first.
- **Tiling WMs** — untested; floating geometry/raise behavior may differ.
- **HiDPI / multi-monitor** — coordinates are global screen pixels; scaling
  factors above 1x are untested.
- **Session restore quirks** — editors like xed may reopen previous buffers
  or show "file changed on disk" bars after external writes; prefer fresh
  windows/files when testing.

