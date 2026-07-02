# ROADMAP Post-MVP Completion Plan

## Summary

Implement the remaining `ROADMAP.md` Post-MVP items only: JDI launch mode, JDI method entry/exit events, and concurrent multi-project JDI use for multiple coding agents. Do not implement `DESIGN.md` 7.1 TODOs (`dump` parser hardening, plugin binary naming) in this pass.

## Public API Changes

- Extend existing `break-in` / MCP `break_in` with optional `event: entry|exit|both`, defaulting to `entry`.
- Keep `jdb` behavior unchanged for default method-entry `break_in`; return explicit unsupported-backend errors for `event=exit|both` on `jdb`.
- Support `launch --backend jdi` and MCP `launch { backend: "jdi" }` with existing fields: `main_class`, `classpath`, `sourcepath`, `app_args`, `name`.
- Treat non-empty `jdb_args` on JDI launch as a clear error because it is jdb-specific.

## Implementation

- In Rust, add a JDI launch path in `SessionManager::create_launch`, `JdiSession::launch`, and JDI command dispatch:
  - initial state `Loaded`;
  - `run` allowed only for JDI launch sessions;
  - `cont` remains valid after the target has started;
  - launch-mode `kill` terminates the debuggee via sidecar VM termination, while attach-mode `kill` keeps detach semantics.
- In the Java sidecar, add `launch` using JDI `com.sun.jdi.CommandLineLaunch`, setting connector arguments through `defaultArguments()` only.
- Add method event support in the sidecar with `MethodEntryRequest` and `MethodExitRequest`, exact class filtering, method-name/argument filtering in the event loop, suspend policy support, and method-exit return-value rendering.
- Add `Event::MethodEntry` and `Event::MethodExit` in `protocol::result`, update output rendering, JSON mapping, MCP schema, README, `DESIGN.md`, and both `skills/jdbg/*/SKILL.md`.
- For the multi-agent goal, keep one sidecar per JDI session and one daemon per user. Add a per-`JdiSession` command lock so commands cannot interleave within one session, while different sessions continue in parallel.

## Test Plan

- Add unit tests for `break_in.event` serde defaults, MCP mapping/schema, JDI stop-event mapping, and sidecar method matching/argument filtering.
- Add Java sidecar self-tests for launch argument construction/quoting and method event filter behavior.
- Add integration tests:
  - `launch --backend jdi -> break_at -> run -> locals -> cont`;
  - MCP JDI launch smoke;
  - method entry, method exit with return value, and `both`;
  - `jdb break_in --event exit` returns explicit unsupported error;
  - four concurrent MCP/CLI clients attach to four distinct JDWP targets, each stops and resumes its own session without event/session leakage.
- Verify locally with `cargo test`; on Windows also run `cargo test -- --test-threads=1`.

## Commit, Push, CI

- Confirm worktree state before edits and preserve unrelated user changes.
- Commit the implementation and docs together after tests pass.
- Push current branch `dev-jdi` to `origin` to trigger CI.
- Use `gh run list --branch dev-jdi --limit 5` and `gh run watch <run-id>` to confirm the matrix completes successfully.

## Assumptions

- "Multi-client sidecar" means the requested outcome: four coding agents can debug four different Java projects at the same time. It does not mean multiple debugger clients attach to the same target JVM.
- JDI launch and method events follow Oracle JDI APIs: [`LaunchingConnector`](https://docs.oracle.com/javase/8/docs/jdk/api/jpda/jdi/com/sun/jdi/connect/LaunchingConnector.html), [`MethodEntryRequest`](https://docs.oracle.com/javase/8/docs/jdk/api/jpda/jdi/com/sun/jdi/request/MethodEntryRequest.html), [`MethodExitRequest`](https://docs.oracle.com/javase/8/docs/jdk/api/jpda/jdi/com/sun/jdi/request/MethodExitRequest.html), and [`VirtualMachine.exit`](https://docs.oracle.com/javase/8/docs/jdk/api/jpda/jdi/com/sun/jdi/VirtualMachine.html).
