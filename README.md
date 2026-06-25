# jdbg

**Agent-friendly Java debugger** CLI for Claude Code and humans, wrapping JDK `jdb` with persistent sessions, structured output, and native MCP tools.

## Highlights

- **Prompt-aware, not sleep-based** — reads jdb output until the prompt returns; never guesses with timeouts.
- **Stateful daemon** — a background process keeps debug sessions alive across CLI invocations.
- **Windows-first, cross-platform** — pure Rust, no Bash/WSL/temp-file dependencies.
- **Two access paths** — CLI (`jdbg <cmd>`) and MCP server (`jdbg __mcp`) for native tool calls from Claude Code.
- **Structured output** — human-readable text by default, `--json` for machine consumption.
- **Auto-enriched stop results** — breakpoint/step hits include source context and top stack frame automatically.
- **Conditional breakpoints** — filter high-traffic code with boolean expressions (e.g. `userId == 123`).
- **Thread breakpoints** — `suspend: "thread"` only holds the hit thread; heartbeat/ZK/Dubbo threads keep running (like IDEA's thread breakpoint).
- **Class/method search** — `classes` finds CGLIB proxies and runtime-generated classes; `methods` lists exact signatures for `break_in`.
- **Field watchpoints** — `watch` breaks on field access or modification (find out *who* changed a field and *when*).
- **Collection inspection** — `inspect` shows size + first N elements of any List/array/Map in one call.
- **Self-update** — `jdbg update` downloads the latest release and re-registers in one step.

## Get Started

### 1. Install the CLI

The installer fetches the right build for your OS/arch from the latest GitHub Release and adds `jdbg` to your `PATH`.

**macOS / Linux**

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/PieceOfFall/jdbg/releases/latest/download/java-agent-debugger-installer.sh | sh
```

**Windows (PowerShell)**

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/PieceOfFall/jdbg/releases/latest/download/java-agent-debugger-installer.ps1 | iex"
```

> The installer edits your **user-level** `PATH` (no admin needed). **Open a new terminal** afterwards so `jdbg` resolves.

<details>
<summary><b>Already have Rust?</b> Install via cargo or from source.</summary>

```bash
# via cargo (installs to ~/.cargo/bin/jdbg)
cargo install --git https://github.com/PieceOfFall/jdbg.git

# from source
git clone https://github.com/PieceOfFall/jdbg.git
cd jdbg
cargo build --release   # binary at target/release/jdbg
```

</details>

### 2. Register with Claude Code

One command wires `jdbg` into Claude Code as an MCP server:

```bash
jdbg setup
```

This writes the MCP server entry to `~/.claude.json` and an auto-allow permission (`mcp__jdbg__*`) to `~/.claude/settings.json`. **Restart Claude Code** to pick up the new server — its tools then appear as `mcp__jdbg__<tool>`.

```bash
jdbg setup --print    # preview the config snippet without writing anything
```

### 3. Start debugging

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

In Claude Code, just ask it to debug — it drives the same flow through the `mcp__jdbg__*` tools.

### Update to latest version

```bash
jdbg update
```

This removes the old registration, downloads and installs the latest release from GitHub, then re-registers. On Windows it handles the running-exe file lock automatically.

### Uninstall

```bash
jdbg setup --remove   # removes the MCP server entry and the permission; leaves the binary
```

## Requirements

- JDK 8–21+ with `jdb` on PATH or discoverable via `JAVA_HOME`
- Rust 1.85+ (edition 2024) — only for the `cargo`/from-source install methods
- For debugging: compile your Java code with `javac -g` (debug info for locals/line breakpoints)

## CLI Commands

```
# Session lifecycle
jdbg launch <MainClass> [--classpath CP] [--sourcepath SP] [--name N] [-- app-args...]
jdbg attach [--host H] [--port P] [--sourcepath SP] [--name N]
jdbg status | list | kill [--session ID]
jdbg daemon start | stop | status

# Breakpoints & watchpoints
jdbg break-at <Class> <line> [-c <condition>] [-s thread|all]
jdbg break-in <Class> <method> [--args types] [-c <condition>] [-s thread|all]
jdbg catch <Exception> [--mode caught|uncaught|all]
jdbg watch <Class.field> [--mode access|modification|all]
jdbg unwatch <Class.field>
jdbg breakpoints | clear <spec>

# Class/method search
jdbg classes [pattern]
jdbg methods <Class>

# Execution control
jdbg run | cont | step | next | step-out

# Inspection
jdbg where [--all] | locals | print <expr> | dump <obj> | eval <expr>
jdbg inspect <expr> [--max-elements N]
jdbg threads | thread <id> | frame <up|down> [n] | list-source [line]
jdbg raw <jdb command...>

# Setup & maintenance
jdbg setup [--remove] [--print]
jdbg update
--session <id>   target a specific session (defaults to the sole live one)
--json           machine-readable JSON output
--timeout <secs> override per-command timeout
--jdb-path <p>   explicit path to the jdb executable
```

## MCP Server (Claude Code native tools)

`jdbg __mcp` runs a stdio JSON-RPC 2.0 MCP server, exposing the CLI surface as **30 native tools**
(`launch`, `break_at`, `run`, `locals`, `cont`, `inspect`, …) so Claude Code can drive a debug session
without going through Bash. Tools appear as `mcp__jdbg__<tool>`.

`jdbg setup` ([Get Started step 2](#2-register-with-claude-code)) wires this up for you. To configure it
manually instead — or to point at a **dev build** while hacking on jdbg itself:

```json
{
  "mcpServers": {
    "jdbg": { "command": "target/debug/jdbg", "args": ["__mcp"] }
  }
}
```

The repo ships `.mcp.json` (dev) and `.claude-plugin/plugin.json` (distribution) wiring this up.

## Architecture

Two clients → one daemon → N × jdb child processes:

```
     CLI (jdbg)  ─┐                           ┌─ jdb child A → JVM A
                  ├─► Daemon (SessionManager) ┤
  MCP (jdbg __mcp)┘    named pipe / socket    └─ jdb child B → JVM B
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
cargo test           # 84 unit + 18 integration tests (parser, reader, MCP tools, session, classes/methods, watch, e2e)
```

The parser is validated against captured real-jdb transcripts under `tests/fixtures/jdb/`. Pure logic
(parser, protocol mapping, JSON-RPC) follows TDD; platform side-effects are verified by end-to-end runs.

## License

Licensed under the [Apache License 2.0](LICENSE).
