# CLAUDE.md — java-agent-debugger (`jdbg`)

> Coding charter for AI agents working in this repo. **Rules and constraints only.**
> For architecture, module layout, command/tool surface, output schema, dependencies, and status,
> read **`DESIGN.md`** (the factual design-and-implementation reference). This file does NOT duplicate it.

## What this project is (one paragraph)

`jdbg` is a cross-platform **Rust CLI** (binary `jdbg`, crate `java-agent-debugger`, edition 2024) that lets an
AI agent **debug Java interactively** by wrapping the JDK's `jdb` — prompt-aware (never sleep-based), stateful
(a background daemon keeps sessions alive across calls), Windows-first. It is consumed two ways: the **CLI** and
an **MCP server** (`jdbg __mcp`, native tool calls for Claude Code, Codex, and OpenCode). Full detail in `DESIGN.md`.

## Binding constraints (do not violate without asking)

These are settled decisions. Changing them needs explicit user sign-off.

- **Threads, NOT tokio.** Concurrency is `std::thread` + channels + blocking IO. The concurrency is tiny and
  bounded; an async runtime is unjustified. This applies to the MCP server too (hand-written stdio JSON-RPC).
- **No temp files, no shell, no sleeps.** Commands are written straight to `jdb`'s stdin. Readiness is detected
  by reading until the prompt, never by sleeping. There is no shell involved anywhere (no injection surface).
- **Minimal dependencies.** Do not add a crate when `std` suffices. Deliberately excluded: `tokio`, `rmcp`
  (pulls tokio), `once_cell`, `nix`, `daemonize`, `portable-pty`/`conpty`, any TCP/RPC framework, any
  windows/winapi crate (use `std` raw FFI for the few Win32 calls). Use `std::sync::LazyLock` for one-time
  regex compilation and `std::os::*` for platform bits.
- **One daemon per user; one in-flight command per session.** The per-session command `Mutex` is held across
  write+wait — `jdb` is line-oriented and cannot interleave commands. Different sessions run in parallel.
- **The daemon is the single writer** of the on-disk registry (atomic temp-in-same-dir + rename). The CLI only
  reads it (offline fallback). The in-memory session map is the source of truth while the daemon is alive.
- **Keep modules small and single-purpose.** If a file grows large it is doing too much — split it.
- **Use English for code comments and commit messages.** Source/test comments, Rustdoc, git commit messages, and
  PR/review comments written for the project history should be in English.

## jdb engine contract (the riskiest code — get these exactly right)

- **MANDATORY locale flags.** Always spawn `jdb` with `-J-Duser.language=en -J-Duser.country=US
  -J-Dfile.encoding=UTF-8`. On this machine `jdb` otherwise emits localized (Chinese) event banners
  (`Breakpoint hit:`, `Step completed:`, `Exception occurred:`, `The application exited`) that will NOT match
  the English regexes. Prompt detection is locale-independent (primary signal); forcing English makes the event
  banners reliable (secondary signal). **Omitting these flags silently breaks parsing.**
- **en_US thousands separator in line numbers.** Forcing US English locale causes jdb to format line numbers
  ≥1000 with comma separators (`line=3,956`). All line-number regexes use `[\d,]+` and strip commas before
  parsing to `u32`. This affects `RE_BREAKPOINT_OR_STEP`, `RE_FIELD_WATCH`, `RE_SOURCE_LINE`, and
  `parse_location_parens`. **Never use bare `\d+` for line numbers in jdb output.**
- **Timeout clears the buffer.** `read_until_prompt` uses `take_text()` (not `.clone()`) on timeout, so
  subsequent commands start with a clean buffer. Stale data in the buffer after timeout was a critical bug
  causing output misalignment and wrong line-number captures.
- **Normal commands always purge.** `execute()` calls `purge_pending()` before any Normal command regardless
  of current `RunState`. This clears channel residue from late-arriving bytes after timeouts or events.
