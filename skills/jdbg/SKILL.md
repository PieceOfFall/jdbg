---
name: jdbg
description: Use when you need a Java program's real runtime state instead of reading source or adding print statements — the actual value of a variable/field/expression at a line, why an exception or NullPointerException is thrown (with stack + locals at the throw site), what a thread is blocked or deadlocked on, or how execution reaches some code. Also for stepping through Java line by line, or attaching to an already-running JVM that has JDWP enabled. Cross-platform, native on Windows, no IDE.
compatibility: Requires a JDK 8+ (provides the `jdb` command). Debugging is driven through the `jdbg` MCP server (tools named `launch`, `break_at`, `run`, `locals`, …). Native on Windows, Linux, macOS.
allowed-tools: mcp__jdbg__launch, mcp__jdbg__attach, mcp__jdbg__status, mcp__jdbg__list, mcp__jdbg__kill, mcp__jdbg__break_at, mcp__jdbg__break_in, mcp__jdbg__catch, mcp__jdbg__breakpoints, mcp__jdbg__clear, mcp__jdbg__run, mcp__jdbg__cont, mcp__jdbg__step, mcp__jdbg__next, mcp__jdbg__step_out, mcp__jdbg__where, mcp__jdbg__locals, mcp__jdbg__print, mcp__jdbg__dump, mcp__jdbg__eval, mcp__jdbg__threads, mcp__jdbg__thread, mcp__jdbg__frame, mcp__jdbg__list_source, mcp__jdbg__inspect, mcp__jdbg__raw, Bash(javac:*), Bash(java:*), Read
metadata:
  version: "2.1"
---

# jdbg — interactive Java debugging for agents

`jdbg` is an **MCP server** that drives the JDK's `jdb` debugger as a **stateful background session**, so you
can debug a Java program across many separate tool calls. Each tool call (`launch`, `break_at`, `run`,
`locals`, …) sends ONE command to the session and returns a **structured result**. A background daemon keeps
the JVM and debugger state alive between calls — set a breakpoint in one call, run in the next, inspect in the
next.

> The tools appear as `mcp__jdbg__<tool>` (e.g. `mcp__jdbg__break_at`). Below they are named bare (`break_at`)
> for brevity. The first tool call auto-starts the background daemon — no setup step is needed.

**Core loop: react to each result.** Call one tool, read its structured output, then decide the next call.
Do NOT batch commands or tune timing — `jdbg` waits for `jdb` to be ready (no sleeps), and a hung program
returns a non-destructive `Timeout`, not a freeze.

## When to use
- Find the real runtime value of a variable / field / expression at a line (not by reading code).
- Diagnose why an exception or NPE is thrown — the stack and locals at the throw site.
- Inspect threads; find what a blocked or deadlocked thread is waiting on.
- Step through execution line by line to see actual control flow.
- Attach to an already-running JVM that has JDWP enabled.

**When NOT:** logic simple enough to read; a compile/build error (that is `javac`, not a debugger); no
running JVM and nothing launchable.

## Start a session — two ways

**Launch** a program under the debugger (you have the main class + classpath) — call `launch` with
`main_class` (and usually `classpath` / `sourcepath`; pass program args via `app_args`):
```
launch { "main_class": "com.example.Main", "classpath": "out", "sourcepath": "src", "app_args": ["arg1", "arg2"] }
```
Returns a session id, state `loaded` (JVM not started yet). Set breakpoints, then call `run`.

**Attach** to a running JVM started with JDWP — call `attach`:
```
attach { "host": "localhost", "port": 5005, "sourcepath": "src" }
```
Returns state `suspended`. Set breakpoints, then call `cont` (attach has no `run`).

### Enabling JDWP on the target (for attach) — JDK-version-aware
Start the target JVM with:
```
-agentlib:jdwp=transport=dt_socket,server=y,suspend=y,address=<ADDR>
```
- **JDK 8:** `address=5005` (all interfaces) or `address=localhost:5005`. **`*:5005` is NOT valid on JDK 8.**
- **JDK 9+:** `address=*:5005` is allowed.
- `suspend=y` makes the JVM wait for the debugger; `suspend=n` attaches to an already-running app.

