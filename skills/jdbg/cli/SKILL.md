---
name: "jdbg"
description: "Use the jdbg CLI to debug Java programs interactively from Pi when real runtime state is needed: variables, fields, expressions, exceptions, breakpoints, stepping, threads, locks, or JDWP attach. Pi should use the CLI because jdbg does not install an official Pi MCP server."
compatibility: "Requires a JDK 8+ with jdb available through JAVA_HOME, PATH, or --jdb-path. Requires the jdbg CLI on PATH. Native on Windows, Linux, and macOS."
allowed-tools: "Bash(jdbg:*), Bash(javac:*), Bash(java:*), Read"
metadata:
  version: "1.20"
---

# jdbg CLI - interactive Java debugging for Pi

`jdbg` is a cross-platform CLI wrapper around the JDK's `jdb`, with an optional JDI sidecar backend for structured runtime data. It keeps a stateful background daemon alive, so a debug session survives across separate shell commands.

Pi has no official jdbg MCP setup. Use the `jdbg` CLI directly.

Core loop: run one `jdbg` command, read the result, then decide the next command. Do not batch debugger commands, add sleeps, or guess timing. `jdbg` waits for the `jdb` prompt internally; long-running commands return `TIMEOUT` without killing the session.

## When To Use

Use `jdbg` when you need runtime truth:

- the value of a variable, field, array element, collection, or expression at a line
- why an exception or `NullPointerException` is thrown, with stack and locals at the throw site
- how execution reaches code, by stepping line by line
- what a blocked or deadlocked thread is waiting on
- who reads or modifies a field, using watchpoints

Do not use it for plain compile errors or code that is simple enough to reason about statically.

## Start A Session

Compile Java with debug info when possible:

```bash
javac -g Main.java
```

Launch a program under the debugger:

```bash
jdbg launch Main --classpath . --sourcepath src
```

Launch with application arguments after `--`:

```bash
jdbg launch com.example.Main --classpath target/classes --sourcepath src/main/java -- arg1 arg2
```

Launch through the JDI sidecar when you need structured JDI data:

```bash
jdbg launch Main --backend jdi --classpath . --sourcepath src
```

Attach to a running JVM with JDWP enabled:

```bash
jdbg attach --host localhost --port 5005 --sourcepath src/main/java
```

Use the JDI sidecar for structured attach debugging:

```bash
jdbg attach --backend jdi --host localhost --port 5005 --sourcepath src/main/java
```

If `--sourcepath` is omitted, jdbg uses the shell's current working directory as the source root and sends it
to the daemon as an absolute path. Relative `--sourcepath` values are also absolutized before the daemon sees
them, so source lookup is stable even when the daemon was started from another directory. On JDI sessions,
source lookup also tries the target JVM's `user.dir` and Maven/Gradle module roots inferred from
`java.class.path` (for example `mall-portal/target/classes` -> `mall-portal/src/main/java`).

The default backend is `jdi`. If the local JDI prerequisites are missing (for example the sidecar jar is
absent, or a JDK 8 sidecar JVM cannot find `tools.jar`), an omitted `--backend` automatically falls back to
`jdb` and returns a note with the reason. Explicit `--backend jdi` means JDI is required and does not fall
back. The JDI backend supports the normal debugging surface too: line and method
breakpoints (including conditional breakpoints, evaluated server-side), exception catchpoints, watchpoints,
stepping, stack frames, classes/methods, source listing, thread control, locks, safe JSON inspect, executable
print/eval/dump, set, and non-void force-return. JDI `raw` dispatches supported jdb-style aliases through the
sidecar rather than writing to a literal jdb stdin. The only things that require a `jdb` session are literal jdb
stdin passthrough (`monitor`, `redefine`, `trace`, and other jdb-only commands) and void `force_return`;
conditional breakpoints do **not** need `jdb`.

If the JDK is not the one you need, pass:

```bash
jdbg --jdb-path /path/to/jdb launch Main --classpath .
```

On Windows, prefer the JDK from `JAVA_HOME` when debugging JDK 8 applications.

## Enable JDWP For Attach

Start the target JVM with:

```bash
-agentlib:jdwp=transport=dt_socket,server=y,suspend=y,address=<ADDR>
```

JDK 8:

```bash
-agentlib:jdwp=transport=dt_socket,server=y,suspend=y,address=5005
```

JDK 9+:

```bash
-agentlib:jdwp=transport=dt_socket,server=y,suspend=y,address=*:5005
```

