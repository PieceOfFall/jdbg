//! Single-connection handler: decode JSONL Request, route command, encode Response.

use std::io::{BufRead, BufReader, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use interprocess::local_socket::Stream;

use super::manager::SessionManager;
use crate::backend::DebugSession;
use crate::error::{Error, Result};
use crate::jdb::parser::CommandHint;
use crate::jdi::session::JdiSession;
use crate::protocol::*;
use crate::session::{CommandKind, Session};

/// Handle one connection: one request → one response.
pub fn handle_connection(
    stream: Stream,
    mgr: &Arc<SessionManager>,
    shutdown: &Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;

    if line.trim().is_empty() {
        return Ok(());
    }

    let req: Request = match serde_json::from_str(&line) {
        Ok(r) => r,
        Err(e) => {
            let resp = Response::err("?", 400, format!("invalid request JSON: {e}"));
            write_response(&stream, &resp)?;
            return Ok(());
        }
    };

    let resp = dispatch(&req, mgr, shutdown);
    write_response(&stream, &resp)?;
    Ok(())
}

/// Route a command to concrete handling logic.
fn dispatch(req: &Request, mgr: &Arc<SessionManager>, shutdown: &AtomicBool) -> Response {
    let id = &req.id;
    match &req.cmd {
        // ── Session lifecycle ──
        Command::Launch {
            main_class,
            backend,
            classpath,
            sourcepath,
            app_args,
            jdb_args,
            name,
            jdb_path,
        } => {
            match mgr.create_launch(super::manager::LaunchParams {
                main_class: main_class.clone(),
                backend: *backend,
                classpath: classpath.clone(),
                sourcepath: sourcepath.clone(),
                app_args: app_args.clone(),
                jdb_args: jdb_args.clone(),
                name: name.clone(),
                jdb_path: jdb_path.clone(),
            }) {
                Ok(session) => {
                    let result = CommandResult::SessionCreated {
                        session: session.id().to_string(),
                        mode: session.mode(),
                        backend: session.backend(),
                        target: session.target().to_string(),
                        state: session.state(),
                    };
                    Response::ok(
                        id,
                        CommandResponse {
                            result,
                            stderr: None,
                            note: None,
                        },
                    )
                }
                Err(e) => Response::err(id, e.exit_code(), e.to_string()),
            }
        }
        Command::Attach {
            backend,
            host,
            port,
            sourcepath,
            name,
            jdb_path,
        } => {
            match mgr.create_attach(super::manager::AttachParams {
                backend: *backend,
                host: host.clone(),
                port: *port,
                sourcepath: sourcepath.clone(),
                name: name.clone(),
                jdb_path: jdb_path.clone(),
            }) {
                Ok(session) => {
                    let result = CommandResult::SessionCreated {
                        session: session.id().to_string(),
                        mode: session.mode(),
                        backend: session.backend(),
                        target: session.target().to_string(),
                        state: session.state(),
                    };
                    // If the entry point normalized localhost to 127.0.0.1, say so explicitly.
                    // On dual-stack machines localhost→::1 while JDWP often listens on IPv4; normalization avoids connection refused.
                    let note = crate::jdb::process::normalize_attach_host(host)
                        .ne(host)
                        .then(|| format!(
                            "host '{host}' normalized to 127.0.0.1 (IPv4 loopback): on dual-stack \
                             hosts 'localhost' may resolve to IPv6 [::1] but JDWP usually listens \
                             only on IPv4. target shows the address actually connected."
                        ));
                    Response::ok(
                        id,
                        CommandResponse {
                            result,
                            stderr: None,
                            note,
                        },
                    )
                }
                Err(e) => Response::err(id, e.exit_code(), e.to_string()),
            }
        }
        Command::List => {
            let result = mgr.list();
            Response::ok(
                id,
                CommandResponse {
                    result,
                    stderr: None,
                    note: None,
                },
            )
        }
        Command::Kill => {
            // Resolve target session (None = unique live session), consistent with other commands' --session default.
            match mgr.get(req.session.as_deref()) {
                Ok(session) => {
                    let sid = session.id().to_string();
                    match mgr.kill(&sid) {
                        Ok(()) => Response::ok(
                            id,
                            CommandResponse {
                                result: CommandResult::Raw {
                                    text: format!("session {sid} killed"),
                                },
                                stderr: None,
                                note: None,
                            },
                        ),
                        Err(e) => Response::err(id, e.exit_code(), e.to_string()),
                    }
                }
                Err(e) => Response::err(id, e.exit_code(), e.to_string()),
            }
        }
        Command::Status => match mgr.get(req.session.as_deref()) {
            Ok(session) => {
                let result = session.status();
                Response::ok(
                    id,
                    CommandResponse {
                        result,
                        stderr: None,
                        note: None,
                    },
                )
            }
            Err(e) => Response::err(id, e.exit_code(), e.to_string()),
        },

        // ── Daemon control ──
        Command::DaemonStatus => {
            let result = CommandResult::Raw {
                text: format!("daemon pid={} running", std::process::id()),
            };
            Response::ok(
                id,
                CommandResponse {
                    result,
                    stderr: None,
                    note: None,
                },
            )
        }
        Command::DaemonStop => daemon_stop_response(id, shutdown),

        // ── All session-bound commands ──
        _ => dispatch_session_cmd(req, mgr),
    }
}

/// Route commands that require a concrete session.
fn dispatch_session_cmd(req: &Request, mgr: &Arc<SessionManager>) -> Response {
    let id = &req.id;
    let session = match mgr.get(req.session.as_deref()) {
        Ok(s) => s,
        Err(e) => return Response::err(id, e.exit_code(), e.to_string()),
    };
    match session {
        DebugSession::Jdb(session) => dispatch_jdb_session_cmd(req, session),
        DebugSession::Jdi(session) => dispatch_jdi_session_cmd(req, session),
    }
}

fn daemon_stop_response(id: &str, shutdown: &AtomicBool) -> Response {
    shutdown.store(true, Ordering::SeqCst);
    Response::ok(
        id,
        CommandResponse {
            result: CommandResult::Raw {
                text: "daemon stopping".into(),
            },
            stderr: None,
            note: None,
        },
    )
}

fn dispatch_jdb_session_cmd(req: &Request, session: Arc<Session>) -> Response {
    let id = &req.id;
    // Timeout override for this request (CLI `--timeout`); None means each command's default.
    let t = req.timeout;
    let precondition_note = match settle_async_condition_breakpoint(&session, &req.cmd, t) {
        AsyncConditionResolution::Continue { note } => note,
        AsyncConditionResolution::Return(resp) => return Response::ok(id, resp),
    };

    let result = match &req.cmd {
        // Breakpoints
        Command::BreakAt {
            class,
            line,
            condition,
            suspend,
        } => {
            // The suspend policy is encoded into the jdb breakpoint command (`stop thread at`), so no hit-time repair is needed.
            let r = session.stop_at(class, *line, suspend.as_deref(), t);
            if r.is_ok() {
                session.record_break_target(class, *line);
                let spec = format!("{class}:{line}");
                if let Some(cond) = condition {
                    session.add_condition(&spec, cond);
                }
            }
            r
        }
        Command::BreakIn {
            class,
            method,
            event,
            args,
            condition,
            suspend,
        } => {
            if *event != MethodEventKind::Entry {
                let error = Error::UnsupportedBackend {
                    backend: "jdb".into(),
                    operation: format!("break_in --event {event:?}").to_lowercase(),
                };
                return Response::err(id, error.exit_code(), error.to_string());
            }
            let r = session.stop_in(class, method, args.as_deref(), suspend.as_deref(), t);
            if r.is_ok() {
                let spec = match args {
                    Some(a) => format!("{class}.{method}({a})"),
                    None => format!("{class}.{method}"),
                };
                if let Some(cond) = condition {
                    session.add_condition(&spec, cond);
                }
            }
            r
        }
        Command::Catch { exception, mode } => {
            let cmd = match mode.as_str() {
                "caught" => format!("catch caught {exception}"),
                "uncaught" => format!("catch uncaught {exception}"),
                _ => format!("catch {exception}"),
            };
            session.execute(
                &cmd,
                CommandKind::normal(CommandHint::BreakpointSet).with_timeout_secs(t),
            )
        }
        Command::Watch { field, mode } => {
            let cmd = watch_command(field, mode);
            session.execute(
                &cmd,
                CommandKind::normal(CommandHint::WatchSet).with_timeout_secs(t),
            )
        }
        Command::Unwatch { field, mode } => {
            let cmd = unwatch_command(field, mode);
            session.execute(
                &cmd,
                CommandKind::normal(CommandHint::Other).with_timeout_secs(t),
            )
        }
        Command::Breakpoints => session.execute(
            "clear",
            CommandKind::normal(CommandHint::Breakpoints).with_timeout_secs(t),
        ),
        Command::Clear { spec } => session.execute(
            &format!("clear {spec}"),
            CommandKind::normal(CommandHint::Other).with_timeout_secs(t),
        ),

        // Execution control (blocking — enrich after)
        Command::Run | Command::Cont | Command::Step | Command::Next | Command::StepOut => {
            let r = match &req.cmd {
                Command::Run => session.run(t),
                Command::Cont => session.cont(t),
                Command::Step => session.step(t),
                Command::Next => session.next(t),
                Command::StepOut => session.step_out(t),
                _ => unreachable!(),
            };
            match r {
                Ok(mut resp) => {
                    resp = eval_condition_loop(&session, resp, t);
                    enrich_stopped(&session, &mut resp);
                    enrich_thread_id(&session, &mut resp);
                    check_line_mismatch(&session, &mut resp);
                    return Response::ok(id, resp);
                }
                Err(e) => return Response::err(id, e.exit_code(), e.to_string()),
            }
        }

        // Inspection — inspect (composite, multi-command)
        Command::Inspect { expr, max_elements } => {
            match handle_inspect(&session, expr, *max_elements, t) {
                Ok(mut resp) => {
                    append_note_if_present(&mut resp, precondition_note.as_deref());
                    return Response::ok(id, resp);
                }
                Err(e) => return Response::err(id, e.exit_code(), e.to_string()),
            }
        }

        // Class/method search
        Command::Classes { pattern } => {
            let cmd = match pattern {
                Some(p) => format!("classes {p}"),
                None => "classes".to_string(),
            };
            let mut r = session.execute(
                &cmd,
                CommandKind::normal(CommandHint::Classes).with_timeout_secs(t),
            );
            // Client-side filter: jdb may not filter classes server-side on some JDK versions.
            // Apply case-insensitive substring filter to guarantee pattern works.
            if let Some(p) = pattern {
                if let Ok(ref mut resp) = r {
                    if let CommandResult::Classes { ref mut classes } = resp.result {
                        let needle = p.to_lowercase();
                        classes.retain(|c| c.to_lowercase().contains(&needle));
                    }
                }
            }
            r
        }
        Command::Methods { class } => {
            let mut r = session.execute(
                &format!("methods {class}"),
                CommandKind::normal(CommandHint::Methods).with_timeout_secs(t),
            );
            if let Ok(ref mut resp) = r {
                if let CommandResult::Methods {
                    class: ref mut c, ..
                } = resp.result
                {
                    *c = class.clone();
                }
            }
            r
        }

        // Inspection (simple)
        Command::Where { all } => {
            let (cmd, hint) = if *all {
                ("where all", CommandHint::WhereAll)
            } else {
                ("where", CommandHint::Where)
            };
            session.execute(cmd, CommandKind::normal(hint).with_timeout_secs(t))
        }
        Command::Locals => session.locals(t),
        Command::Print { expr } => session.print(expr, t),
        Command::Dump { expr } => session.execute(
            &format!("dump {expr}"),
            CommandKind::normal(CommandHint::Dump).with_timeout_secs(t),
        ),
        Command::Eval { expr } => session.execute(
            &format!("eval {expr}"),
            CommandKind::normal(CommandHint::Eval).with_timeout_secs(t),
        ),
        Command::Threads { filter } => {
            let mut r = session.threads(t);
            // Parser returns everything; filtering happens in handler to keep parser pure as a test oracle.
            if let Ok(ref mut resp) = r {
                if let CommandResult::Threads { threads } = &mut resp.result {
                    *threads = filter_threads(std::mem::take(threads), filter.as_deref());
                }
            }
            r
        }
        Command::Thread { id: tid } => session.execute(
            &format!("thread {tid}"),
            CommandKind::normal(CommandHint::Other).with_timeout_secs(t),
        ),
        Command::Frame { direction, n } => {
            let cmd = format!("{direction} {n}");
            session.execute(
                &cmd,
                CommandKind::normal(CommandHint::Other).with_timeout_secs(t),
            )
        }
        Command::ListSource { line } => session.list_source(*line, t),
        Command::Raw { command } => session.raw(command, t),

        // Thread control / state mutation / locks — all Normal commands, Raw passthrough.
        Command::Suspend { id } => {
            let cmd = match id {
                Some(i) => format!("suspend {i}"),
                None => "suspend".to_string(),
            };
            session.execute(
                &cmd,
                CommandKind::normal(CommandHint::Other).with_timeout_secs(t),
            )
        }
        Command::Resume { id } => {
            let cmd = match id {
                Some(i) => format!("resume {i}"),
                None => "resume".to_string(),
            };
            session.execute(
                &cmd,
                CommandKind::normal(CommandHint::Other).with_timeout_secs(t),
            )
        }
        Command::Set { lvalue, value } => {
            // Strategy: if the value is obviously not a Java expression (contains hyphens, slashes, etc.),
            // wrap it in quotes upfront. Otherwise, try as-is first; if jdb returns "Name unknown" (meaning
            // it tried to resolve the value as a variable and failed), retry with double quotes — this
            // transparently handles LLMs that pass bare string literals like "TestHeader".
            let effective_value = if needs_string_quoting(value) {
                format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
            } else {
                value.clone()
            };
            let first_try = session.execute(
                &format!("set {lvalue} = {effective_value}"),
                CommandKind::normal(CommandHint::Other).with_timeout_secs(t),
            );
            // If the value was NOT pre-quoted and jdb failed with "Name unknown", retry with quotes.
            if effective_value == *value {
                if let Ok(ref resp) = first_try {
                    if let CommandResult::Raw { text } = &resp.result {
                        if text.contains("Name unknown") || text.contains("ParseException") {
                            let quoted =
                                format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""));
                            return match session.execute(
                                &format!("set {lvalue} = {quoted}"),
                                CommandKind::normal(CommandHint::Other).with_timeout_secs(t),
                            ) {
                                Ok(mut resp) => {
                                    append_note_if_present(&mut resp, precondition_note.as_deref());
                                    Response::ok(id, resp)
                                }
                                Err(e) => Response::err(id, e.exit_code(), e.to_string()),
                            };
                        }
                    }
                }
            }
            first_try
        }
        Command::ForceReturn { .. } => Err(Error::UnsupportedBackend {
            backend: "jdb".into(),
            operation: "force_return".into(),
        }),
        Command::Ignore { exception, mode } => {
            // Mirror Catch's mode dispatch for symmetric exception breakpoint removal.
            let cmd = match mode.as_str() {
                "caught" => format!("ignore caught {exception}"),
                "uncaught" => format!("ignore uncaught {exception}"),
                _ => format!("ignore {exception}"),
            };
            session.execute(
                &cmd,
                CommandKind::normal(CommandHint::Other).with_timeout_secs(t),
            )
        }
        Command::Lock { expr } => session.execute(
            &format!("lock {expr}"),
            CommandKind::normal(CommandHint::Other).with_timeout_secs(t),
        ),
        Command::ThreadLocks { id } => {
            let cmd = match id {
                Some(i) => format!("threadlocks {i}"),
                None => "threadlocks".to_string(),
            };
            session.execute(
                &cmd,
                CommandKind::normal(CommandHint::Other).with_timeout_secs(t),
            )
        }

        // Should not get here; lifecycle/daemon/attach commands are handled above.
        _ => return Response::err(id, 400, "unexpected command in session dispatch"),
    };

    match result {
        Ok(mut resp) => {
            append_note_if_present(&mut resp, precondition_note.as_deref());
            Response::ok(id, resp)
        }
        Err(e) => Response::err(id, e.exit_code(), e.to_string()),
    }
}