## Typical workflow
1. `launch { "main_class": "Main", "classpath": "out" }` (or `attach { "port": 5005 }`)
2. `break_at { "class": "com.example.Main", "line": 42 }` — set a line breakpoint
3. `run` (launch) or `cont` (attach) — execution stops at the breakpoint → `Stopped`
4. Inspect: `locals`, `where`, `print { "expr": "..." }`
5. Decide and advance: `step` / `next` / `cont`
6. Repeat 4–5; finish with `cont` to run to exit (`VmExited`) or `kill`

## Tool reference

Common arguments: any session tool accepts `session` (a session id; omit when exactly one session exists);
execution-control tools (`run` / `cont` / `step` / `next` / `step_out`) also accept `timeout` (seconds,
overrides the default). To force a specific JDK, pass `jdb_path` to `launch` / `attach` (jdb is otherwise
found via `JAVA_HOME/bin` → PATH → common install dirs).

### Session
| Tool | Purpose |
|---|---|
| `launch { main_class, classpath?, sourcepath?, app_args?, jdb_args?, name?, jdb_path? }` | start a JVM under jdb (state `loaded`) |
| `attach { host?, port?, sourcepath?, name?, jdb_path? }` | attach to a running JVM via JDWP |
| `status` · `list` | current state / all sessions |
| `kill` | end the session (defaults to the sole session; pass `session` if more than one) |

> Daemon management is automatic: the daemon starts on the first tool call and persists. There is no MCP tool
> to start/stop it — it is an implementation detail, not part of the debugging surface.

### Breakpoints
| Tool | Purpose |
|---|---|
| `break_at { class, line }` | break at a source line |
| `break_in { class, method, args? }` | break at method entry (`args` = comma-separated param types, disambiguates overloads) |
| `catch { exception, mode? }` | break when an exception is thrown (`mode`: caught \| uncaught \| all) |
| `breakpoints` · `clear { spec }` | list / remove breakpoints |

### Execution control (blocking; larger default timeout)
| Tool | Purpose |
|---|---|
| `run` | start the app (launch mode only) |
| `cont` | continue until the next stop |
| `step` · `next` · `step_out` | step into · over · out of the current method |

These tools return a `Stopped` result that **automatically includes source context** (surrounding source
lines with `=>` marking the current line) and the **top stack frame** when available. You do NOT need to
call `list_source` or `where` separately after stopping — the information is already in the response.

### Inspection (fast)
| Tool | Purpose |
|---|---|
| `locals` | local variables in the current frame |
| `print { expr }` · `eval { expr }` | value of an expression (can call methods on live objects) |
| `dump { expr }` | all fields of an object |
| `inspect { expr, max_elements? }` | show size + first N elements of a collection/array (default 10, max 50) |
| `where { all? }` | call stack of the current thread (or every thread with `all: true`) |
| `threads` · `thread { id }` | list threads / switch the current thread |
| `frame { direction, n? }` | move within the call stack (`direction`: up \| down) |
| `list_source { line? }` | show source around a line |
| `raw { command }` | escape hatch: send a literal jdb command (`monitor`, `fields`, `methods`, `classes`, `redefine`, `trace`, …) |

## Reading results & deciding what to do next
Every tool returns a typed result. The ones that drive the next move:
- **`Stopped`** — a breakpoint or step landed. The response **already includes** source context (lines around
  the stop with `=>` marking the current line) and the top stack frame — you can read them immediately without
  extra calls. Execution stops **before** the indicated line runs (the line has not yet executed). Use `locals`
  / `print { expr }` / `inspect { expr }` to examine state.
- **`ExceptionCaught`** — an exception fired. `where` for the throw site, `locals` for state.
- **`VmExited`** — the program ended; the session is done (`list` / `kill`).
- **`Timeout`** — the app did not stop within the timeout (long loop or deadlock). The session is **kept
  alive** and marked `running` — investigate with `threads` / `where { all: true }`, or `kill`. Re-run the
  blocking tool with a larger `timeout` if it just needs more time.
- A **`[note]` about line mismatch** means the JVM rounded the breakpoint to the nearest line with executable
  bytecode — this is normal for lines that are comments, blank, or declarations-only.
- A **`[note]` about `-g`** means the class lacks local-variable debug info → recompile with `javac -g`.

