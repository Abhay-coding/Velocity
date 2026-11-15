# Velocity Search

A lightning-fast desktop file search application for Windows, built with Rust and Slint.

## Overview

Velocity Search provides instant file search capabilities using the Everything search engine backend. It features a clean, minimal UI with keyboard-driven workflow for power users.

![Velocity Search](docs/screenshot.png)

## Features

- **Instant Search**: Sub-millisecond search results using Everything's indexed database
- **Keyboard-First**: Global hotkey (Alt+Space) to open, type to search, Enter to open, Esc to close
- **Clean UI**: Minimal, distraction-free interface built with Slint
- **File Preview**: Quick preview of file metadata (size, modified date, type)
- **Cross-Platform Core**: Rust core with Windows-specific UI (extensible to other platforms)
- **MFT Parser (Learning)**: NTFS Master File Table parser for future native indexing

## Architecture

```
velocity/
├── velocity-core/      # Core library: config, error handling, index provider traits
├── velocity-ui/        # Slint-based desktop UI (Windows)
├── velocity-everything/# Everything CLI adapter (production backend)
└── velocity-mft/       # NTFS MFT parser (learning project, not production)
```

### Backend

The production backend uses **Everything CLI** (`es.exe`) from voidtools.com, which provides:
- NTFS USN Journal-based real-time indexing
- Instant search across all local drives
- Advanced query syntax (operators, wildcards, regex)
- Minimal resource usage

### Future Backend

The `velocity-mft` crate is a learning project implementing direct NTFS MFT parsing for a future native Rust backend that won't require external dependencies.

## Requirements

- **Windows 10/11** (UI currently Windows-only)
- **Everything Search** installed from [voidtools.com](https://www.voidtools.com/)
- **Rust 1.75+** (for building from source)

## Installation

### From Release (Recommended)

Download the latest release from the [releases page](#) (when available).

### From Source

```bash
# Clone the repository
git clone https://github.com/yourusername/velocity.git
cd velocity

# Install Everything CLI (es.exe) - required for search functionality
# Download from https://www.voidtools.com/ and place es.exe in:
# - Same directory as velocity executable, OR
# - C:\Program Files\Everything\es.exe, OR
# - D:\Everything\es.exe

# Build release version
cargo build --release

# Run
./target/release/velocity-ui.exe
```

## Usage

1. **Launch** the application (runs in background)
2. **Press Alt+Space** to open the search window
3. **Type** your search query
4. **Navigate** results with arrow keys
5. **Press Enter** to open file/folder
6. **Press Esc** to close window

### Search Syntax

Velocity inherits Everything's powerful query syntax:

| Syntax | Example | Description |
|--------|---------|-------------|
| `text` | `report` | Search by name |
| `ext:` | `ext:pdf` | Filter by extension |
| `path:` | `path:src` | Filter by path |
| `size:` | `size:>10mb` | Filter by size |
| `dm:` | `dm:lastweek` | Filter by date modified |
| `regex:` | `regex:\d{4}-\d{2}` | Regular expression |

See [Everything Search Syntax](https://www.voidtools.com/support/everything/search_syntax/) for complete reference.

## Configuration

Configuration file location:
- Windows: `%APPDATA%\velocity\config.toml`

Example `config.toml`:
```toml
[ui]
max_results = 100
hover_delay_ms = 150

[hotkey]
key = "Alt+Space"
```

## Development

### Testing the MFT Parser

The `velocity-mft` crate includes a test example that validates the parser against a real NTFS volume (C: drive):

```bash
# Requires Administrator privileges to access raw volume
# Open PowerShell as Administrator first, then:
cd velocity
cargo run -p velocity-mft --example test_mft
```

This test:
1. Opens `\\.\C:` for raw MFT access (read-only)
2. Parses and displays boot sector info (bytes/sector, sectors/cluster, MFT location)
3. Reads and displays MFT records
4. Tests path building from parent references
5. Initializes the full IndexProvider and performs sample searches

**Note**: The full drive indexing in step 5 scans the entire MFT and can take 10+ minutes on large drives. Press Ctrl+C to stop after verifying the core parser works (steps 1-4 complete in seconds).

### Project Structure

```
velocity/
├── Cargo.toml              # Workspace manifest
├── .gitignore
├── velocity-core/
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs          # Core exports
│       ├── config.rs       # Configuration management
│       ├── error.rs        # Error types
│       └── index_provider.rs # Index provider trait
├── velocity-ui/
│   ├── Cargo.toml
│   ├── build.rs            # Slint build script
│   ├── ui/
│   │   └── ui.slint        # Slint UI definition
│   └── src/
│       └── main.rs         # UI entry point
├── velocity-everything/
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs
│       └── cli_adapter.rs  # Everything CLI adapter
└── velocity-mft/
    ├── Cargo.toml
    └── src/
        └── lib.rs          # MFT parser (learning)
```

### Building

```bash
# Debug build
cargo build

# Release build
cargo build --release

# Run tests
cargo test

# Check formatting
cargo fmt --check

# Run clippy
cargo clippy
```

### Running the UI

```bash
cargo run --bin velocity-ui
```

## Dependencies

### Runtime
- **Everything** (voidtools.com) - Required for search functionality

### Build (Rust Crates)
- `slint` - UI framework
- `tokio` - Async runtime
- `tracing` - Logging
- `chrono` - Date/time handling
- `rdev` - Global hotkey listener
- `windows-rs` - Windows API bindings
- `anyhow`, `thiserror` - Error handling
- `serde` - Serialization

## Roadmap

- [ ] Native MFT-based backend (remove Everything dependency)
- [ ] Linux/macOS support with platform-specific backends
- [ ] Content search (file contents)
- [ ] Plugin system for custom indexers
- [ ] Dark/Light theme support
- [ ] Portable mode (config in exe directory)
- [ ] Auto-update mechanism

## Contributing

1. Fork the repository
2. Create a feature branch
3. Make your changes
4. Run `cargo fmt`, `cargo clippy`, `cargo test`
5. Submit a pull request

## License

MIT License - see [LICENSE](LICENSE) for details.

## Acknowledgments

- [Everything Search Engine](https://www.voidtools.com/) by voidtools - The incredible search backend
- [Slint](https://slint.dev/) - The UI framework
- [Rust](https://www.rust-lang.org/) - The systems programming language

## Support

- **Issues**: [GitHub Issues](#)
- **Discussions**: [GitHub Discussions](#)
- **Everything Help**: [voidtools.com/support](https://www.voidtools.com/support/)