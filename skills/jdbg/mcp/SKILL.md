---
name: "jdbg"
description: "Use when you need a Java program's real runtime state instead of reading source or adding print statements — the actual value of a variable/field/expression at a line, why an exception or NullPointerException is thrown (with stack + locals at the throw site), what a thread is blocked or deadlocked on, or how execution reaches some code. Also for stepping through Java line by line, or attaching to an already-running JVM that has JDWP enabled. Cross-platform, native on Windows, no IDE."
compatibility: "Requires a JDK 8+ (provides the `jdb` command). Debugging is driven through the `jdbg` MCP server (tools named `launch`, `break_at`, `run`, `locals`, …). Native on Windows, Linux, macOS."
allowed-tools: "mcp__jdbg__launch, mcp__jdbg__attach, mcp__jdbg__status, mcp__jdbg__list, mcp__jdbg__kill, mcp__jdbg__break_at, mcp__jdbg__break_in, mcp__jdbg__catch, mcp__jdbg__watch, mcp__jdbg__unwatch, mcp__jdbg__breakpoints, mcp__jdbg__clear, mcp__jdbg__run, mcp__jdbg__cont, mcp__jdbg__step, mcp__jdbg__next, mcp__jdbg__step_out, mcp__jdbg__where, mcp__jdbg__locals, mcp__jdbg__print, mcp__jdbg__dump, mcp__jdbg__eval, mcp__jdbg__threads, mcp__jdbg__classes, mcp__jdbg__methods, mcp__jdbg__thread, mcp__jdbg__frame, mcp__jdbg__list_source, mcp__jdbg__inspect, mcp__jdbg__raw, mcp__jdbg__suspend, mcp__jdbg__resume, mcp__jdbg__set, mcp__jdbg__force_return, mcp__jdbg__ignore, mcp__jdbg__lock, mcp__jdbg__threadlocks, Bash(javac:*), Bash(java:*), Read"
metadata:
  version: "2.21"
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
`main_class` (and usually `classpath`; pass program args via `app_args`):
```
launch { "main_class": "com.example.Main", "classpath": "out", "sourcepath": "src", "app_args": ["arg1", "arg2"] }
```
Returns a session id, state `loaded` (JVM not started yet). Set breakpoints, then call `run`.

**Attach** to a running JVM started with JDWP — call `attach`:
```
attach { "host": "localhost", "port": 5005, "sourcepath": "src" }
```
The default `jdb` backend returns state `suspended`. Set breakpoints, then call `cont` (attach has no `run`).
For the JDI sidecar, pass `backend: "jdi"` on `launch` or `attach`; launched sessions use `run`, attached sessions use `cont`.

If `sourcepath` is omitted, jdbg uses the MCP server's current working directory as the source root and sends it
to the daemon as an absolute path. On JDI sessions, source lookup also tries the target JVM's `user.dir` and
Maven/Gradle module roots inferred from `java.class.path` (for example `mall-portal/target/classes` →
`mall-portal/src/main/java`). Pass `sourcepath` explicitly when sources live outside the workspace root or
the target classpath does not reveal module directories.

> **`localhost` is auto-normalized to `127.0.0.1`.** On dual-stack machines `localhost` often
> resolves to IPv6 `[::1]`, but JDWP usually listens only on IPv4 `0.0.0.0` → connection refused.
> jdbg rewrites `localhost` to the IPv4 loopback so attach just works; the response `target` field
> shows the address actually connected, plus a `note`. To force IPv6, pass `host: "::1"` explicitly.

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
found via `JAVA_HOME/bin` → PATH → common install dirs). On JDI sessions, `jdb_path` also selects the sidecar
Java runtime when `JDBG_JDI_JAVA` is not set.

### Backend guidance

- Omit `backend` for the mature `jdb` backend. It supports the full tool surface and keeps `raw` as an escape hatch.
- Use `backend: "jdi"` when launching or attaching and you want structured sidecar data. JDI supports breakpoints, exception catchpoints, watchpoints, `run` for launched sessions, `cont`, `step`/`next`/`step_out`, stack/frame navigation, class/method lookup, source listing, thread suspend/resume, locks, safe JSON `inspect`, executable `print`/`eval`/`dump`, `set`, and non-void `force_return`.
- JDI launch starts in state `loaded`; set breakpoints, then call `run`. JDI attach starts in state `running`; set a line or method breakpoint, then call `cont` to wait for the next stop.
- JDI `raw` dispatches supported jdb-style aliases through the sidecar; use a `jdb` session only when you need literal jdb stdin passthrough for an obscure jdb-only command.
- JDI uses `jdbg-jdi-sidecar.jar` next to the `jdbg` binary. Release updates install the official jar there; source builds create it during `cargo build`. If the jar is missing, run `jdbg update` or reinstall from the official release archive. Do not search the filesystem and copy a jar from a source checkout. Override with `JDBG_JDI_SIDECAR_JAR` or `JDBG_JDI_JAVA` only when necessary.