// ─── Enrichment helpers ─────────────────────────────────────────────────────────

fn dispatch_jdi_session_cmd(req: &Request, session: Arc<JdiSession>) -> Response {
    let id = &req.id;
    let result = match &req.cmd {
        Command::BreakAt {
            class,
            line,
            condition,
            suspend,
        } => {
            if condition.is_some() {
                Err(session.unsupported("conditional break_at"))
            } else {
                session.stop_at(class, *line, suspend.as_deref())
            }
        }
        Command::BreakIn {
            class,
            method,
            event,
            args,
            condition,
            suspend,
        } => {
            if condition.is_some() {
                Err(session.unsupported("conditional break_in"))
            } else {
                session.break_in(class, method, args.as_deref(), *event, suspend.as_deref())
            }
        }
        Command::Run => session.run(req.timeout),
        Command::Cont => session.cont(req.timeout),
        Command::Next => session.next(req.timeout),
        Command::Where { all } => session.stack(*all),
        Command::Locals => session.locals(),
        Command::Threads { filter } => session.threads(filter.as_deref()),
        Command::Thread { id } => session.select_thread(id),
        Command::Inspect { expr, max_elements } => session.inspect(expr, *max_elements),
        Command::Print { expr } | Command::Eval { expr } => session.evaluate(expr),
        Command::Dump { expr } => session.dump(expr, 10),
        Command::Step => session.step(req.timeout),
        Command::StepOut => session.step_out(req.timeout),
        Command::Catch { exception, mode } => session.catch_exception(exception, mode),
        Command::Watch { field, mode } => session.watch(field, mode),
        Command::Unwatch { field, mode } => session.unwatch(field, mode),
        Command::Breakpoints => session.breakpoints(),
        Command::Clear { spec } => session.clear(spec),
        Command::Classes { pattern } => session.classes(pattern.as_deref()),
        Command::Methods { class } => session.methods(class),
        Command::Frame { direction, n } => session.frame(direction, *n),
        Command::ListSource { line } => session.list_source(*line),
        Command::Raw { command } => session.raw(command, req.timeout),
        Command::Suspend { id } => session.suspend(id.as_deref()),
        Command::Resume { id } => session.resume(id.as_deref()),
        Command::Set { lvalue, value } => session.set_value(lvalue, value),
        Command::ForceReturn { value } => session.force_return(value),
        Command::Ignore { exception, mode } => session.ignore_exception(exception, mode),
        Command::Lock { expr } => session.lock(expr),
        Command::ThreadLocks { id } => session.threadlocks(id.as_deref()),
        _ => return Response::err(id, 400, "unexpected command in JDI session dispatch"),
    };

    match result {
        Ok(resp) => Response::ok(id, resp),
        Err(e) => Response::err(id, e.exit_code(), e.to_string()),
    }
}

