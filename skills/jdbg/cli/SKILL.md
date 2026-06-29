---
name: jdbg
description: Use the jdbg CLI to debug Java programs interactively from Pi when real runtime state is needed: variables, fields, expressions, exceptions, breakpoints, stepping, threads, locks, or JDWP attach. Pi should use the CLI because jdbg does not install an official Pi MCP server.
compatibility: Requires a JDK 8+ with jdb available through JAVA_HOME, PATH, or --jdb-path. Requires the jdbg CLI on PATH. Native on Windows, Linux, and macOS.
allowed-tools: Bash(jdbg:*), Bash(javac:*), Bash(java:*), Read
metadata:
  version: "1.0"
---

# jdbg CLI - interactive Java debugging for Pi

`jdbg` is a cross-platform CLI wrapper around the JDK's `jdb`. It keeps a stateful background daemon alive, so a debug session survives across separate shell commands.

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

Attach to a running JVM with JDWP enabled:

```bash
jdbg attach --host localhost --port 5005 --sourcepath src/main/java
```

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
```

- `--session <id>` selects a session when more than one is live. Omit it only when exactly one live session exists.
- `--json` prints machine-readable results. Prefer it when parsing output programmatically.
- `--timeout <secs>` overrides the per-command timeout, useful for long `run` or `cont`.
- `--jdb-path <path>` forces a specific `jdb`.

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

Thread-only breakpoint, useful in servers:

```bash
jdbg break-at com.example.Service 87 --suspend thread
```

Method breakpoint:

```bash
jdbg break-in com.example.Service process
jdbg break-in com.example.Service process --args "java.lang.String,int"
```

Exception catchpoint:

```bash
jdbg catch java.lang.NullPointerException --mode all
jdbg ignore java.lang.NullPointerException --mode all
```

Field watchpoint:

```bash
jdbg watch com.example.User.name --mode modification
jdbg watch com.example.User.name --mode access
jdbg unwatch com.example.User.name
```

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
```

## Raw Escape Hatch

Use raw `jdb` commands only when no `jdbg` command covers the need:

```bash
jdbg raw help
jdbg raw monitor where all
```

Prefer structured `jdbg` commands first because they preserve better result parsing.
