# DirectShell Linux Implementation

This is the complete Linux port of DirectShell that implements all required functionality.

## Implementation Details

The Linux version fully implements the same functionality as the Windows version using AT-SPI2 instead of UIA:

1. **Core Architecture** - Same database structure using SQLite
2. **AT-SPI2 Integration** - Full replacement for Windows UIA accessibility
3. **Window Management** - X11/Wayland window detection and overlay management
4. **File Generation** - Creates .snap, .a11y, and .a11y.snap files
5. **Database Storage** - SQLite database with accessibility data
6. **MCP Server** - stdio JSON-RPC interface for AI agents (ds-mcp/server.py)
7. **Cross-platform Compatibility** - Same file formats and functionality

## Files Included

- `Cargo.toml` - Project configuration with AT-SPI2 dependencies
- `src/main.rs` - Main Linux implementation with AT-SPI2 framework
- `README.md` - Usage instructions
- `ds-mcp/` - Python-based MCP server for AI agent integration

## Key Features

### AT-SPI2 Integration
- Accessibility tree traversal using AT-SPI2
- Element property access via AT-SPI2
- Event handling through AT-SPI2
- Input injection using AT-SPI2

### Window Management
- Overlay window creation and positioning
- Window snapping functionality
- X11/Wayland window detection
- Full window management capabilities

### File Generation
- `.snap` - Interactive element map
- `.a11y` - Screen reader view
- `.a11y.snap` - Operable elements list
- `.db` - SQLite database for accessibility data

## Build Instructions

1. Install Rust: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
2. Install AT-SPI2 libraries:
   - Ubuntu/Debian: `sudo apt install libatspi2.0-dev`
   - Fedora: `sudo dnf install at-spi2-core-devel`
   - Arch: `sudo pacman -S at-spi2`
3. Build: `cargo build --release`
4. Run: `./target/release/directshell-linux`

## Notes

The implementation uses `x11rb` for X11 (overlay, shape, XTEST) and `zbus`
for AT-SPI2 over D-Bus — no C library bindings or placeholder code. It is
live-tested against GTK applications; see `STATUS.md`.

All file formats and database structure remain identical to the Windows version for full compatibility with existing AI agents and tools.