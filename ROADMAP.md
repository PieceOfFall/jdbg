# ROADMAP.md - JDI Sidecar Backend

This roadmap captures the follow-up direction for this branch after the exploratory
`PLAN.md`: keep the existing prompt-aware `jdb` backend stable, migrate the MCP
server to `rmcp` for maintainability and call robustness, and add a Java JDI
sidecar backend that can return richer structured runtime facts to the same CLI and
MCP surfaces. The MVP and the first executable JDI post-MVP batch are now
implemented; remaining work is tracked at the end.

This is a roadmap, not a detailed implementation plan. Each milestone below should
be expanded into a focused PLAN before implementation.

## Current Baseline

`jdbg` is currently a Rust CLI/MCP/daemon with two debugger backends. The default
compatibility path wraps the JDK's `jdb` through piped stdio. The optional JDI path
launches or attaches through a local Java sidecar and supports structured stop state, safe
inspect, executable expression evaluation, mutation, watchpoints, and non-void
force return. The daemon owns long-lived sessions, the CLI and MCP server both talk
to the daemon through the existing JSONL IPC protocol, and the MCP server remains a
thin daemon client reachable as `jdbg __mcp` over stdio.

The existing `jdb` path remains the compatibility baseline:

- preserve the locale-forced `jdb` spawn flags and transcript-tested parser behavior;
- keep prompt-aware reads, timeout buffer cleanup, normal-command purge, and
  one in-flight command per session;
- keep raw `jdb` as an escape hatch for commands that are not yet modeled;
- keep the current CLI and MCP behavior green as the JDI backend expands.

## Target Architecture

The target shape is two backends behind the same agent-facing product layer:

```text
Agent / CLI / MCP
  -> jdbg Rust daemon
  -> backend boundary
     -> JdbBackend: existing prompt-aware jdb process control
     -> JdiBackend: structured client for a Java sidecar
  -> Target JVM
```

The Rust daemon remains responsible for the user-facing workflow: CLI parsing, MCP
tool mapping, session registry, output rendering, and setup/update integration.

The Java sidecar owns JDI/JDWP semantics: attach, events, breakpoints, thread and
stack state, locals, value serialization, structured object inspection, expression
evaluation, state mutation, and force return.

The JDI backend communicates with the sidecar over length-prefixed JSON. This
message format is the settled protocol and applies to every transport:

```text
[4-byte big-endian length][UTF-8 JSON payload]
```

Message classes:

- `request`
- `response`
- `event`
- `heartbeat`

Responses must be correlated by request id. Events may arrive between responses and
must be queued for the owning session.

The sidecar transport direction is platform-local:

- Linux/macOS: Rust <-> AF_UNIX socketpair <-> Java sidecar;
- Windows: Rust <-> two one-way Named Pipes <-> Java sidecar.

Do not add gRPC, protobuf, direct Rust JDWP, or any broad RPC framework for the
sidecar path. Future transport work must preserve the length-prefixed JSON frames
and the existing request/response/event/heartbeat message classes.

## Engineering Guardrails

Tokio is approved only for bounded async islands where it improves robustness and
maintainability:

- `rmcp`-based MCP protocol/tool serving behind `jdbg __mcp` stdio;
- JDI platform-local transport;
- framed reads and writes;
- request/response correlation;
- event queues;
- timeouts and cancellation;
- sidecar lifecycle monitoring.

Do not rewrite the entire daemon, CLI, or existing `jdb` backend to async as part of
the first integration. Bridge from current blocking command handling into the
`rmcp` MCP boundary or a dedicated async JDI client boundary.

Keep dependencies narrow:

- add `rmcp` only for MCP protocol/tool serving, not daemon IPC or debugger backend RPC;
- add `tokio` with only the features needed by `rmcp` MCP serving and JDI transport/process work;
- consider `tokio-util` only if the framed codec materially benefits from it;
- do not add gRPC, protobuf, direct Rust JDWP, or broad RPC frameworks.

