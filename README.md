# jdbg

Cross-platform CLI debugger for Java — wraps JDK's `jdb` with **prompt-aware** control so AI coding agents (and humans) can debug Java programs interactively.

## Highlights

- **Prompt-aware, not sleep-based** — reads jdb output until the prompt returns; never guesses with timeouts.
- **Stateful daemon** — a background process keeps debug sessions alive across CLI invocations.
- **Windows-first, cross-platform** — pure Rust, no Bash/WSL/temp-file dependencies.
- **Two access paths** — CLI (`jdbg <cmd>`) and MCP server (`jdbg __mcp`) for native tool calls from Claude Code.
- **Structured output** — human-readable text by default, `--json` for machine consumption.

## Installation

Every install method drops `jdbg` (or `jdbg.exe` on Windows) onto your `PATH`, so the
MCP plugin's bare `jdbg` command resolves on every platform — no per-OS binary name to configure.

### Prebuilt binaries (recommended)

The installers fetch the right build for your OS/arch from the latest GitHub Release and add it to `PATH`.

**macOS / Linux**

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/PieceOfFall/jdbg/releases/latest/download/java-agent-debugger-installer.sh | sh
```

**Windows (PowerShell)**

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/PieceOfFall/jdbg/releases/latest/download/java-agent-debugger-installer.ps1 | iex"
```

### Via cargo

```bash
cargo install --git https://github.com/PieceOfFall/jdbg.git
# Installs to ~/.cargo/bin/jdbg
```

### From source

```bash
git clone https://github.com/PieceOfFall/jdbg.git
cd jdbg
cargo build --release
# Binary at target/release/jdbg (or jdbg.exe on Windows)
```

### Requirements

- JDK 8–21+ with `jdb` on PATH or discoverable via `JAVA_HOME`
- Rust 1.85+ (edition 2024) — only for the `cargo`/from-source methods
- For debugging: compile your Java code with `javac -g` (debug info for locals/line breakpoints)

## Quick Start

```bash
# Compile the target program with debug info
javac -g Main.java

# Launch a debug session (daemon auto-starts)
jdbg launch Main --classpath .

# Set a breakpoint and run
jdbg break-at Main 9
jdbg run

# Inspect state
jdbg locals
jdbg where
jdbg print myVar

# Step and continue
jdbg step
jdbg cont

# Clean up
jdbg kill
jdbg daemon stop
```

## CLI Commands

```
# Session lifecycle
jdbg launch <MainClass> [--classpath CP] [--sourcepath SP] [--name N] [-- app-args...]
jdbg attach [--host H] [--port P] [--sourcepath SP] [--name N]
jdbg status | list | kill [--session ID]
jdbg daemon start | stop | status

# Breakpoints
jdbg break-at <Class> <line>
jdbg break-in <Class> <method> [--args types]
jdbg catch <Exception> [--mode caught|uncaught|all]
jdbg breakpoints | clear <spec>

# Execution control
jdbg run | cont | step | next | step-out

# Inspection
jdbg where [--all] | locals | print <expr> | dump <obj> | eval <expr>
jdbg threads | thread <id> | frame <up|down> [n] | list-source [line]
jdbg raw <jdb command...>

# Global flags
--session <id>   target a specific session (defaults to the sole live one)
--json           machine-readable JSON output
--timeout <secs> override per-command timeout
--jdb-path <p>   explicit path to the jdb executable
```

## MCP Server (Claude Code native tools)

`jdbg __mcp` runs a stdio JSON-RPC 2.0 MCP server, exposing the CLI surface as **25 native tools**
(`launch`, `break_at`, `run`, `locals`, `cont`, …) so Claude Code can drive a debug session without
going through Bash. Tools appear as `mcp__jdbg__<tool>`.

During development, point your MCP config at the dev binary:

```json
{
  "mcpServers": {
    "jdbg": { "command": "target/debug/jdbg", "args": ["__mcp"] }
  }
}
```

For production (after installing `jdbg` to PATH), use the bare command:

```json
{
  "mcpServers": {
    "jdbg": { "command": "jdbg", "args": ["__mcp"] }
  }
}
```

The repo ships `.mcp.json` (dev) and `.claude-plugin/plugin.json` (distribution) wiring this up.

## Architecture

Two clients → one daemon → N × jdb child processes:

```
    CLI (jdbg)  ─┐                          ┌─ jdb child A → JVM A
                 ├─► Daemon (SessionManager) ┤
  MCP (jdbg __mcp)┘    named pipe / socket   └─ jdb child B → JVM B
```

- **CLI / MCP server** — two peer clients; each turns its input into a `Request` and sends it to the daemon
  via `client::send_request`.
- **Daemon** (`jdbg __daemon`, auto-spawned) — owns the IPC listener and a `HashMap<SessionId, Session>`,
  multiplexing N concurrent sessions.
- **jdb engine** (`src/jdb/`) — spawns `jdb` with forced English locale and piped stdio, reads byte-wise
  until the prompt, and parses output into structured results with regex (validated against captured jdb
  transcripts).

Layered, dependency flows downward only:

```
  bin (main.rs) → cli / output → client / daemon → session → jdb / jdkpath → error / protocol / registry
```

See [`DESIGN.md`](DESIGN.md) for the full design reference (Chinese).

## Building & Testing

```bash
cargo build          # debug build
cargo build --release
cargo test           # 37 unit tests (parser, protocol mapping, MCP tools, session)
```

The parser is validated against captured real-jdb transcripts under `tests/fixtures/jdb/`. Pure logic
(parser, protocol mapping, JSON-RPC) follows TDD; platform side-effects are verified by end-to-end runs.

## License

Licensed under the [Apache License 2.0](LICENSE).