/// Append one message to resp.note, separating multiple messages with newlines.
fn append_note(resp: &mut CommandResponse, msg: &str) {
    match &mut resp.note {
        Some(existing) => {
            existing.push('\n');
            existing.push_str(msg);
        }
        None => resp.note = Some(msg.to_string()),
    }
}

fn append_note_if_present(resp: &mut CommandResponse, note: Option<&str>) {
    if let Some(note) = note {
        append_note(resp, note);
    }
}

fn watch_command(field: &str, mode: &str) -> String {
    match mode {
        "access" => format!("watch access {field}"),
        "all" => format!("watch all {field}"),
        _ => format!("watch {field}"),
    }
}

fn unwatch_command(field: &str, mode: &str) -> String {
    match mode {
        "access" => format!("unwatch access {field}"),
        "all" => format!("unwatch all {field}"),
        _ => format!("unwatch {field}"),
    }
}

enum AsyncConditionResolution {
    Continue { note: Option<String> },
    Return(CommandResponse),
}

fn settle_async_condition_breakpoint(
    session: &Session,
    cmd: &Command,
    timeout: Option<u64>,
) -> AsyncConditionResolution {
    if !should_settle_async_conditions(cmd) || !session.has_conditions() {
        return AsyncConditionResolution::Continue { note: None };
    }

    let stack = match session.stack(Some(5)) {
        Ok(resp) => resp,
        Err(_) => return AsyncConditionResolution::Continue { note: None },
    };
    let resp = match &stack.result {
        CommandResult::StackTrace { frames } => {
            let Some(top) = frames.first() else {
                return AsyncConditionResolution::Continue { note: None };
            };
            let Some(_) = session.condition_for_hit(
                &top.location.class,
                top.location.line,
                &top.location.method,
            ) else {
                return AsyncConditionResolution::Continue { note: stack.note };
            };

            let location = top.location.clone();
            let thread = String::new();
            CommandResponse {
                result: CommandResult::Stopped {
                    event: Event::Breakpoint {
                        location: location.clone(),
                        thread: thread.clone(),
                    },
                    location,
                    thread,
                    thread_id: None,
                    frame: Some(top.clone()),
                    source_context: None,
                },
                stderr: None,
                note: stack.note,
            }
        }
        CommandResult::Stopped {
            event: Event::Breakpoint { .. },
            location,
            ..
        } => {
            let Some(_) =
                session.condition_for_hit(&location.class, location.line, &location.method)
            else {
                return AsyncConditionResolution::Continue { note: stack.note };
            };
            stack
        }
        _ => return AsyncConditionResolution::Continue { note: None },
    };

    let resolved = eval_condition_loop(session, resp, timeout);
    match resolved.result {
        CommandResult::Stopped { .. } => AsyncConditionResolution::Continue {
            note: resolved.note,
        },
        _ => AsyncConditionResolution::Return(resolved),
    }
}