Use `suspend=y` when you want the JVM to wait for the debugger before application code runs. Use `suspend=n` to attach to an already-running app.

## Basic Workflow

For a launched session:

```bash
jdbg launch Main --classpath . --sourcepath .
jdbg break-at Main 9
jdbg run
jdbg locals
jdbg where
jdbg print myVar
jdbg next
jdbg cont
jdbg kill
```

For an attached session:

```bash
jdbg attach --host localhost --port 5005 --sourcepath src/main/java
jdbg break-at com.example.Service 42
jdbg cont
jdbg locals
jdbg print this.userId
jdbg cont
```

Use `run` only for launch mode. Use `cont` for attach mode and after the program has started.

## Important Options

Most commands accept these global flags before the subcommand:

```bash
jdbg --session <id> locals
jdbg --json status
jdbg --timeout 60 cont
jdbg --jdb-path C:\Users\you\.jdks\jdk8\bin\jdb.exe launch Main --classpath .
jdbg attach --backend jdi --host localhost --port 5005
```

- `--session <id>` selects a session when more than one is live. Omit it only when exactly one live session exists.
- `--json` prints machine-readable results. Prefer it when parsing output programmatically.
- `--timeout <secs>` overrides the per-command timeout, useful for long `run` or `cont`.
- `--jdb-path <path>` forces a specific `jdb`. For JDI, it also selects the sidecar JDK.
- `--backend jdb|jdi` is accepted only on `launch` and `attach`; omit it for the default `jdi` backend.
  Use `--backend jdb` when you need literal raw jdb stdin passthrough or jdb-only commands.

Source builds create `jdbg-jdi-sidecar.jar` during `cargo build` by running the Gradle wrapper in
`sidecar/jdi`; this requires a JDK 17+ build JVM. Debug targets still support JDK 8+. Set
`JDBG_GRADLE_JAVA_HOME` when the Gradle build JDK differs from the target/debuggee JDK. Override sidecar
discovery with `JDBG_JDI_SIDECAR_JAR` or the Java runtime with `JDBG_JDI_JAVA` only when needed. On JDK 8 the
sidecar also needs `tools.jar` (JDK 9+ bundles JDI); jdbg finds it via `--jdb-path`, `JAVA_HOME`, or PATH, or
set `JDBG_JDI_TOOLS_JAR` to `<jdk8>/lib/tools.jar` if attach/launch fails because it cannot be found.
Release updates install the official sidecar jar next to the `jdbg` binary. If it is missing, run
`jdbg update` or reinstall from the official release archive; do not search the filesystem and copy a jar
from a source checkout or unrelated build. When `--backend` is omitted, missing JDI prerequisites fall back
to `jdb` with a note; explicit `--backend jdi` reports the error.

List sessions and inspect state:

```bash
jdbg list
jdbg status
jdbg daemon status
```

Stop everything when done:

```bash
jdbg kill
jdbg daemon stop
```

For JDI, `status` changes to `suspended` as soon as the sidecar receives a stop. In JSON,
`pending_stops > 0` means a stop is waiting for an execution-control command to consume it, rather than an
idle running VM. JDI source snippets read UTF-8 first and fall back to GBK for legacy Chinese Java files.
`resume` without an id discards all pending JDI stops and clears `last_event`; use it when intentionally
continuing past an asynchronously observed hit.

## Breakpoints And Watchpoints

Line breakpoint:

```bash
jdbg break-at com.example.Main 42
```

Conditional breakpoint:

```bash
jdbg break-at com.example.Service 87 --condition "userId == 123"
```

False conditional hits auto-continue. On the **JDI** backend the condition is evaluated server-side, so
`cont`/`run`/`step` only stop when it holds. On the **jdb** backend against an already-running attached JVM, if a
conditional breakpoint fires before your next blocking command, the next inspection command (`threads`, `where`,
`print`, `locals`, etc.) first resolves the condition.

Thread-only breakpoint, useful in servers:

```bash
jdbg break-at com.example.Service 87 --suspend thread
```

Method breakpoint:

```bash
jdbg break-in com.example.Service process
jdbg break-in com.example.Service process --args "java.lang.String,int"
jdbg break-in com.example.Service process --event exit --args "java.lang.String,int"
```

`--event entry` is the default. JDI sessions also support `--event exit` and `--event both`; method-exit stops
include a rendered return value when JDI exposes it. The `jdb` backend supports entry only and rejects exit/both
explicitly.

Exception catchpoint:

```bash
jdbg catch java.lang.NullPointerException --mode all
jdbg ignore java.lang.NullPointerException --mode all
```

