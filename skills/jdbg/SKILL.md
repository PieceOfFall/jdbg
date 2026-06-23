---
name: jdbg
description: Use when you need a Java program's real runtime state instead of reading source or adding print statements — the actual value of a variable/field/expression at a line, why an exception or NullPointerException is thrown (with stack + locals at the throw site), what a thread is blocked or deadlocked on, or how execution reaches some code. Also for stepping through Java line by line, or attaching to an already-running JVM that has JDWP enabled. Cross-platform, native on Windows, no IDE.
compatibility: Requires a JDK 8+ (provides the `jdb` command) and the `jdbg` binary on PATH. Native on Windows, Linux, macOS.
allowed-tools: Bash(jdbg:*) Bash(javac:*) Bash(java:*) Read
metadata:
  version: "1.0"
---

# jdbg — interactive Java debugging for agents

`jdbg` drives the JDK's `jdb` debugger as a **stateful background session**, so you can debug a Java
program across many separate tool calls. Each `jdbg <subcommand>` is a one-shot process that sends ONE
command to the session and returns a **structured result** (human text, or `--json`). A background daemon
keeps the JVM and debugger state alive between calls — set a breakpoint in one call, run in the next,
inspect in the next.

**Core loop: react to each result.** Run one command, read its structured output, then decide the next
command. Do NOT batch commands or tune timing — `jdbg` waits for `jdb` to be ready (no sleeps), and a
hung program returns a non-destructive `Timeout`, not a freeze.

## When to use
- Find the real runtime value of a variable / field / expression at a line (not by reading code).
- Diagnose why an exception or NPE is thrown — the stack and locals at the throw site.
- Inspect threads; find what a blocked or deadlocked thread is waiting on.
- Step through execution line by line to see actual control flow.
- Attach to an already-running JVM that has JDWP enabled.

**When NOT:** logic simple enough to read; a compile/build error (that is `javac`, not a debugger); no
running JVM and nothing launchable.

## Start a session — two ways

**Launch** a program under the debugger (you have the main class + classpath):
```
jdbg launch com.example.Main --classpath out --sourcepath src -- arg1 arg2
```
Returns a session id, state `loaded` (JVM not started yet). Set breakpoints, then `jdbg run`.

**Attach** to a running JVM started with JDWP:
```
jdbg attach --host localhost --port 5005 --sourcepath src
```
Returns state `suspended`. Set breakpoints, then `jdbg cont` (attach has no `run`).

### Enabling JDWP on the target (for attach) — JDK-version-aware
Start the target JVM with:
```
-agentlib:jdwp=transport=dt_socket,server=y,suspend=y,address=<ADDR>
```
- **JDK 8:** `address=5005` (all interfaces) or `address=localhost:5005`. **`*:5005` is NOT valid on JDK 8.**
- **JDK 9+:** `address=*:5005` is allowed.
- `suspend=y` makes the JVM wait for the debugger; `suspend=n` attaches to an already-running app.

## Typical workflow
1. `jdbg launch Main --classpath out` (or `jdbg attach --port 5005`)
2. `jdbg break-at com.example.Main 42` — set a line breakpoint
3. `jdbg run` (launch) or `jdbg cont` (attach) — execution stops at the breakpoint → `Stopped`
4. Inspect: `jdbg locals`, `jdbg where`, `jdbg print <expr>`
5. Decide and advance: `jdbg step` / `jdbg next` / `jdbg cont`
6. Repeat 4–5; finish with `jdbg cont` to run to exit (`VmExited`) or `jdbg kill`

## Command reference

Global flags (any session command): `--session <id>` (omit when only one session exists), `--json`,
`--timeout <secs>`, `--jdb-path <path>`.

### Session
| Command | Purpose |
|---|---|
| `jdbg launch <Main> [--classpath CP] [--sourcepath SP] [--name N] [-- args]` | start a JVM under jdb (state `loaded`) |
| `jdbg attach [--host H] [--port P] [--sourcepath SP] [--name N]` | attach to a running JVM via JDWP |
| `jdbg status` · `jdbg list` | current state / all sessions |
| `jdbg kill` | end the session (defaults to the sole session; pass `--session` if more than one) |
| `jdbg daemon start\|stop\|status` | manage the background daemon — rarely needed (it auto-starts on first use and persists); `daemon stop` ends all sessions at once |

### Breakpoints
| Command | Purpose |
|---|---|
| `jdbg break-at <Class> <line>` | break at a source line |
| `jdbg break-in <Class> <method> [--args types]` | break at method entry (`--args` disambiguates overloads) |
| `jdbg catch <Exception> [--mode caught\|uncaught\|all]` | break when an exception is thrown |
| `jdbg breakpoints` · `jdbg clear <spec>` | list / remove breakpoints |

### Execution control (blocking; larger default timeout)
| Command | Purpose |
|---|---|
| `jdbg run` | start the app (launch mode only) |
| `jdbg cont` | continue until the next stop |
| `jdbg step` · `jdbg next` · `jdbg step-out` | step into · over · out of the current method |

### Inspection (fast)
| Command | Purpose |
|---|---|
| `jdbg locals` | local variables in the current frame |
| `jdbg print <expr>` · `jdbg eval <expr>` | value of an expression (can call methods on live objects) |
| `jdbg dump <obj>` | all fields of an object |
| `jdbg where [--all]` | call stack of the current thread (or all threads) |
| `jdbg threads` · `jdbg thread <id>` | list threads / switch the current thread |
| `jdbg frame up\|down [n]` | move within the call stack |
| `jdbg list-source [line]` | show source around a line |
| `jdbg raw <jdb command…>` | escape hatch: send a literal jdb command (`monitor`, `fields`, `methods`, `classes`, `redefine`, `trace`, …) |

## Reading results & deciding what to do next
Every command returns a typed result. The ones that drive the next move:
- **`Stopped`** — a breakpoint or step landed. Now `jdbg locals` / `jdbg where` / `jdbg print <expr>`.
- **`ExceptionCaught`** — an exception fired. `jdbg where` for the throw site, `jdbg locals` for state.
- **`VmExited`** — the program ended; the session is done (`jdbg list` / `jdbg kill`).
- **`Timeout`** — the app did not stop within the timeout (long loop or deadlock). The session is **kept
  alive** and marked `running` — investigate with `jdbg threads` / `jdbg where --all`, or `jdbg kill`.
  Re-run the blocking command with a larger `--timeout` if it just needs more time.
- A **`[note]` line** about `-g` means the class lacks local-variable debug info → recompile with `javac -g`.

## Common mistakes
- **`locals` empty / "information not available"** → the class was compiled without debug info. Recompile
  with `javac -g`.
- **`jdbg run` after attach** → attach has no `run` (the JVM is already running); use `jdbg cont`.
- **Breakpoint never hit** → wrong line (e.g. a `}`-only line has no code), or wrong class. Note that
  breakpoints set before the class loads are **deferred** (this is normal) and bind on `run`/`cont`.
- **Multiple sessions** → pass `--session <id>` (from `jdbg list`), or keep one session at a time.
- **Treating `Timeout` as a crash** → it is non-destructive; the session is still alive. Inspect or kill it.
- **Wrong JDK picked up** → `jdbg` finds `jdb` via `--jdb-path` → `JAVA_HOME/bin` → PATH → common install
  dirs. Set `JAVA_HOME` or pass `--jdb-path` to force a specific JDK (e.g. JDK 8 vs JDK 21).