fn should_settle_async_conditions(cmd: &Command) -> bool {
    matches!(
        cmd,
        Command::Where { .. }
            | Command::Locals
            | Command::Print { .. }
            | Command::Dump { .. }
            | Command::Eval { .. }
            | Command::Classes { .. }
            | Command::Methods { .. }
            | Command::Threads { .. }
            | Command::Thread { .. }
            | Command::Frame { .. }
            | Command::ListSource { .. }
            | Command::Inspect { .. }
            | Command::Set { .. }
            | Command::ForceReturn { .. }
            | Command::Lock { .. }
            | Command::ThreadLocks { .. }
    )
}

/// Conditional-breakpoint loop: if a hit breakpoint has a condition and the condition is false, auto-cont.
/// Cap at 100 iterations to avoid infinite loops.
fn eval_condition_loop(
    session: &Session,
    mut resp: CommandResponse,
    timeout: Option<u64>,
) -> CommandResponse {
    for _ in 0..100 {
        let (hit_class, hit_line, method_name) = match &resp.result {
            CommandResult::Stopped {
                event: Event::Breakpoint { location, .. },
                ..
            } => (
                location.class.clone(),
                location.line,
                location.method.clone(),
            ),
            _ => return resp,
        };

        let condition = match session.condition_for_hit(&hit_class, hit_line, &method_name) {
            Some(c) => c,
            None => return resp,
        };

        // Evaluate condition expression.
        let cond_result = session.print(&condition, Some(5));
        let should_stop = match &cond_result {
            Ok(r) => match &r.result {
                CommandResult::Value { value, .. } => value.trim() == "true",
                _ => {
                    append_note(
                        &mut resp,
                        &format!(
                            "WARNING: conditional breakpoint eval of \"{}\" returned unexpected result — \
                         stopping to let you inspect.",
                            condition
                        ),
                    );
                    true
                }
            },
            Err(e) => {
                append_note(
                    &mut resp,
                    &format!(
                        "WARNING: conditional breakpoint eval of \"{}\" failed ({}) — \
                     stopping to let you inspect.",
                        condition, e
                    ),
                );
                true
            }
        };

        if should_stop {
            return resp;
        }

        // Condition is false; auto-cont.
        match session.cont(timeout) {
            Ok(next_resp) => resp = next_resp,
            Err(e) => {
                append_note(
                    &mut resp,
                    &format!(
                        "WARNING: conditional breakpoint auto-cont failed ({}) — \
                     returning current stop location.",
                        e
                    ),
                );
                return resp;
            }
        }
    }
    append_note(
        &mut resp,
        "WARNING: conditional breakpoint hit 100 iterations without condition becoming true — stopping to prevent infinite loop.",
    );
    resp
}

