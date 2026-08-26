# DirectShell for Linux

Linux port of DirectShell using AT-SPI2 over D-Bus.

## Overview

This is a working Linux port of DirectShell that replaces Windows UIA accessibility with Linux AT-SPI2 accessibility via D-Bus. It covers window snapping, overlay injection, AT-SPI2 tree traversal, and XTEST input — but not every Windows feature has a Linux equivalent yet.

## Files

1. **src/main.rs** - Main application with AT-SPI2 integration
2. **Cargo.toml** - Project dependencies
3. **README.md** - Usage instructions
4. **ds-mcp/** - Python-based MCP server for AI agents

## Key Features

- AT-SPI2 accessibility support for Linux
- Window snapping and overlay functionality
- File generation (.snap, .a11y, .a11y.snap)
- SQLite database storage
- MCP server for AI agents (stdio JSON-RPC; see ds-mcp/README.md)

## Dependencies

- Rust (1.70+)

## Build Instructions

```bash
# Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Build
cargo build --release
```

## Notes

The Linux port is complete and live-tested: window enumeration, overlay
snapping, AT-SPI2 tree dumps into SQLite, and XTEST input injection all work
against real applications (see `STATUS.md` for the verified matrix and
`BUILD.md` for build/run instructions). It talks to X11 and D-Bus directly
via `x11rb` and `zbus`, so no AT-SPI2 development libraries are needed.