---
name: "jdbg"
description: "Use the jdbg CLI to debug Java programs interactively from Pi when real runtime state is needed: variables, fields, expressions, exceptions, breakpoints, stepping, threads, locks, or JDWP attach. Pi should use the CLI because jdbg does not install an official Pi MCP server."
compatibility: "Requires a JDK 8+ with jdb available through JAVA_HOME, PATH, or --jdb-path. Requires the jdbg CLI on PATH. Native on Windows, Linux, and macOS."
allowed-tools: "Bash(jdbg:*), Bash(javac:*), Bash(java:*), Read"
metadata:
  version: "1.9"
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

The default backend is `jdb`. The JDI backend supports the normal debugging surface too: breakpoints,
exception catchpoints, watchpoints, stepping, stack frames, classes/methods, source listing, thread control,
locks, safe JSON inspect, executable print/eval/dump, set, and non-void force-return. JDI `raw` dispatches
supported jdb-style aliases through the sidecar rather than writing to a literal jdb stdin.

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
- `--jdb-path <path>` forces a specific `jdb`.
- `--backend jdb|jdi` is accepted only on `launch` and `attach`; omit it for the full `jdb` backend.

Source builds create `jdbg-jdi-sidecar.jar` during `cargo build` by running the Gradle wrapper in
`sidecar/jdi`; this requires a JDK 17+ build JVM. Debug targets still support JDK 8+. Set
`JDBG_GRADLE_JAVA_HOME` when the Gradle build JDK differs from the target/debuggee JDK. Override sidecar
discovery with `JDBG_JDI_SIDECAR_JAR` or the Java runtime with `JDBG_JDI_JAVA` only when needed.
Release updates install the official sidecar jar next to the `jdbg` binary. If it is missing, run
`jdbg update` or reinstall from the official release archive; do not search the filesystem and copy a jar
from a source checkout or unrelated build.

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

## Breakpoints And Watchpoints

Line breakpoint:

```bash
jdbg break-at com.example.Main 42
```

Conditional breakpoint:

```bash
jdbg break-at com.example.Service 87 --condition "userId == 123"
```

False conditional hits auto-continue. In an already-running attached JVM, if a conditional breakpoint fires
before your next blocking command, the next inspection command (`threads`, `where`, `print`, `locals`, etc.)
first resolves the condition.

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
`LinkedHashMap`, unmodifiable wrappers, arrays, and ordinary objects.

On JDI sessions, `print`, `eval`, `dump`, `set`, and `force-return` are executable capabilities. They may
invoke methods in the target JVM and can have side effects. Use `inspect` when you need safe field-reading
without getters or method calls.

Source context:

```bash
jdbg list-source
jdbg list-source 120
```

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
current suspended method to return the evaluated value expression.

## Raw Escape Hatch

Use raw `jdb` commands only when no `jdbg` command covers the need:

```bash
jdbg raw help
jdbg raw monitor where all
```

Prefer structured `jdbg` commands first because they preserve better result parsing.
