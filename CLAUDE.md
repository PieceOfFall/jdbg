# CLAUDE.md — java-agent-debugger (`jdbg`)

> Project charter for AI agents working on this repository. Read this first.

## 1. Project Overview

`java-agent-debugger` is a cross-platform **Rust CLI** (invoked as `jdbg`) that lets an AI coding agent
(Claude Code) **debug Java programs interactively in any Java project**, with **Windows as the primary target**.

It reimplements the logic of the reference project [`jdb-agentic-debugger/`](./jdb-agentic-debugger/) — a Claude
*plugin* made of Bash scripts that wrap the JDK's `jdb` command-line debugger. That reference is **Unix-only**
(Bash pipes, `/tmp`, `mktemp`, `timeout`, `nc`, and WSL on Windows) and drives `jdb` with **fragile sleep-based
timing and no output parsing** — the agent just reads raw transcript text.

This project fixes both problems:
- **Native & cross-platform** — pure Rust, no Bash/WSL/temp-file dependencies; runs natively on Windows.
- **Prompt-aware, not sleep-based** — reads `jdb`'s output until its prompt returns, bounded by timeouts.
- **Stateful** — a debug session persists in the background, so the agent can set a breakpoint, inspect, decide,
  step, and inspect again across many separate tool calls.

The end product is the `jdbg` binary **plus** a Claude Code skill + plugin manifest so Claude knows the command
surface and workflow.

## 2. Current Status

**Greenfield.** The crate (`Cargo.toml`, edition 2024) currently contains only `src/main.rs` with
`fn main() { println!("Hello, world!"); }` and no dependencies. Nothing in this document is implemented yet — this
charter defines what to build. Build it incrementally following the **Implementation Roadmap (§12)**.

## 3. Confirmed Design Decisions (binding)

These were decided with the user and are not open for re-litigation without asking:

1. **Stateful background session.** A long-lived debug session persists in the background, one per debugged JVM.
   Each `jdbg <subcommand>` invocation is a *separate OS process* that sends ONE command to the running session and
   returns its result. This is required because each Claude Code tool call spawns a new process, but the `jdb`
   session and JVM state must survive between calls.
2. **Engine = wrap `jdb`, prompt-aware.** Spawn the JDK's `jdb`, control it via piped stdin/stdout, and detect
   readiness by reading until the prompt — never by sleeping. All blocking commands are bounded by timeouts.
3. **Output = human-readable text by default, `--json` flag** for machine-structured results.
4. **Deliverable = CLI binary + Claude skill/plugin.** See **Future Deliverables (§13)**.

## 4. Architecture

### Three-actor model

```
  Claude Code tool call                          (one short-lived process per call)
        │  jdbg break-at com.example.Main 42
        ▼
  ┌─────────────┐   local socket (named pipe / UDS)   ┌──────────────────────────────┐
  │  CLI (jdbg) │ ─────────────────────────────────►  │  Daemon (jdbg __daemon)       │
  │  one-shot   │ ◄─────────────────────────────────  │  one per user login           │
  └─────────────┘        JSONL request/response        │  HashMap<SessionId, Session> │
                                                        └──────────────┬───────────────┘
                                                                       │ owns
                                                          ┌────────────┴────────────┐
                                                          ▼                         ▼
                                                   ┌─────────────┐           ┌─────────────┐
                                                   │ jdb child A │  …        │ jdb child B │
                                                   │  → JVM A    │           │  → JVM B    │
                                                   └─────────────┘           └─────────────┘
```

- **CLI** (`jdbg <subcommand>`) — short-lived. Parses args, connects to the daemon (auto-spawning it if absent),
  sends one request, prints the response, exits. One per Claude tool call.
- **Daemon** (`jdbg __daemon`, hidden subcommand) — long-lived, **one per user login**. Owns the IPC listener and a
  `HashMap<SessionId, Session>`. Multiplexes N concurrent sessions (not one daemon per session — this makes session
  listing and orphan cleanup tractable).
- **`jdb` child** — one per debug session, spawned and owned by the daemon.

### IPC