### Session
| Tool | Purpose |
|---|---|
| `launch { main_class, backend?, classpath?, sourcepath?, app_args?, jdb_args?, name?, jdb_path? }` | start a JVM under the selected backend (state `loaded`); `jdb_args` is jdb-only |
| `attach { backend?, host?, port?, sourcepath?, name?, jdb_path? }` | attach to a running JVM via JDWP; pass `backend: "jdi"` for the JDI sidecar |
| `status` · `list` | current state / all sessions |
| `kill` | end the session (defaults to the sole session; pass `session` if more than one) |

> Daemon management is automatic: the daemon starts on the first tool call and persists. There is no MCP tool
> to start/stop it — it is an implementation detail, not part of the debugging surface.

### Breakpoints & watchpoints
| Tool | Purpose |
|---|---|
| `break_at { class, line, condition?, suspend? }` | break at a source line |
| `break_in { class, method, event?, args?, condition?, suspend? }` | break at method entry by default; JDI also supports `event: "exit"` and `"both"` |
| `catch { exception, mode? }` | break when an exception is thrown (`mode`: caught \| uncaught \| all) |
| `watch { field, mode? }` | break when a field is accessed or modified (`mode`: access \| modification \| all; default: modification) |
| `unwatch { field, mode? }` | remove field watchpoint(s) (`mode`: access \| modification \| all; default: modification) |
| `breakpoints` · `clear { spec }` | list / remove breakpoints |

**Conditional breakpoints** — filter for a specific request in high-traffic code:
```
break_at { "class": "com.example.CartService", "line": 42, "condition": "userId == 1619458289" }
```
The condition is evaluated each time the breakpoint fires; execution automatically continues if false.
For an already-running attached JVM, if the breakpoint fires before your next blocking command, the next
inspection command (`threads`, `where`, `print`, `locals`, etc.) first resolves any false conditional hit and
continues automatically.

**Thread breakpoints** (`suspend: "thread"`) — like IDEA's "Suspend: Thread" option. Only the hit thread
is suspended; all other threads (ZooKeeper heartbeat, Dubbo registry, other HTTP handlers) keep running.
**Use this when debugging a live server** to prevent the service from being deregistered:
```
break_at { "class": "com.example.CartService", "line": 42, "suspend": "thread" }
```
The `Stopped` response will include a note confirming thread-level suspend is active. All inspection
commands (`locals`, `print`, `where`) work normally on the suspended thread. When you `cont`, the next
breakpoint hit re-applies the same policy automatically.

You can combine both: `{ "condition": "userId == 123", "suspend": "thread" }` — only stop for the
target user, and only freeze that one request thread when it does stop.

### Execution control (blocking; larger default timeout)
| Tool | Purpose |
|---|---|
| `run` | start the app (launch mode only) |
| `cont` | continue until the next stop |
| `step` · `next` · `step_out` | step into · over · out of the current method |

These tools return a `Stopped` result that **automatically includes source context** (surrounding source
lines with `=>` marking the current line), the **top stack frame**, and the **hit thread's id**
(`thread_id`) when available. You do NOT need to call `list_source` or `where` separately after stopping —
the information is already in the response. To switch to or act on the hit thread, pass `thread_id`
straight to `thread`/`suspend`/`threadlocks` — no need to scan `threads` for it.

### Inspection (fast)
| Tool | Purpose |
|---|---|
| `locals` | local variables in the current frame |
| `print { expr }` · `eval { expr }` | value of an expression (JDI may call methods on live objects) |
| `dump { expr }` | all fields of an object |
| `inspect { expr, max_elements? }` | show size + first N elements of a collection/array (default 10, max 50) |
| `where { all? }` | call stack of the current thread (or every thread with `all: true`) |
| `threads { filter? }` · `thread { id }` | list threads (filter by name substring) / switch the current thread |
| `frame { direction, n? }` | move within the call stack (`direction`: up \| down) |
| `list_source { line? }` | show source around a line |
| `classes { pattern? }` | search loaded classes by substring — find CGLIB proxies, inner classes, or confirm a class is loaded |
| `methods { class }` | list all methods of a loaded class (with param types) — find exact signature for `break_in` |
| `raw { command }` | escape hatch: send a literal jdb command (`monitor`, `fields`, `redefine`, `trace`, …) |