/// After a blocking command returns Stopped, automatically fetch stack frame + source context.
/// On failure, report a warning in note instead of silently ignoring it.
fn enrich_stopped(session: &Session, resp: &mut CommandResponse) {
    // Extract needed information first to avoid long-lived mutable borrows.
    let (location_line, needs_frame, needs_source) = match &resp.result {
        CommandResult::Stopped {
            location,
            frame,
            source_context,
            ..
        } => (location.line, frame.is_none(), source_context.is_none()),
        _ => return,
    };

    // Collect enrichment data independent of resp's borrow.
    let mut frame_data = None;
    let mut source_data = None;
    let mut warnings: Vec<String> = Vec::new();

    if needs_frame {
        match session.stack(Some(5)) {
            Ok(stack_resp) => {
                if let CommandResult::StackTrace { frames } = &stack_resp.result {
                    if let Some(top) = frames.first() {
                        frame_data = Some(top.clone());
                    }
                }
            }
            Err(e) => {
                warnings.push(format!("WARNING: failed to enrich stack frame: {e}"));
            }
        }
    }

    // Determine the line number for source_context. If location.line==0 (e.g. FieldWatch), backfill from frame.
    let effective_line = if location_line > 0 {
        location_line
    } else {
        frame_data.as_ref().map(|f| f.location.line).unwrap_or(0)
    };

    if needs_source && effective_line > 0 {
        match session.list_source(Some(effective_line), Some(5)) {
            Ok(src_resp) => {
                if let CommandResult::Source { lines, .. } = src_resp.result {
                    source_data = Some(lines);
                }
            }
            Err(e) => {
                warnings.push(format!("WARNING: failed to enrich source context: {e}"));
            }
        }
    }

    // Write back the result.
    if let CommandResult::Stopped {
        location,
        frame,
        source_context,
        ..
    } = &mut resp.result
    {
        if frame.is_none() {
            *frame = frame_data.clone();
        }
        // If location is empty (FieldWatch), backfill from frame.
        if location.line == 0 {
            if let Some(ref f) = frame_data {
                location.class = f.location.class.clone();
                location.method = f.location.method.clone();
                location.file = f.location.file.clone();
                location.line = f.location.line;
            }
        }
        if source_context.is_none() {
            *source_context = source_data;
        }

        if frame.is_none() && needs_frame {
            warnings.push(
                "WARNING: could not retrieve stack frame (enrichment skipped — `where` may return unexpected format).".into()
            );
        }
        if source_context.is_none() && needs_source && effective_line > 0 {
            warnings.push(
                "WARNING: could not retrieve source context (enrichment skipped — is sourcepath set and class compiled with -g?).".into()
            );
        }
    }

    for w in &warnings {
        append_note(resp, w);
    }
}