## Inspecting collections efficiently

Use `inspect` instead of manually looping `print expr.get(0)`, `print expr.get(1)`, etc.:
```
inspect { "expr": "myList" }               → shows size + all elements (up to 10)
inspect { "expr": "map.keySet()", "max_elements": 20 }  → first 20 keys
```
Works with `ArrayList`, `HashMap.values()`, `HashMap.keySet()`, arrays, and any object with
`.size()`/`.length` + `.get(i)`/`[i]` accessors. Returns structured output: size, elements list, and
whether the result was truncated.

## Common mistakes
- **`locals` empty / "information not available"** → the class was compiled without debug info. Recompile
  with `javac -g`.
- **`run` after attach** → attach has no `run` (the JVM is already running); use `cont`.
- **Breakpoint never hit** → wrong line (e.g. a `}`-only line has no code), or wrong class. Note that
  breakpoints set before the class loads are **deferred** (this is normal) and bind on `run`/`cont`.
- **Calling `list_source`/`where` after every stop** → unnecessary. `Stopped` results already include source
  context and the top stack frame. Only call them if you need the full stack or a different line range.
- **Multiple sessions** → pass `session` (an id from `list`), or keep one session at a time.
- **Treating `Timeout` as a crash** → it is non-destructive; the session is still alive. Inspect or kill it.
- **Wrong JDK picked up** → pass `jdb_path` to `launch` / `attach`, or set `JAVA_HOME`, to force a specific
  JDK (e.g. JDK 8 vs JDK 21).

## Attach-mode guidance (Web servers / long-running JVMs)

### Session lifecycle: one attach per debugging task

**Do NOT kill + re-attach to the same JVM repeatedly.** jdb reconnection to the same JDWP port (without
restarting the JVM) is unreliable on JDK 8 — breakpoint event registration may silently fail on the second
connection, causing `cont` to timeout indefinitely.

**Best practice:** use a single session for the entire debugging task:
1. `attach` once
2. Set breakpoints, `cont`, inspect, `step`/`next`, `cont` again — all within the same session
3. When done, `kill` (this resumes the VM so the server keeps running)
4. If you need to debug again later, `attach` fresh — but avoid rapid kill+re-attach cycles

### Avoiding "No thread specified" or empty responses

If `where` / `locals` returns empty or "No thread specified" immediately after a `Stopped` result:
1. Retry the **same** tool call once — it is a transient timing issue that resolves on the second call
2. If it persists, use `threads` to see if the hit thread shows `running (at breakpoint)` (normal) or
   `cond. waiting` (abnormal — session may need to be killed and re-attached)
3. The `thread { id }` tool can force-set the current thread by name (the thread name from the `Stopped`
   event, e.g. `http-nio-8080-exec-5`)

### Thread IDs in `threads` output

The `threads` output shows lines like `(TaskThread)0x37bd http-nio-8231-exec-8 cond. waiting`. The
**full hex value with `0x` prefix** (e.g. `0x37bd`) is the thread id accepted by the `thread` tool.
Do NOT pass the thread **name** (e.g. `http-nio-8231-exec-8`) — jdb rejects names.
Do NOT strip the `0x` prefix — `37bd` alone is also rejected.

Example: if `threads` shows `(TaskThread)0x37f2 http-nio-8231-exec-20`, use:
```
thread { "id": "0x37f2" }
```

Note: if the thread is no longer suspended (e.g. it has finished executing and returned to the pool),
`thread` will report "not a valid thread id" even with the correct hex value. This indicates the
breakpoint did not hold the thread — see the guidance above about retrying or re-attaching.

### External Tomcat / application servers

When attaching to an external Tomcat (or similar) started with JDWP:
```
-agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5005
```
- `suspend=n` is correct — it means Tomcat doesn't wait for the debugger at startup
- Breakpoints **do** properly suspend threads (SUSPEND_ALL) when hit — this is controlled by jdb, not by
  the `suspend=n` flag
- The target server will **freeze** (all request processing stops) while stopped at a breakpoint — this is
  expected with SUSPEND_ALL. Inspect quickly and `cont` to unfreeze
- `kill` resumes the VM before disconnecting so the server continues running afterwards