- Use the **`interprocess` crate `LocalSocket`** abstraction — Windows **named pipe** (`\\.\pipe\jdbg-<user>`) and
  Unix **domain socket** (namespaced) behind one API. **Not** TCP-on-localhost (avoids port allocation, firewall/AV
  prompts, and cross-process reachability). Use the blocking API (we are threads-based, see §5).
- The socket name is **fixed per user** (derived from the sanitized username), so the CLI never discovers it.
  Connect failure (not found / refused) means "no daemon running."
- **Wire format = newline-delimited JSON (JSONL)**, one request + one response per connection:
  - Request: `{"v":1,"id":"<reqid>","session":"k7m2qx9p"|null,"cmd":{...}}`
  - Response: `{"v":1,"id":"<reqid>","ok":true,"result":{...}}` or `{"ok":false,"error":{...}}`
  - `cmd` is an internally-tagged serde enum mirroring the subcommands. One connection = one request, so no
    multiplexing protocol is needed.

### Daemon lifecycle (auto-spawn)

- When a CLI command needs the daemon and the connect fails, the CLI **auto-spawns** it
  (`Command::new(current_exe()).arg("__daemon")`, detached), polls (~2s bounded) for the socket, then retries. So
  `jdbg launch …` "just works" with no explicit start step (important — Claude will forget to start a daemon).
- Detach: Windows `CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS` (via `std::os::windows::process::CommandExt`),
  stdio null; Unix `setsid` via `std::os::unix::process::CommandExt::pre_exec`, stdio null. No extra crates.
- **Idempotent bind** resolves spawn races: if two CLIs auto-spawn at once, the second daemon fails to bind the
  fixed socket name and exits 0 — the first is already serving.
- `jdbg daemon start|stop|status` exist for explicit control/diagnostics but are not on the happy path.

### Session registry (on disk)