Field watchpoint:

```bash
jdbg watch com.example.User.name --mode modification
jdbg watch com.example.User.name --mode access
jdbg watch com.example.User.name --mode all
jdbg unwatch com.example.User.name --mode modification
jdbg unwatch com.example.User.name --mode access
```

`--mode all` sets separate access and modification watchpoints. Remove either side independently with
`unwatch --mode modification` or `unwatch --mode access`, or remove both with `unwatch --mode all`.

Manage breakpoints:

```bash
jdbg breakpoints
jdbg clear com.example.Main:42
```

## Inspect Runtime State

Stack and locals:

```bash
jdbg where
jdbg where --all
jdbg locals
```

Values:

```bash
jdbg print "this.userId"
jdbg dump "this"
jdbg eval "items.size()"
```

Collections and arrays:

```bash
jdbg inspect "items" --max-elements 20
```

On JDI sessions, safe JSON `inspect` reads fields directly and does not invoke getters. It covers common
`ArrayList`, `LinkedList`, `ArrayDeque`, `HashSet`, `LinkedHashSet`, `TreeMap`, `TreeSet`, `HashMap`,
`LinkedHashMap`, unmodifiable wrappers, arrays, and ordinary objects. It resolves locals, `this`, instance and
static fields, field chains, and array access, but rejects method calls (e.g. `map.keySet()`) with a
`method_invocation_not_allowed` error — use `print`/`eval` for those.

On JDI sessions, `print`, `eval`, `dump`, `set`, and `force-return` are executable capabilities. They may
invoke methods in the target JVM and can have side effects. Use `print`/`eval` for anything with a method call;
use `inspect` when you need safe field-reading without getters.

An executable evaluation can block in target code. If it times out, do not retry it: `evaluation_in_progress`
means the first call is still running. Use `jdbg threads`, `jdbg clear ...`, `jdbg resume`, or `jdbg kill` to
recover; JDI cannot cancel the original method call. Executable evaluation requires a breakpoint/step/exception
stop. A manually suspended thread is not an evaluation site, so set a breakpoint in the target method and wait
for `Stopped` instead.

Source context:

```bash
jdbg list-source
jdbg list-source 120
```

`run`, `cont`, `step`, `next`, and `step-out` stop output includes the top frame and surrounding source context
when available. Use `list-source` only when you need a different line range or want to re-read source while stopped.

Classes and methods:

```bash
jdbg classes Service
jdbg methods com.example.Service
```

Always pass a pattern to `classes`; without one, large app servers can return thousands of loaded classes.

## Step And Control Execution

```bash
jdbg step
jdbg next
jdbg step-out
jdbg cont
```

- `step` enters method calls.
- `next` stays in the current frame.
- `step-out` runs until the current method returns.
- `cont` resumes until the next breakpoint, exception, step completion, VM exit, or timeout.

On JDI, `step`, `next`, and `step-out` remain on the stopped thread; unrelated thread events are resumed while
the step is active and cannot replace its result.

If output says `TIMEOUT`, the debuggee is still alive. Inspect with `status`, `threads`, or stop it with `kill`.

## Threads And Locks

List threads, optionally filtered:

```bash
jdbg threads
jdbg threads --filter http-nio
```

Switch thread or frame:

```bash
jdbg thread 0x23
jdbg frame up 1
jdbg frame down 1
```

Suspend/resume:

```bash
jdbg suspend 0x23
jdbg resume 0x23
jdbg suspend
jdbg resume
```

Locks:

```bash
jdbg lock "this"
jdbg threadlocks 0x23
```

Use the exact thread id from `jdbg threads`. Some JDKs print decimal ids; pass them exactly as shown.

## Mutating State

Only change program state when it is useful and safe for the debugging task:

```bash
jdbg set "this.count" "42"
jdbg set "arr[0]" "\"patched\""
jdbg force-return "123"
```

`force-return` is JDI-only and currently supports non-void methods. It mutates control flow by forcing the
current suspended method to return the evaluated value expression. The forced return is applied when the
thread resumes; a `where` command before the next `cont`/`step` may still show the old frame and include a
note explaining that pending refresh.

## Raw Escape Hatch

Use raw `jdb` commands only when no `jdbg` command covers the need:

```bash
jdbg launch Main --backend jdb --classpath .
jdbg raw help
jdbg raw monitor where all
```

Prefer structured `jdbg` commands first because they preserve better result parsing. On JDI sessions, `raw`
only dispatches known aliases and cannot execute literal jdb stdin commands.