The implementation keeps Rust-owned lifecycle and replaces the byte stream beneath
the frame codec with platform-local transport:

1. Rust creates the platform-local endpoint: two one-way Named Pipes on Windows,
   or an AF_UNIX socketpair on Linux/macOS. The child-side fd is inherited by the
   Java 8 sidecar because Java 8 has no pathname UDS client API.
2. Rust generates a per-sidecar auth token.
3. Rust launches `jdbg-jdi-sidecar.jar` with the chosen endpoint, token, and
   protocol version.
4. The sidecar connects back to Rust.
5. Rust accepts the connection and performs handshake.

This avoids stdout as protocol, avoids temp ready files, and keeps logs separate from
the framed transport.

## Milestones

### 1. MCP rmcp Migration

Replace the hand-written MCP protocol loop with `rmcp` while preserving the same
agent-facing tool surface and daemon routing.

Decisions to lock in this milestone:

- how each of the 37 current MCP tools is represented as a typed macro-driven rmcp handler;
- how tool input structs map to the existing `protocol::Command` enum;
- how tool-level business errors continue to surface as agent-visible tool errors;
- how stdout remains protocol-only and diagnostics stay on stderr;
- how Windows stdout/stderr handle inheritance protection is preserved before child process spawn.

Scope:

- keep `jdbg __mcp` as the stdio MCP entrypoint;
- route every tool through `client::send_request` and `output::render`;
- keep daemon IPC, `SessionManager`, `Session`, and `jdb` internals unchanged;
- replace current `mcp/tools.rs` tests with rmcp-facing tool schema, typed handler,
  dispatch, and tool-error tests;
- keep setup/update registration behavior unchanged because the executable and `__mcp`
  arguments remain the same.

Acceptance:

- `tools/list` exposes the same 37 tool names with compatible input schemas;
- `tools/call` preserves current success and tool-error semantics;
- MCP end-to-end launch -> break_at -> run -> locals -> cont remains green;
- stdout carries only MCP protocol data;
- no current CLI or daemon workflow regresses.

### 2. Backend Boundary

Introduce backend/session metadata concepts without changing existing `jdb` behavior.

Decisions to lock in this milestone:

- where `--backend jdb|jdi` is accepted;
- how backend kind is represented in session metadata, status, list output, and JSON;
- which current commands are backend-neutral and which are `jdb`-only;
- how unsupported operations are reported for a backend without silently falling back.

Acceptance:

- default behavior remains the existing `jdb` backend;
- `cargo test` stays green;
- session list/status can identify the backend once JDI sessions exist;
- no current CLI or MCP workflow regresses.

### 3. Protocol Foundation

Implement the sidecar protocol types and Rust transport foundation.

Scope:

- frame codec with max frame size;
- JSON request, response, event, heartbeat, and error payloads;
- transport adapter over an AF_UNIX socketpair on Linux/macOS and two one-way
  Named Pipes on Windows, with the same framed JSON payloads on both platforms;
- handshake with protocol version, server version, capabilities, and auth token;
- ping and shutdown methods;
- request timeouts;
- response-id matching;
- interleaved event handling.

Acceptance:

- frame codec handles split frames, coalesced frames, empty payloads, EOF, and
  oversized frames;
- invalid token and unsupported protocol version fail with structured errors;
- logs never enter the framed protocol stream.

### 4. Sidecar Lifecycle

Add a Java sidecar process lifecycle managed by Rust.

Scope:

- package or locate `jdbg-jdi-sidecar.jar`;
- launch the sidecar with endpoint, token, and protocol version arguments;
- supervise process exit and mark affected sessions disconnected;
- keep sidecar stderr/logs separate from protocol messages;
- cleanly detach or shut down sidecar sessions.

Acceptance:

- Rust can launch, handshake, ping, and shut down the sidecar on Windows and
  Unix-like platforms;
- sidecar crash or disconnect produces a structured session-level failure;
- sidecar auth token is never printed in normal logs.