### Thread control · state mutation · locks
| Tool | Purpose |
|---|---|
| `suspend { id? }` · `resume { id? }` | suspend/resume one thread (or all if no id) — fine-grained control without freezing the whole VM |
| `set { lvalue, value }` | assign a variable/field/array element — **mutates live state** to test a fix or force a branch |
| `force_return { value }` | JDI only: force the current non-void method to return a value expression — **mutates control flow** |
| `ignore { exception, mode? }` | stop catching an exception (removes a `catch`); `mode` must match how it was caught |
| `lock { expr }` | monitor owner + waiters for an object — contention/deadlock diagnosis |
| `threadlocks { id? }` | locks a thread holds and is blocked on — the core deadlock command |

## Reading results & deciding what to do next
Every tool returns a typed result. The ones that drive the next move:
- **`Stopped`** — a breakpoint or step landed. The response **already includes** source context (lines around
  the stop with `=>` marking the current line), the top stack frame, and the hit thread's `thread_id` — you can
  read them immediately without extra calls. Execution stops **before** the indicated line runs (the line has
  not yet executed). Use `locals` / `print { expr }` / `inspect { expr }` to examine state. To act on the hit
  thread (switch, suspend, inspect its locks), pass `thread_id` directly — do not scan `threads` for it.
  For JDI method events, `event.type` is `method_entry` or `method_exit`; method exit includes the return
  value when available.
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
On JDI sessions, safe JSON `inspect` reads fields directly and does not invoke getters; it covers common
`ArrayList`, `LinkedList`, `ArrayDeque`, `HashSet`, `LinkedHashSet`, `TreeMap`, `TreeSet`, `HashMap`,
`LinkedHashMap`, unmodifiable wrappers, arrays, and ordinary objects. On jdb sessions, `inspect` keeps the
classic collection/array summary behavior. Results include size/entries/elements and truncation metadata
where available.

On JDI sessions, `print`, `eval`, `dump`, `set`, and `force_return` are executable capabilities. They may
invoke methods in the target JVM and can have side effects. Use `inspect` when you need safe field-reading
without getters or method calls.

After JDI `force_return`, the target VM applies the forced return when the thread resumes. A `where` call before
the next `cont`/`step` may still show the old frame and include a note explaining that pending refresh.

## Finding classes and methods (Spring/CGLIB/Tomcat)

Use `classes` to discover runtime class names — essential when Spring wraps beans in CGLIB proxies:
```
classes { "pattern": "CartService" }
→ ["com.example.CartService", "com.example.CartService$$EnhancerBySpringCGLIB$$a1b2c3"]
```

Then use `methods` to find the exact method signature for `break_in`:
```
methods { "class": "com.example.CartService" }
→ ["void addItem(java.lang.String, int)", "java.util.List getItems()", ...]
```

**Always pass a pattern to `classes`** — without one it returns ALL loaded classes (thousands in a Tomcat JVM).

## Field watchpoints

Use `watch` to find out *who* modifies a field and *when*:
```
watch { "field": "com.example.Config.timeout", "mode": "modification" }
cont
→ Stopped (FieldWatch): field modified at Config.setTimeout() line=42
```

Modes: `modification` (default, catch writes), `access` (catch reads), `all` (both).
`watch` with `mode: "all"` creates separate access and modification watchpoints. You can remove them
independently: `unwatch { "field": "com.example.Config.timeout", "mode": "modification" }` leaves the
access watchpoint active, and `mode: "access"` removes the read watchpoint. Use `mode: "all"` when you want
to remove both classes of watchpoint in one call.

Field watchpoints fire during blocking commands (`run`/`cont`/`step`/`next`/`step_out`) — the response
includes the location, thread, and enriched source context just like breakpoint hits.
On JDI sessions, watchpoints are supported through the same blocking execution controls.

## Common mistakes
- **`locals` empty / "information not available"** → the class was compiled without debug info. Recompile
  with `javac -g`.
- **`run` after attach** → attach has no `run` (the JVM is already running); use `cont`.
- **Breakpoint never hit** → wrong line (e.g. a `}`-only line has no code), or wrong class. Note that
  breakpoints set before the class loads are **deferred** (this is normal) and bind on `run`/`cont`.