- **"Nothing suspended." fast return.** In Blocking mode, if jdb responds with "Nothing suspended." + bare
  prompt (VM was already running, `cont`/`resume` is a no-op), the reader returns immediately rather than
  waiting for the full blocking timeout.
- **Attach deduplication.** `create_attach` rejects connections to a target (host:port) that already has a
  live session. Two jdb clients on the same JDWP port interfere (one's `kill` sends `resume` that unfreezes
  the other's breakpoint).
- **Piped stdio, not ConPTY.** Plain `std::process::Command` with piped stdin/stdout/stderr. ConPTY injects
  ANSI/cursor escapes that are harder to parse. (Keep ConPTY only as a documented future fallback if some JDK
  withholds the prompt on a pipe.)
- **Read byte-wise into a rolling buffer; match the prompt at the tail.** The prompt has no trailing newline and
  one read may not be a full line. Normalize `\r\n` → `\n` and decode UTF-8 lossy before matching.
- **Timeout is non-destructive — never kill on timeout.** Return a `Timeout` result with partial output and mark
  the session `Running` (deadlock/long-loop case); leave it alive so the agent can inspect or kill. (The Bash
  reference kills on timeout; we deliberately do not.)
- **Blocking vs normal commands.** Normal (`locals`/`where`/`print`/…): any prompt ends it (~15s default).
  Blocking (`run`/`cont`/`step`/`next`/`step up`): the prompt does not return until a breakpoint/exception/step
  /VM-exit — watch terminal marker → event marker → bare prompt, in that priority (~30s default, `--timeout`
  overridable).
- **The parser regexes are the authoritative contract** and live in `src/jdb/parser.rs` / `reader.rs`
  (compiled once via `LazyLock`). Treat captured real jdb transcripts as the test oracle — that is where
  correctness lives. Do not loosen a regex without a transcript proving the new shape.

## MCP server rules

- **stdout carries ONLY JSON-RPC.** Every log/diagnostic goes to stderr (`eprintln!`). A stray `println!` (or a
  library writing to stdout) corrupts the protocol stream. Audit any new code on the `run_mcp` path.
- **Protocol vs tool errors.** Unknown tool / missing required param / bad JSON → JSON-RPC error
  (`-32601`/`-32602`/`-32700`). Business failures (session dead) and daemon-connect failures →
  tool-level error (`isError:true`) so Claude sees a message and can continue.
- **The MCP layer is a thin client of the daemon** — it reuses `client::send_request` + `output::render` and
  must not reach around them into session/jdb internals. Keep `src/mcp/tools.rs` the single MCP↔`Command`
  mapping point.

## Setup / agent registration rules

- **`jdbg setup` is multi-agent.** First-class setup targets are `claude`, `codex`, `opencode`, and `pi`; `--target`
  accepts `claude,codex,opencode,pi`, `auto`, `all`, or `none`, and `--yes` must make setup non-interactive.
- **Claude Code target:** write only `mcpServers.jdbg` in `~/.claude.json`, `mcp__jdbg__*` in
  `~/.claude/settings.json`, and the embedded MCP skill to `~/.claude/skills/jdbg/SKILL.md`.
- **Codex target:** write only `[mcp_servers.jdbg]` in `~/.codex/config.toml` and the embedded MCP skill to
  `~/.codex/skills/jdbg/SKILL.md`. Do not invent a Codex permissions surface.
- **OpenCode target:** write only `mcp.jdbg` in `~/.config/opencode/opencode.json` and the embedded MCP skill to
  `~/.config/opencode/skills/jdbg/SKILL.md`. Do not invent an OpenCode permissions surface.
- **Pi target:** write only the embedded CLI skill to `~/.pi/agent/skills/jdbg/SKILL.md`. Do not invent a Pi
  MCP surface.
- **Removal is surgical.** `setup --remove` removes only jdbg-owned MCP entries, permissions, and skill dirs;
  preserve sibling servers, settings, TOML tables, and user-authored content.
- **`jdbg update` preserves prior targets.** Detect which targets already have jdbg configured before removal,
  then after installing the new binary re-run setup for that same target list. If none are configured, fall back
  to Claude for backward compatibility.

## Environment gotchas (affect correctness on this machine)

- **Minimum JDK 8** (must also work on 9–21+). User `JAVA_HOME` = `C:\Users\luyiwen\.jdks\azul-1.8.0_492`.
- **Prefer `JAVA_HOME` over PATH** when locating `jdb` — on this machine PATH resolves to JDK 21 but the target
  is the JDK 8 at `JAVA_HOME`. Discovery order: `--jdb-path` → `JAVA_HOME/bin` → PATH → common install dirs.
- **`-g` for locals/line breakpoints.** If the parser sees "Local variable information not available", set a
  `note` advising `javac -g` — still succeed.
- **JDWP attach syntax is JDK-version-aware.** `address=*:5005` is **JDK 9+ only**; on **JDK 8** use
  `address=5005` or `address=localhost:5005`. Attach uses the socket connector
  (`-connect com.sun.jdi.SocketAttach:hostname=H,port=P`), **not** `jdb -attach host:port` — on Windows the
  latter defaults to shared-memory (dt_shmem) and fails against a dt_socket JDWP target.

## CI / Release

Release builds use **cargo-dist** + GitHub Actions (`.github/workflows/release.yml`).

**Trigger:** pushing a **git tag** matching `**[0-9]+.[0-9]+.[0-9]+*` (e.g. `v0.7.0`). A plain branch
push does NOT trigger a release build. PRs trigger a plan-only dry-run (no publish).

**Release checklist:**
1. `cargo test` passes (all unit + integration).
2. **Check `skills/jdbg/mcp/SKILL.md` and `skills/jdbg/cli/SKILL.md`** — if any tool was added/removed/renamed, parameters changed, or
   behavior semantics changed (e.g. new fields in responses, new notes), update the skill file:
   `allowed-tools` list, tool reference table, "Reading results" section, "Common mistakes", etc.
3. Bump `metadata.version` in changed `SKILL.md` files.
4. Bump `version` in `Cargo.toml`.
5. Commit all changes (SKILL.md files + Cargo.toml + README can share one commit).
6. **Tag and push** — this is the only action that triggers the CI release:
   ```
   git tag v<version>
   git push origin main --tags
   ```
7. The workflow runs automatically: plan → build (Windows/Linux/macOS) → create GitHub Release with
   platform artifacts.

**Workflow output:** per-platform binary archives + installer scripts (.sh/.ps1), attached to the
GitHub Release page. The `jdbg update` command downloads the latest release artifact for the current
platform, self-updates, and re-registers every coding agent that had already been configured by `jdbg setup`.

## Build & test conventions

- Build `cargo build` · test `cargo test`. The environment is Windows; this session drives builds/tests via
  **PowerShell** (the Bash tool is unavailable here).
- **TDD for pure logic** (parser, protocol mapping, jsonrpc/tools): write the failing test first, watch it fail,
  then implement. Platform side-effects (handle inheritance, real-jdb behavior) are the documented TDD
  exception — verify them with an end-to-end run instead.
- Unit-test the parser against captured real jdb transcripts (locale-forced). MCP can be exercised end-to-end by
  feeding JSON-RPC to `jdbg __mcp`'s stdin. Don't break existing tests.

## Reference material

The Bash reference plugin is the **authoritative source for jdb command syntax/behavior** — consult, do not
re-derive: `jdb-agentic-debugger/skills/jdb-debugger/` (`references/jdb-commands.md`, `references/jdwp-options.md`,
`scripts/*.sh`, `SKILL.md`).

> **Known reference bug — do not replicate:** the reference launch script has a dead `--suspend` flag (parsed but
> never applied).