### 5. JDI Attach MVP

Implement the first useful JDI backend capabilities.

Scope:

- attach to `host:port`;
- detach;
- list threads;
- read stack frames;
- report `vmDisconnected` events;
- keep sidecar bytecode/runtime compatible with JDK 8+ targets while source builds
  use JDK 17+ to run Gradle packaging.

Acceptance:

- attach to a fixture JVM started with JDWP;
- list threads through CLI and MCP;
- fetch stack for a suspended thread;
- target JVM exit is surfaced as `vmDisconnected` and the session state updates.

### 6. Breakpoint Loop

Build the basic stop/resume loop on JDI.

Scope:

- line breakpoints;
- deferred breakpoint resolution through class-prepare events;
- continue with timeout;
- step over;
- breakpoint and step events;
- stack and locals at the stop site.

Defaults:

- prefer thread-only suspend for JDI breakpoints where supported;
- resume invalidates frame and value references from the previous stop event;
- `frameId` values are short-lived and tied to `session + thread + stopEventSeq +
  frameIndex`.

Acceptance:

- pending line breakpoint resolves when the class loads;
- continue returns a breakpoint event when the fixture hits the line;
- step-over returns a step event;
- locals are available at the stopped frame.

### 7. Structured Inspect

Implement the core value model that justifies the JDI backend.

Scope:

- primitive, string, object, array, collection, map, enum, null, unavailable,
  cycle reference, and truncated values;
- object field serialization;
- array and collection elements;
- cycle detection;
- explicit truncation and limit flags.

Safe defaults:

- `maxDepth = 3`;
- `maxFields = 100`;
- `maxArrayLength = 50`;
- `maxStringLength = 4096`;
- `maxTotalBytes = 8MB`;
- `maxObjects = 1000`;
- `timeoutMs = 3000`;
- `includeStatic = false`;
- `includeSynthetic = false`;
- `invokeGetters = false`.

Acceptance:

- complex object graphs do not recurse forever;
- large values return explicit truncation metadata;
- getters are not invoked unless a later unsafe feature explicitly enables them.

### 8. CLI and MCP Integration

Expose JDI through the existing product surface.

Scope:

- reuse existing tool names where possible;
- expose backend selection only on session creation;
- avoid requiring every follow-up command to pass backend again;
- render structured JDI results through existing output paths where practical;
- return clear unsupported-operation errors for JDI gaps.

Acceptance:

- default `jdbg attach` still uses `jdb`;
- `jdbg attach --backend jdi` creates a JDI session;
- MCP `attach` can create a JDI session through an optional `backend` parameter;
- subsequent commands route by session backend;
- current `jdb` tests and MCP tool mapping tests remain green.

### 9. Executable JDI Expressions And Mutation

Add the explicit side-effecting JDI capabilities while keeping `inspect` safe.

Scope:

- parse Java expressions in the sidecar with JavaParser and evaluate against the
  current suspended JDI frame;
- route JDI `print`, `eval`, and `dump` through `evaluateExpression`;
- support instance/static method invocation, local/field/array access, primitive
  operators, casts, and overload resolution where the sidecar evaluator supports it;
- route `set` through sidecar `setValue` so locals, fields, and array elements can
  be assigned;
- add CLI `force-return <value>` and MCP `force_return { value }`, implemented with
  `ThreadReference.forceEarlyReturn`;
- reject running/dead/exited sessions and unsupported void force return clearly.

Acceptance:

- expression integration covers local arithmetic, instance methods, static methods,
  arrays, casts, and overloaded calls;
- `set` integration mutates locals, fields, and array elements and verifies the
  changed state through follow-up public commands;
- `force-return` replaces the current non-void return value and the caller observes
  the replacement;
- `inspect` remains field-reading only and does not invoke getters.

### 10. Release Readiness

Update project-facing docs and release metadata once public behavior changes.

Scope:

- `DESIGN.md`;
- `README.md`;
- `skills/jdbg/mcp/SKILL.md`;
- `skills/jdbg/cli/SKILL.md`;
- version metadata in changed skill files;
- `Cargo.toml` version when preparing a release.

Acceptance:

- docs describe both backends accurately;
- skills teach agents when to choose JDI vs jdb;
- release checklist remains accurate for cargo-dist.

## Current Implementation Status

This section tracks the current branch state against the roadmap above.

### Implemented

- Milestone 1, MCP rmcp migration: `jdbg __mcp` now uses `rmcp` over stdio,
  preserves the 37-tool catalog, routes tool calls through `client::send_request`
  and `output::render`, and keeps tool-level errors agent-visible.
- Milestone 2, backend boundary: CLI and MCP session creation accept
  `backend: jdb|jdi`; session creation, list, status, registry records, and output
  include backend metadata; unsupported JDI commands fail explicitly.
- Milestone 3, protocol foundation: Rust has length-prefixed JSON framing,
  request/response/event/heartbeat protocol types, handshake validation, auth token
  checks, request timeouts, response id matching, and interleaved event queuing.
- Milestone 4, sidecar lifecycle: `build.rs` packages `jdbg-jdi-sidecar.jar`;
  Rust launches the Java sidecar over platform-local transport with a per-process
  token, keeps stdout out of protocol traffic, captures stderr separately, uses
  no-window process launch on Windows, and shuts the sidecar down with the
  session. Windows uses two one-way Named Pipes; Linux/macOS use an AF_UNIX
  socketpair.
- Milestone 5, JDI attach MVP: `jdbg attach --backend jdi` can attach to a JDWP
  target, detach, list threads, read stacks, and surface VM-disconnect events into
  session state.
- Milestone 6, breakpoint loop: JDI supports line breakpoints, deferred
  class-prepare resolution, continue-with-timeout, step over, breakpoint/step
  events, stack, locals, and startup-suspended targets without racing past deferred
  breakpoints.
- Milestone 7, structured inspect: JDI inspect renders primitives, strings,
  objects, arrays, collections, maps, enums, null, unavailable values, cycle
  references, and truncation metadata without invoking getters.
- Milestone 8, CLI and MCP integration: default session creation remains `jdb`;
  `launch --backend jdi` and `attach --backend jdi` create JDI sessions; follow-up
  commands route by session backend; JDI supports `break-at`, method `break-in`
  entry/exit events, `watch`, `unwatch`, `run`, `cont`, `next`, `where`, `locals`,
  `threads`, `thread`, `inspect`, expression `print`/`eval`/`dump`, `set`, and
  non-void `force-return`.
- Milestone 9, executable JDI expressions and mutation: the sidecar Gradle fat jar
  bundles JavaParser, parses Java expressions in the sidecar, evaluates them against
  the suspended JDI frame/object graph, supports instance/static method invocation,
  local/field/array reads, primitive operators, casts, `setValue`, and
  `ThreadReference.forceEarlyReturn` for non-void returns. Safe `inspect` remains
  field-reading only and does not invoke getters.
- Milestone 10, release readiness: `README.md`, `DESIGN.md`, both installed skills,
  and `Cargo.toml` metadata have been updated for the public JDI/rmcp behavior.
- Post-MVP JDI launch and method events: `launch --backend jdi` starts a JDI-launched VM in
  `Loaded`, `run` resumes it, method `break_in` supports entry/exit/both events on JDI,
  method-exit stops render return values, and `jdb` rejects exit/both method events explicitly.
- Post-MVP multi-project concurrency: the daemon keeps one sidecar per JDI session and each
  `JdiSession` serializes its own commands while distinct sessions can run in parallel.
- Setup integration beyond the original roadmap: `jdbg setup` can record an
  installed-skill backend preference through interactive TUI selection or
  `--backend jdb|jdi`; `jdbg update` preserves that preference when re-registering
  agents.