/// After a hit (Stopped / ExceptionCaught), backfill the hit thread's jdb id for direct `thread <id>` switching.
///
/// The PartialStop path already fills the id in the session layer by reusing its `threads` query. This handles
/// only full-banner paths: if `thread_id` is still None and the event has a thread name or an at-breakpoint
/// thread exists, run `threads` once and reverse-lookup with `thread_id_for`. If lookup fails, write WARNING.
fn enrich_thread_id(session: &Session, resp: &mut CommandResponse) {
    // Extract current thread name and whether id already exists, avoiding long-lived borrows.
    let (have_id, name) = match &resp.result {
        CommandResult::Stopped {
            thread_id, thread, ..
        }
        | CommandResult::ExceptionCaught {
            thread_id, thread, ..
        } => (thread_id.is_some(), thread.clone()),
        _ => return,
    };
    if have_id {
        return; // PartialStop path already filled it.
    }

    let found = match session.threads(None) {
        Ok(r) => match r.result {
            CommandResult::Threads { threads } => thread_id_for(&threads, &name),
            _ => None,
        },
        Err(_) => None,
    };

    match found {
        Some(tid) => {
            if let CommandResult::Stopped { thread_id, .. }
            | CommandResult::ExceptionCaught { thread_id, .. } = &mut resp.result
            {
                *thread_id = Some(tid);
            }
        }
        None => append_note(
            resp,
            "WARNING: could not resolve the hit thread's id (`threads` lookup failed or no match); \
             run `threads` and pass the id to `thread` manually.",
        ),
    }
}

/// On breakpoint hit, compare with the most recent break_at line and add a note if it differs.
fn check_line_mismatch(session: &Session, resp: &mut CommandResponse) {
    let location = match &resp.result {
        CommandResult::Stopped {
            event: Event::Breakpoint { .. },
            location,
            ..
        } => location,
        _ => return,
    };

    if let Some((ref cls, req_line)) = session.take_break_target() {
        if cls == &location.class && req_line != location.line {
            append_note(
                resp,
                &format!(
                    "Breakpoint requested at line {} but hit at line {} — \
                 JVM rounded to nearest executable bytecode.",
                    req_line, location.line
                ),
            );
        }
    }
}

/// `inspect` command: fetch collection/array size plus the first N elements.
fn handle_inspect(
    session: &Session,
    expr: &str,
    max_elements: u32,
    timeout: Option<u64>,
) -> Result<CommandResponse> {
    let max = max_elements.min(50);

    // Try to get size, preferring .size() and falling back to .length.
    let size = try_eval_int(session, &format!("{expr}.size()"), timeout)
        .or_else(|| try_eval_int(session, &format!("{expr}.length"), timeout));

    let count = match size {
        Some(s) => s.min(max),
        None => max,
    };

    // Fetch elements one by one.
    let mut elements = Vec::new();
    for i in 0..count {
        if let Some(val) = try_get_element(session, expr, i, timeout) {
            elements.push(val);
        } else {
            break;
        }
    }

    let truncated = size.map(|s| s > max);
    Ok(CommandResponse {
        result: CommandResult::Inspection {
            expr: expr.to_string(),
            size,
            elements,
            truncated,
        },
        stderr: None,
        note: None,
    })
}

fn try_eval_int(session: &Session, expr: &str, timeout: Option<u64>) -> Option<u32> {
    let resp = session.print(expr, timeout).ok()?;
    if let CommandResult::Value { ref value, .. } = resp.result {
        value.trim().parse().ok()
    } else {
        None
    }
}

fn try_get_element(
    session: &Session,
    expr: &str,
    index: u32,
    timeout: Option<u64>,
) -> Option<VarBinding> {
    for accessor in [format!("{expr}.get({index})"), format!("{expr}[{index}]")] {
        if let Ok(resp) = session.print(&accessor, timeout) {
            if let CommandResult::Value { ref value, .. } = resp.result {
                return Some(VarBinding {
                    name: format!("[{index}]"),
                    ty: None,
                    value: value.clone(),
                });
            }
        }
    }
    None
}