- Paths via `directories::ProjectDirs::from("dev","claude","jdbg").data_local_dir()`:
  - Windows: `%LOCALAPPDATA%\claude\jdbg\data\`
  - Linux: `$XDG_DATA_HOME/jdbg`  ·  macOS: `~/Library/Application Support/dev.claude.jdbg`
- Files: `daemon.json` (daemon pid, socket name, version, start time — liveness) and `sessions.json`
  (`[{id,name,mode,target,jdb_pid,created_at,state}]`).
- **The daemon is the single writer**, rewriting atomically (temp-in-same-dir + rename). The daemon's in-memory map
  is the source of truth while alive; the registry is a convenience/diagnostic cache (e.g. for an offline `list`),
  **not** the IPC channel.

## 5. jdb Control Contract

This is the riskiest part of the system. **Prove it first** (§12). The regexes below are the authoritative
parser contract for `jdb/parser.rs`.

### Spawn flags (MANDATORY)

Always spawn `jdb` with these `-J` flags, or output parsing WILL break on this machine:

```
-J-Duser.language=en  -J-Duser.country=US  -J-Dfile.encoding=UTF-8
```

**Why:** on this system `jdb` emits **localized (Chinese) messages** (`jdb版本 …`). Event banners like
`Breakpoint hit:`, `Step completed:`, `Exception occurred:`, and `The application exited` are localized and will
NOT match English regexes unless the locale is forced. Prompt detection is locale-independent and is the primary
readiness signal; forcing English makes the event banners reliable as a secondary signal.

### Spawning

- Use plain `std::process::Command` with **piped** stdin/stdout/stderr. **Not ConPTY** — ConPTY injects
  ANSI/cursor/resize escapes that are harder to parse, and the reference proves plain pipes work. Keep ConPTY
  (via a pty crate) only as a documented fallback if a future JDK build withholds the prompt on a pipe.
- Read stdout **byte-wise into a rolling buffer** and match the prompt at the **tail** (the prompt has no trailing
  newline; one read may not contain a full line). Normalize `\r\n` → `\n` and decode UTF-8 lossy before matching.

### Threading model (NOT tokio)

Per session: (1) a **stdout-reader thread** (rolling buffer + prompt/event matcher), (2) a **stderr-drain thread**
(jdb prints connection errors / "Listening for transport" there; folded into results, never blocks stdout
matching), (3) the **request handler** that writes `cmd\n` to stdin then waits on a channel. The daemon accept loop
spawns one short handler thread per connection. Threads + channels only — the concurrency is tiny and bounded.

### Run-state machine

`RunState`: `Loaded` (before `run`) → `Suspended` (stopped at bp/exception/step) → `Running` (app executing) →
`Exited` / `Dead`.

- **Normal commands** (`locals`, `where`, `print`, `stop at`, `threads`, …): write `cmd\n`, read until the prompt
  regex matches at buffer tail. Small default timeout (~15s).
- **Blocking commands** (`run`, `cont`, `step`, `next`, `step up`): the prompt does NOT return until a breakpoint
  hit, exception caught, step complete, or VM exit. Watch, in priority order: (1) terminal/VM-exit marker →
  `Exited`; (2) event marker (breakpoint/exception/step) → `Suspended`; (3) a bare prompt reappearing →
  `Suspended`. Larger default timeout (~30s, per-call overridable via `--timeout`).
- **On timeout → non-destructive `Timeout` result** with partial output; **leave the session alive** and mark
  `Running` (the deadlock/long-loop case). Claude can then run `jdbg threads` / `jdbg where --all` / `jdbg kill`.
  (The reference *kills* on timeout; we do not.)
- **One in-flight command per session**, enforced by a per-session `Mutex` held across write+wait. `jdb` is
  line-oriented and cannot handle interleaved commands. Different sessions run in parallel freely.

### Regexes (compile once via `std::sync::LazyLock`)

- **Prompt** (match at buffer tail; both forms; no trailing newline):
  `^(?:>|[^\s\[\]]+\[\d+\])\s$`
  matches `> ` (pre-run / not suspended) and `main[1] `, `Thread-3[1] `, `pool-1-thread-2[2] ` (`thread[frame] `).
- **Breakpoint hit / step:**
  `^(?:Breakpoint hit|Step completed): "thread=.*", (?P<class>\S+)\.(?P<method>\S+)\(\), line=(?P<line>\d+)`
- **Exception caught:** `^Exception occurred: (?P<exc>\S+) \((?P<caught>caught|uncaught)`
- **VM exit / terminal:** `^The application (?:exited|has been disconnected)`
- **Connection/launch errors** (surface as failure): `^Unable to attach to target VM`, `^java\.io\.IOException`,
  `^Input stream closed`.
- **Missing debug info** (hint `-g`): `Local variable information not available` → set `note`, still succeed.

## 6. Module Layout

```
src/
  main.rs            // entry: parse CLI, dispatch to daemon mode (__daemon) or client flow
  cli.rs             // clap derive: Cli, Commands enum, arg structs (maps 1:1 to §7)
  client.rs          // CLI-side: connect-or-auto-spawn daemon, send one Request, receive Response
  protocol.rs        // shared wire types: Request, Response, Command enum, CommandResult (§8)
  daemon/
    mod.rs           // daemon lifecycle: bind socket, accept loop, detached-spawn helper, shutdown
    handler.rs       // per-connection: decode Request, route to SessionManager, encode Response
    manager.rs       // SessionManager: HashMap<SessionId,Session>, create/lookup/remove, default-session
  session.rs         // Session: owns jdb child + reader threads + RunState + per-session command Mutex
  jdb/
    process.rs       // spawn jdb (launch/attach arg building, MANDATORY -J flags), piped stdio, write_command
    reader.rs        // stdout read loop: rolling buffer, prompt/event/terminal detection, timeout, emit block
    parser.rs        // regexes + classify raw block -> Event / StackFrames / LocalsTable / RawText
  registry.rs        // on-disk paths via `directories`; atomic read/write of daemon.json & sessions.json
  output.rs          // render CommandResult as human text (default) or serde_json (--json); tables
  jdkpath.rs         // locate jdb: --jdb-path -> JAVA_HOME/bin -> PATH -> common dirs
  error.rs           // thiserror Error enum + Result alias; maps to wire error + process exit codes
```

### Design invariants

- **One daemon per user**; many sessions multiplexed inside it.
- **One in-flight command per session** (per-session `Mutex`).
- **The daemon is the single writer** of the on-disk registry; the CLI only reads it (offline fallback).
- **No temp files, no shell, no sleeps.** Commands are written directly to `jdb`'s stdin.
- Keep modules **small and single-purpose**; if a file grows large it is doing too much.

## 7. CLI Command Surface

Binary name: **`jdbg`** (set via `[[bin]] name = "jdbg"` in `Cargo.toml`; crate stays `java-agent-debugger`).
Global flags (every jdb-touching subcommand): `--session <id>` (omit ⇒ default session if exactly one exists),
`--json`, `--timeout <secs>`. Also global: `--jdb-path <path>` to override jdb discovery.

### Session lifecycle

| Subcommand | Args | jdb mapping / behavior |
|---|---|---|
| `jdbg launch <MainClass>` | `--classpath`, `--sourcepath`, `--app-args`, `--jdb-arg` (repeatable), `--name` | `jdb -classpath CP -sourcepath SP MainClass appargs`; returns new session id, app at initial stop (not yet `run`). |
| `jdbg attach` | `--host` (localhost), `--port` (5005), `--sourcepath`, `--name` | `jdb -attach host:port -sourcepath SP` |
| `jdbg status` | `--session` or all | reports `RunState`, mode, target, jdb pid, last event (no jdb command) |
| `jdbg list` | `--json` | list sessions from daemon (fallback `sessions.json`) |
| `jdbg kill` | `--session` | send `quit`, join threads, remove session (named `kill`, not `stop`, to avoid colliding with breakpoint `stop`) |
| `jdbg daemon start\|stop\|status` | — | control the daemon itself |

### Breakpoints

| Subcommand | Args | jdb mapping |
|---|---|---|
| `jdbg break-at <Class> <line>` | — | `stop at Class:line` |
| `jdbg break-in <Class> <method>` | `--args <types>` (overloads) | `stop in Class.method[(types)]` |
| `jdbg catch <Exception>` | `--mode caught\|uncaught\|all` | `catch [mode] <Exception>` |
| `jdbg breakpoints` | — | list breakpoints (`clear` with no args) |
| `jdbg clear <Class:line\|Class.method>` | — | `clear <spec>` |

### Execution control (blocking-aware, larger default timeout)

| Subcommand | jdb mapping |
|---|---|
| `jdbg run` | `run` (launch mode only) |
| `jdbg cont` | `cont` |
| `jdbg step` | `step` |
| `jdbg next` | `next` |
| `jdbg step-out` | `step up` |

### Inspection (fast, small default timeout)

| Subcommand | Args | jdb mapping |
|---|---|---|
| `jdbg where` | `--all` | `where` / `where all` |
| `jdbg locals` | — | `locals` |
| `jdbg print <expr>` | — | `print <expr>` |
| `jdbg dump <obj>` | — | `dump <obj>` |
| `jdbg eval <expr>` | — | `eval <expr>` |
| `jdbg threads` | — | `threads` |
| `jdbg thread <id>` | — | `thread <id>` |
| `jdbg frame up\|down [n]` | — | `up [n]` / `down [n]` |
| `jdbg list-source [line]` | — | `list [line]` |
| `jdbg raw <jdb command...>` | escape hatch | writes the literal string to jdb, returns raw text |

`jdbg raw` is the safety valve so Claude is never blocked by an unmodeled command (`monitor`, `redefine`, `trace`,
`fields`, `methods`, `classes`, …). No shell is involved (we write straight to jdb stdin), so there is no
shell-injection surface.

## 8. Output Schema

One internally-tagged `CommandResult` enum (`#[serde(tag="kind")]`) serializes to JSON and is rendered to text by
`output.rs`:

```text
enum CommandResult {
  SessionCreated { session, mode: "launch"|"attach", target, state },
  SessionList    { sessions: Vec<SessionInfo> },          // id,name,mode,target,state,jdb_pid,created_at
  Status         { session, state, last_event: Option<Event>, jdb_alive },
  BreakpointSet  { spec, kind: "line"|"method"|"catch", deferred },  // jdb defers until class loads
  BreakpointList { breakpoints: Vec<String> },

  // execution outcomes (run/cont/step/next/step-out):
  Stopped         { event, location, thread, frame },     // breakpoint/step landed
  ExceptionCaught { exception, caught: bool, location, thread },
  VmExited        { exit_code: Option<i32>, tail },        // "The application exited"
  Timeout         { partial_output, state },               // app likely hung/deadlocked (session kept alive)

  // inspection outcomes:
  StackTrace { frames: Vec<StackFrame> },                  // index, class, method, file, line
  Locals     { vars: Vec<VarBinding> },                    // name, type?, value (renders as table)
  Value      { expr, value, ty: Option<String> },          // print/eval
  ObjectDump { expr, fields: Vec<VarBinding> },            // dump
  Threads    { threads: Vec<ThreadInfo> },                 // id(hex), name, group, state
  Source     { around_line, lines: Vec<SourceLine> },

  Raw { text },                                            // jdbg raw / unmodeled fallback
}
```

Supporting structs: `Location{class,method,file,line}`, `StackFrame{index,location,is_native}`,
`VarBinding{name,ty,value}`, `ThreadInfo{id,name,group,state}`, `SourceLine{number,text}`,
`RunState{Loaded|Suspended|Running|Exited|Dead}`, `Event{Breakpoint|Step|Exception|VmExit}`.

Every result also carries side bands: `stderr: Option<String>` (drained jdb stderr if non-empty) and
`note: Option<String>` (e.g. "compile with `-g` for locals").

**Rendering rule:** text mode renders tables for `Locals`/`Threads`/`StackTrace`, a one-line headline for
`Stopped`/`VmExited`, and `Raw.text` verbatim. JSON mode (`--json`) emits the enum as-is.

## 9. Dependencies

Edition is **2024**. Add to `Cargo.toml`:

| Crate | Why |
|---|---|
| `clap` (derive) | Declarative subcommand/flag parsing; matches §7. |
| `serde` + `serde_json` (derive) | Wire format (JSONL) and `--json` output from one set of types. |
| `interprocess` | Cross-platform local sockets: Windows named pipes + Unix domain sockets behind one API (core IPC). |
| `thiserror` | Typed `Error` enum in `error.rs`, mapped to exit codes. |
| `anyhow` | Top-level error context in `main.rs`/`client.rs`. |
| `regex` | Prompt/event detection in `reader.rs`/`parser.rs`. |
| `directories` | `%LOCALAPPDATA%` / XDG / macOS app-support paths for the registry. |
| `rand` | Generate `SessionId` slugs (could be hand-rolled to drop the dep). |
| `jiff` (or `time`) | Timestamps in registry/status. Modern; avoid `chrono`. |

Use `std::sync::LazyLock` (stable, edition 2024) for one-time regex compilation, and `std::os::*::process` for
detach. **Deliberately excluded:** `once_cell`, `nix`, `daemonize`, `tokio` (threads suffice, §5),
`portable-pty`/`conpty` (pipe-first, §5), and any TCP/RPC framework (JSONL over local socket).

## 10. Environment & Prerequisites

- **Minimum supported JDK: 8.** The user's `JAVA_HOME` is `C:\Users\luyiwen\.jdks\azul-1.8.0_492`
  (Zulu OpenJDK **1.8.0_492**). The tool MUST work on JDK 8; JDK 9–21+ must also work. The `jdb` command surface
  used here (`stop`, `run`, `cont`, `step`, `next`, `where`, `locals`, `print`, `dump`, `eval`, `threads`, `quit`)
  is identical across these versions.
- **jdb discovery order** (`jdkpath.rs`): `--jdb-path` flag → **`JAVA_HOME/bin/jdb(.exe)`** → `PATH` → common
  install dirs (`C:\Program Files\Java\*`, `%USERPROFILE%\.jdks\*`, `Eclipse Adoptium`, `Microsoft\jdk-*`; Unix
  `$JAVA_HOME/bin`, `/usr/lib/jvm/*`). **Prefer `JAVA_HOME` over PATH** — on this machine PATH resolves to JDK 21
  (`C:\Program Files\Java\zulu-21\bin\`) while the user wants the JDK 8 at `JAVA_HOME`. On miss, return a structured
  error telling the user to install a JDK or set `JAVA_HOME`.
- **`-g` debug info:** locals/line breakpoints need classes compiled with debug info (`javac -g`). If the parser
  sees "Local variable information not available", set `note` advising recompilation with `-g`.
- **Attach mode (JDWP) — JDK-version-aware syntax:** start the target JVM with
  `-agentlib:jdwp=transport=dt_socket,server=y,suspend=y,address=<ADDR>`.
  - **JDK 8:** `address=5005` (all interfaces) or `address=localhost:5005` (local only). **`address=*:5005` is NOT
    supported on JDK 8** — the `*:` wildcard was added in JDK 9.
  - **JDK 9+:** `address=*:5005` is allowed.
  Use `suspend=y` so the JVM waits for the debugger; `suspend=n` to attach to an already-running app.

## 11. Build, Run & Test

- Build: `cargo build` · Run: `cargo run -- <subcommand> …` (or the built `jdbg`).
- **Manual exercise against real `jdb`** (the important verification — automate later):
  1. Write a tiny `Main.java` with a loop and a few locals; `javac -g Main.java`.
  2. `jdbg launch Main --classpath .` → expect `SessionCreated`, state `Loaded`.
  3. `jdbg break-in Main main` → `BreakpointSet`.
  4. `jdbg run` → expect `Stopped` at `main`.
  5. `jdbg locals`, `jdbg where`, `jdbg print <var>`, `jdbg step`, `jdbg cont` → verify each result type.
  6. `jdbg cont` past the end → `VmExited`. `jdbg list` / `jdbg kill`.
- Suggested tests: unit-test `jdb/parser.rs` against **captured real jdb transcripts** (locale-forced) — this is
  where correctness lives. Integration tests can drive a real `jdb` against a fixture class behind a feature flag,
  since they require a JDK.
- Keep the daemon observable: `jdbg daemon status` and `jdbg status` should make state inspectable while debugging
  the debugger.

## 12. Implementation Roadmap (ordered)

1. **`protocol.rs` + `error.rs`** — wire types and the error enum first.
2. **`jdb/{process,reader,parser}.rs`** — drive a real `jdb` from a throwaway `main`. **De-risk prompt detection +
   the locale mandate before anything else** — this is the make-or-break piece. Capture transcripts for parser
   tests.
3. **`session.rs`** — bind a jdb child to its reader threads; per-session command `Mutex`; `RunState`.
4. **`daemon/*` + `registry.rs` + IPC** — listener, accept loop, `SessionManager`, on-disk registry.
5. **`client.rs`** — connect-or-auto-spawn the daemon; one-shot request/response.
6. **`cli.rs` + `output.rs`** — the clap surface and text/JSON rendering.
7. **`SKILL.md` + plugin manifest** — see §13.

## 13. Future Deliverables (the Claude packaging)

- **`SKILL.md`** (native-first): documents the `jdbg` surface (§7) and the stateful workflow
  (launch/attach → break → run → inspect → cont → kill), the `--session`/`--json`/`--timeout` conventions, the `-g`
  hint, and the JDK-version-aware JDWP-enable instructions (§10). It must **not** carry over the reference's
  WSL/temp-file/sleep-tuning/`--auto-inspect` guidance; the new workflow is "react to each result."
- **Plugin manifest** — `.claude-plugin/plugin.json` + `marketplace.json` naming the skill and declaring
  `allowed-tools: Bash(jdbg:*) Read`, pointing Claude at the subcommands.

## 14. Reference Material

The reference plugin is the **authoritative source for `jdb` command syntax and behavior** — consult it, do not
re-derive:

- `jdb-agentic-debugger/skills/jdb-debugger/references/jdb-commands.md` — full jdb command syntax.
- `jdb-agentic-debugger/skills/jdb-debugger/references/jdwp-options.md` — JDWP launch options.
- `jdb-agentic-debugger/skills/jdb-debugger/scripts/jdb-breakpoints.sh` — the sleep-based batch driver this project
  replaces with prompt-aware reads (study what it sends to jdb, ignore its timing model).
- `jdb-agentic-debugger/skills/jdb-debugger/scripts/{jdb-launch.sh,jdb-attach.sh}` — launch/attach arg construction
  to port into `jdb/process.rs`.
- `jdb-agentic-debugger/skills/jdb-debugger/SKILL.md` — source content to rewrite native-first for our `SKILL.md`.

> **Note:** the reference's launch script has a known dead `--suspend` flag (parsed but never applied). Do not
> replicate that bug.