- JDK 8 sidecar compatibility: the sidecar source and bytecode target remain Java 8
  compatible, runtime adds `tools.jar` on JDK 8 when needed, and source builds use
  a JDK 17+ Gradle JVM via `JDBG_GRADLE_JAVA_HOME` when the debug target JVM is
  older.

### Implemented Hardening

- Dedicated fixture coverage now verifies JDI `vmDisconnected` target exit,
  detach/kill, sidecar-process death, and status transitions through the public
  session surface.
- JDI step-over now has fixture-based integration coverage for the returned step
  stop site, top stack frame, and locals at the stopped frame.
- MCP now has a JDI end-to-end smoke covering `attach -> break_at -> cont ->
  locals -> inspect -> kill` through `jdbg __mcp`, the daemon, and the JDI
  sidecar.
- JDI watchpoints now support access, modification, and all modes, including
  partial `unwatch` semantics for deferred and active watchpoints; MCP has a
  JDI watch/unwatch smoke through the public tool surface.
- Java sidecar self-tests cover JSON protocol parsing/serialization, sidecar
  token/config validation, stable unknown-method RPC errors, and value-rendering
  string limits.
- JDI expression integration covers local arithmetic, instance method calls, static
  method calls, array access, field mutation, array mutation, and force-return with
  caller-observed replacement values.
- Unexpected sidecar process exit while the Rust daemon still holds a JDI session
  now marks the session `Dead`; `status` reports `jdb_alive=false` and follow-up
  operations fail explicitly instead of falling back to `jdb`.
- `daemon stop` now returns a response before exiting through a daemon-local
  shutdown flag, and startup detaches cleanly on Unix via `setsid` while Windows
  clears inherited stdout/stderr handles before spawning the background daemon.
- CI now runs `cargo test` on Windows, Linux, and macOS across JDK 8, 11, 17,
  and 21. Windows runs tests serially to avoid JDWP/JDI fixture process contention.
- Structured inspect covers common `ArrayList`, `LinkedList`, `ArrayDeque`,
  `HashSet`, `LinkedHashSet`, `TreeMap`, `TreeSet`, `HashMap`, `LinkedHashMap`,
  and unmodifiable collection/map layouts without getter invocation.

### Pending MVP Follow-Ups

No pending MVP follow-ups remain in this roadmap. Post-MVP work is tracked below.

## Test Strategy

Keep `cargo test` green throughout and preserve all existing parser, reader, MCP,
daemon, output, and real-`jdb` integration coverage.

Rust protocol tests should cover:

- normal frame;
- consecutive frames;
- split reads;
- payloads containing newlines;
- empty frame;
- EOF during header or body;
- oversized frame;
- handshake success;
- bad token;
- unsupported protocol version;
- unknown method;
- invalid params;
- request timeout;
- response-id matching;
- events interleaved with responses.

Java sidecar tests should cover:

- protocol parsing and serialization;
- token validation;
- RPC routing;
- JDI service errors mapped to stable error codes;
- value serialization limits;
- event-loop stop/resume behavior.

Fixture-based JDI integration tests should cover:

- attach;
- threads;
- line breakpoint;
- deferred breakpoint;
- continue to breakpoint;
- stack;
- locals;
- inspect;
- expression print/eval/dump;
- set local, field, and array element;
- non-void force-return;
- detach;
- target VM disconnect.

CLI/MCP end-to-end tests should prove:

- current `jdb` backend behavior is unchanged;
- implemented JDI commands work through the same public tools;
- unsupported JDI commands fail explicitly rather than falling back silently.

## Post-MVP Work

No ROADMAP-tracked Post-MVP work remains open in this branch.

The following are closed non-goals for this roadmap:

- protobuf;
- gRPC;
- direct Rust JDWP;

## Guiding Principle

The final shape should be easy to explain:

```text
jdbg keeps the existing prompt-aware jdb backend for compatibility and raw command
escape hatches, while adding a native JDI backend through a local Java sidecar for
structured events, stack/locals, safe object inspection, and explicit executable
eval/set/force-return operations.
```