- **Calling `list_source`/`where` after every stop** → unnecessary. `Stopped` results already include source
  context and the top stack frame. Only call them if you need the full stack or a different line range.
- **Multiple sessions** → pass `session` (an id from `list`), or keep one session at a time.
- **"a live session already attached to …"** → you (or a previous conversation) already have a session on that
  port. Use `list` to find the existing session id, then reuse it or `kill` it first before re-attaching.
- **`set` string values** → the `value` field is a Java expression. For string literals, pass the value
  directly (e.g. `"TestHeader"`) — the tool auto-quotes values that look like bare strings (contain hyphens,
  slashes, etc.). For numbers, `null`, `true`/`false`, or variable references, pass them unquoted.
- **Wrong JDK picked up** → pass `jdb_path` to `launch` / `attach`, or set `JAVA_HOME`, to force a specific
  JDK (e.g. JDK 8 vs JDK 21).
- **JDI sidecar jar missing** → run `jdbg update` or reinstall from the official release archive. Do not
  globally search for `jdbg-jdi-sidecar.jar` or copy one from a source checkout; it may not match the installed
  binary.
- **JDI `attach`/`launch` fails or times out on JDK 8** → the sidecar needs `tools.jar`
  (JDK 8 ships JDI only there; JDK 9+ bundles it in the runtime). jdbg auto-discovers it via `jdb_path`,
  `JAVA_HOME`, or PATH; if it cannot, set `JDBG_JDI_TOOLS_JAR` to `<jdk8>/lib/tools.jar`, or point `JAVA_HOME`
  at a JDK 8 that has it.
  JDK 9+ is unaffected.
- **`attach` "connection refused" / "not reachable" on a port that IS listening** → dual-stack address
  mismatch: `localhost` resolves to IPv6 `[::1]` but JDWP listens on IPv4 `0.0.0.0` (check `netstat`). jdbg
  auto-normalizes `localhost`→`127.0.0.1`; if you passed some other hostname that still fails, retry with the
  literal `127.0.0.1`.

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

**After a stop, prefer the `thread_id` field from the `Stopped` result** — it is the hit thread's id, ready
to pass to `thread`/`suspend`/`threadlocks`. You only need the `threads` output below when targeting a
*different* thread than the one that stopped.

The `threads` output shows lines like `0x37bd http-nio-8231-exec-8 cond. waiting` (the hit thread, if any, is
prefixed with `*`). **Pass the id exactly as shown** to the `thread` tool — copy it verbatim, do not reformat
it. Do NOT pass the thread **name** (e.g. `http-nio-8231-exec-8`) — jdb rejects names. On a large app, use
`threads { "filter": "http-nio" }` to cut the list down instead of scanning 90+ lines.

The id format depends on the JDK: most print a `0x`-prefixed hex value (`0x37f2`), but some (commonly
external Tomcat / app-server attach) print a plain **decimal** value (`18315`). Use whichever the
`threads` output gave you, unchanged — do NOT add a `0x` prefix to a decimal id, and do NOT strip the
`0x` from a hex id. Both forms are wrong if reformatted, and jdb will reject them.

Example: if `threads` shows `0x37f2 http-nio-8231-exec-20`, use `thread { "id": "0x37f2" }`.
If it shows `18315 http-nio-9702-exec-1`, use `thread { "id": "18315" }`.

Note: if the thread is no longer suspended (e.g. it has finished executing and returned to the pool),
`thread` will report "not a valid thread id" even with the correct id. This indicates the
breakpoint did not hold the thread — see the guidance above about retrying or re-attaching.

### External Tomcat / application servers

When attaching to an external Tomcat (or similar) started with JDWP:
```
-agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5005
```
- `suspend=n` is correct — it means Tomcat doesn't wait for the debugger at startup
- By default, breakpoints use SUSPEND_ALL (all threads freeze). **To avoid freezing heartbeat threads**
  (ZooKeeper, Dubbo registry), use `suspend: "thread"` on your breakpoints:
  ```
  break_at { "class": "com.example.Handler", "line": 100, "suspend": "thread" }
  ```
  This keeps the service registered in ZK/gateway while you inspect the hit thread.
- Without `suspend: "thread"`, the server **will freeze** until you `cont` — inspect quickly
- `kill` resumes the VM before disconnecting so the server continues running afterwards