/// Write a response as JSONL: one JSON object plus newline.
fn write_response(mut stream: &Stream, resp: &Response) -> anyhow::Result<()> {
    let json = serde_json::to_string(resp)?;
    stream.write_all(json.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}

// ─── Thread Helper Pure Functions ──────────────────────────────────────────────

/// Find the hit thread id in a `threads` list.
///
/// - Non-empty `name`: prefer an **exact** thread-name match; if unique, return its id.
/// - Empty name (PartialStop truncated-banner fallback), or multiple threads with the same name: fall back to
///   a thread whose state contains `"at breakpoint"`.
/// - If nothing matches, return `None`; the caller writes WARNING and never silently falls back.
fn thread_id_for(threads: &[ThreadInfo], name: &str) -> Option<String> {
    if !name.is_empty() {
        let mut matches = threads.iter().filter(|t| t.name == name);
        if let Some(first) = matches.next() {
            // Unique same-name match: use it directly; multiple same-name matches fall back to at-breakpoint.
            if matches.next().is_none() {
                return Some(first.id.clone());
            }
        }
    }
    threads
        .iter()
        .find(|t| t.state.contains("at breakpoint"))
        .map(|t| t.id.clone())
}

/// Filter by **case-insensitive substring** in thread name; None/empty filter returns all threads unchanged.
fn filter_threads(threads: Vec<ThreadInfo>, filter: Option<&str>) -> Vec<ThreadInfo> {
    match filter {
        Some(f) if !f.is_empty() => {
            let needle = f.to_lowercase();
            threads
                .into_iter()
                .filter(|t| t.name.to_lowercase().contains(&needle))
                .collect()
        }
        _ => threads,
    }
}

/// Determine whether a `set` value needs to be wrapped in double quotes for jdb.
///
/// Returns `true` if the value does not look like a valid Java expression that jdb can evaluate directly:
/// - Already quoted (`"..."`) → no quoting needed
/// - Numeric literal (integer/float, optionally negative) → no quoting needed
/// - Boolean/null (`true`, `false`, `null`) → no quoting needed
/// - Identifier or field/array chain (e.g. `x`, `this.count`, `arr[0]`, `obj.field`) → no quoting needed
/// - `new ...` expression → no quoting needed
/// - Anything else (contains hyphens, spaces, special chars) → likely intended as a string literal
fn needs_string_quoting(value: &str) -> bool {
    let v = value.trim();
    if v.is_empty() {
        return true;
    }
    // Already quoted string
    if v.starts_with('"') && v.ends_with('"') && v.len() >= 2 {
        return false;
    }
    // Already a char literal
    if v.starts_with('\'') && v.ends_with('\'') && v.len() >= 3 {
        return false;
    }
    // Boolean or null
    if matches!(v, "true" | "false" | "null") {
        return false;
    }
    // Numeric literal (integer or float, optionally negative, with optional L/F/D suffix)
    let num_part = v.strip_suffix(|c: char| "lLfFdD".contains(c)).unwrap_or(v);
    if num_part.parse::<f64>().is_ok() {
        return false;
    }
    // Hex literal
    if num_part.starts_with("0x") || num_part.starts_with("0X") {
        if num_part[2..].chars().all(|c| c.is_ascii_hexdigit()) {
            return false;
        }
    }
    // `new` expression (e.g. `new String("x")`)
    if v.starts_with("new ") {
        return false;
    }
    // Cast expression (e.g. `(int)42`)
    if v.starts_with('(') {
        return false;
    }
    // Valid Java identifier/field/method/array chain: starts with letter/$/_, contains only
    // alphanumeric/$/_/./[/]/(/)/,/space (for method calls and generics). This covers:
    // `x`, `this.field`, `arr[0]`, `obj.method()`, `SomeClass.CONST`
    let first = v.chars().next().unwrap();
    if first.is_ascii_alphabetic() || first == '_' || first == '$' {
        let valid_chain = v.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || c == '_'
                || c == '$'
                || c == '.'
                || c == '['
                || c == ']'
                || c == '('
                || c == ')'
                || c == ','
                || c == ' '
                || c == '"'
                || c == '\''
        });
        if valid_chain {
            return false;
        }
    }
    // Anything else: likely a bare string that needs quoting
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn ti(id: &str, name: &str, state: &str) -> ThreadInfo {
        ThreadInfo {
            id: id.into(),
            name: name.into(),
            group: None,
            state: state.into(),
        }
    }

    #[test]
    fn daemon_stop_response_sets_shutdown_flag() {
        let shutdown = AtomicBool::new(false);

        let resp = daemon_stop_response("r1", &shutdown);

        assert!(resp.ok);
        assert!(shutdown.load(Ordering::SeqCst));
    }

    #[test]
    fn watch_command_uses_jdb_mode_syntax() {
        assert_eq!(watch_command("Main.x", "modification"), "watch Main.x");
        assert_eq!(watch_command("Main.x", "access"), "watch access Main.x");
        assert_eq!(watch_command("Main.x", "all"), "watch all Main.x");
    }

    #[test]
    fn unwatch_command_uses_jdb_mode_syntax() {
        assert_eq!(unwatch_command("Main.x", "modification"), "unwatch Main.x");
        assert_eq!(unwatch_command("Main.x", "access"), "unwatch access Main.x");
        assert_eq!(unwatch_command("Main.x", "all"), "unwatch all Main.x");
    }

    #[test]
    fn async_condition_settle_runs_before_inspection_commands() {
        assert!(should_settle_async_conditions(&Command::Threads {
            filter: None
        }));
        assert!(should_settle_async_conditions(&Command::Print {
            expr: "name".into()
        }));
        assert!(should_settle_async_conditions(&Command::Set {
            lvalue: "name".into(),
            value: "userToken".into()
        }));
    }

    #[test]
    fn async_condition_settle_skips_breakpoint_management() {
        assert!(!should_settle_async_conditions(&Command::BreakAt {
            class: "Main".into(),
            line: 42,
            condition: Some("x > 1".into()),
            suspend: None,
        }));
        assert!(!should_settle_async_conditions(&Command::Clear {
            spec: "Main:42".into()
        }));
        assert!(!should_settle_async_conditions(&Command::Cont));
    }

    #[test]
    fn thread_id_for_exact_name_match() {
        let threads = vec![
            ti("0x1", "main", "running"),
            ti("18315", "http-nio-9702-exec-1", "running (at breakpoint)"),
            ti("18316", "http-nio-9702-exec-2", "cond. waiting"),
        ];
        assert_eq!(
            thread_id_for(&threads, "http-nio-9702-exec-2").as_deref(),
            Some("18316")
        );
    }

    #[test]
    fn thread_id_for_empty_name_falls_back_to_at_breakpoint() {
        let threads = vec![
            ti("0x1", "main", "running"),
            ti("18315", "http-nio-9702-exec-1", "running (at breakpoint)"),
        ];
        assert_eq!(thread_id_for(&threads, "").as_deref(), Some("18315"));
    }

    #[test]
    fn thread_id_for_duplicate_names_uses_at_breakpoint() {
        // Two threads with the same name are common in pools; exact match is ambiguous, so fall back to at-breakpoint.
        let threads = vec![
            ti("0xaa", "worker", "cond. waiting"),
            ti("0xbb", "worker", "running (at breakpoint)"),
        ];
        assert_eq!(thread_id_for(&threads, "worker").as_deref(), Some("0xbb"));
    }

    #[test]
    fn thread_id_for_no_match_returns_none() {
        let threads = vec![ti("0x1", "main", "running")];
        assert_eq!(thread_id_for(&threads, "nonexistent"), None);
    }

    #[test]
    fn filter_threads_case_insensitive_substring() {
        let threads = vec![
            ti("0x1", "main", "running"),
            ti("18315", "http-nio-9702-exec-1", "running (at breakpoint)"),
            ti("18316", "HTTP-nio-9702-exec-2", "cond. waiting"),
            ti("0x2", "redisson-netty-2-1", "running"),
        ];
        let out = filter_threads(threads, Some("http-nio"));
        assert_eq!(out.len(), 2);
        assert!(
            out.iter()
                .all(|t| t.name.to_lowercase().contains("http-nio"))
        );
    }

    #[test]
    fn filter_threads_none_returns_all() {
        let threads = vec![ti("0x1", "main", "running"), ti("0x2", "worker", "running")];
        assert_eq!(filter_threads(threads.clone(), None).len(), 2);
        assert_eq!(filter_threads(threads, Some("")).len(), 2);
    }

    // ─── needs_string_quoting tests ────────────────────────────────────────────

    #[test]
    fn quoting_already_quoted_string() {
        assert!(!needs_string_quoting("\"hello\""));
    }

    #[test]
    fn quoting_number_literals() {
        assert!(!needs_string_quoting("42"));
        assert!(!needs_string_quoting("-3.14"));
        assert!(!needs_string_quoting("0xFF"));
        assert!(!needs_string_quoting("100L"));
    }

    #[test]
    fn quoting_boolean_and_null() {
        assert!(!needs_string_quoting("true"));
        assert!(!needs_string_quoting("false"));
        assert!(!needs_string_quoting("null"));
    }

    #[test]
    fn quoting_identifier_chains() {
        assert!(!needs_string_quoting("x"));
        assert!(!needs_string_quoting("this.count"));
        assert!(!needs_string_quoting("arr[0]"));
        assert!(!needs_string_quoting("SomeClass.CONST"));
        assert!(!needs_string_quoting("obj.method()"));
    }

    #[test]
    fn quoting_new_and_cast_expressions() {
        assert!(!needs_string_quoting("new String(\"x\")"));
        assert!(!needs_string_quoting("(int)42"));
    }

    #[test]
    fn quoting_bare_strings_that_need_quotes() {
        // Contains hyphen — not a valid Java identifier
        assert!(needs_string_quoting("X-Test-Header"));
        assert!(needs_string_quoting("content-type"));
        // Contains special characters
        assert!(needs_string_quoting("hello world!"));
        assert!(needs_string_quoting("/api/v1/users"));
    }

    #[test]
    fn quoting_empty_value() {
        assert!(needs_string_quoting(""));
    }
}
