//! 单连接处理器：解码 JSONL Request，路由命令，编码 Response。

use std::io::{BufRead, BufReader, Write};
use std::sync::Arc;

use interprocess::local_socket::Stream;

use crate::error::Result;
use crate::jdb::parser::CommandHint;
use crate::protocol::*;
use crate::session::{CommandKind, Session};
use super::manager::SessionManager;

/// 处理一条连接（一个 request → 一个 response）。
pub fn handle_connection(stream: Stream, mgr: &Arc<SessionManager>) -> anyhow::Result<()> {
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

    let resp = dispatch(&req, mgr);
    write_response(&stream, &resp)?;
    Ok(())
}

/// 路由命令到具体处理逻辑。
fn dispatch(req: &Request, mgr: &Arc<SessionManager>) -> Response {
    let id = &req.id;
    match &req.cmd {
        // ── Session lifecycle ──
        Command::Launch {
            main_class, classpath, sourcepath, app_args, jdb_args, name, jdb_path,
        } => {
            match mgr.create_launch(super::manager::LaunchParams {
                main_class: main_class.clone(),
                classpath: classpath.clone(),
                sourcepath: sourcepath.clone(),
                app_args: app_args.clone(),
                jdb_args: jdb_args.clone(),
                name: name.clone(),
                jdb_path: jdb_path.clone(),
            }) {
                Ok(session) => {
                    let result = CommandResult::SessionCreated {
                        session: session.meta.id.clone(),
                        mode: session.meta.mode,
                        target: session.meta.target.clone(),
                        state: session.state(),
                    };
                    Response::ok(id, CommandResponse { result, stderr: None, note: None })
                }
                Err(e) => Response::err(id, e.exit_code(), e.to_string()),
            }
        }
        Command::Attach { host, port, sourcepath, name, jdb_path } => {
            match mgr.create_attach(super::manager::AttachParams {
                host: host.clone(),
                port: *port,
                sourcepath: sourcepath.clone(),
                name: name.clone(),
                jdb_path: jdb_path.clone(),
            }) {
                Ok(session) => {
                    let result = CommandResult::SessionCreated {
                        session: session.meta.id.clone(),
                        mode: session.meta.mode,
                        target: session.meta.target.clone(),
                        state: session.state(),
                    };
                    Response::ok(id, CommandResponse { result, stderr: None, note: None })
                }
                Err(e) => Response::err(id, e.exit_code(), e.to_string()),
            }
        }
        Command::List => {
            let result = mgr.list();
            Response::ok(id, CommandResponse { result, stderr: None, note: None })
        }
        Command::Kill => {
            // 解析目标会话（None = 唯一存活会话），与其它命令的 --session 默认行为一致。
            match mgr.get(req.session.as_deref()) {
                Ok(session) => {
                    let sid = session.meta.id.clone();
                    match mgr.kill(&sid) {
                        Ok(()) => Response::ok(id, CommandResponse {
                            result: CommandResult::Raw { text: format!("session {sid} killed") },
                            stderr: None, note: None,
                        }),
                        Err(e) => Response::err(id, e.exit_code(), e.to_string()),
                    }
                }
                Err(e) => Response::err(id, e.exit_code(), e.to_string()),
            }
        }
        Command::Status => {
            match mgr.get(req.session.as_deref()) {
                Ok(session) => {
                    let result = session.status();
                    Response::ok(id, CommandResponse { result, stderr: None, note: None })
                }
                Err(e) => Response::err(id, e.exit_code(), e.to_string()),
            }
        }

        // ── Daemon control ──
        Command::DaemonStatus => {
            let result = CommandResult::Raw {
                text: format!("daemon pid={} running", std::process::id()),
            };
            Response::ok(id, CommandResponse { result, stderr: None, note: None })
        }
        Command::DaemonStop => {
            // 先响应 ok，然后 daemon 会在这个连接关闭后终止进程。
            let result = CommandResult::Raw { text: "daemon stopping".into() };
            let resp = Response::ok(id, CommandResponse { result, stderr: None, note: None });
            // 排出响应后退出进程（粗暴但有效；后续可改优雅 shutdown flag）。
            std::thread::spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(100));
                std::process::exit(0);
            });
            resp
        }

        // ── All session-bound commands ──
        _ => dispatch_session_cmd(req, mgr),
    }
}

/// 对需要具体会话的命令做路由。
fn dispatch_session_cmd(req: &Request, mgr: &Arc<SessionManager>) -> Response {
    let id = &req.id;
    let session = match mgr.get(req.session.as_deref()) {
        Ok(s) => s,
        Err(e) => return Response::err(id, e.exit_code(), e.to_string()),
    };

    // 本请求的超时覆盖（CLI `--timeout`），None = 用各命令默认值。
    let t = req.timeout;

    let result = match &req.cmd {
        // Breakpoints
        Command::BreakAt { class, line, condition, suspend } => {
            let r = session.stop_at(class, *line, condition.as_deref(), t);
            if r.is_ok() {
                session.record_break_target(class, *line);
                let spec = format!("{class}:{line}");
                if let Some(cond) = condition {
                    session.add_condition(&spec, cond);
                }
                if let Some(sp) = suspend {
                    session.set_suspend_policy(&spec, sp);
                }
            }
            r
        }
        Command::BreakIn { class, method, args, condition, suspend } => {
            let r = session.stop_in(class, method, args.as_deref(), condition.as_deref(), t);
            if r.is_ok() {
                let spec = match args {
                    Some(a) => format!("{class}.{method}({a})"),
                    None => format!("{class}.{method}"),
                };
                if let Some(cond) = condition {
                    session.add_condition(&spec, cond);
                }
                if let Some(sp) = suspend {
                    session.set_suspend_policy(&spec, sp);
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
            session.execute(&cmd, CommandKind::normal(CommandHint::BreakpointSet).with_timeout_secs(t))
        }
        Command::Breakpoints => {
            session.execute("clear", CommandKind::normal(CommandHint::Breakpoints).with_timeout_secs(t))
        }
        Command::Clear { spec } => {
            session.execute(&format!("clear {spec}"), CommandKind::normal(CommandHint::Other).with_timeout_secs(t))
        }

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
                    apply_suspend_policy(&session, &mut resp, t);
                    enrich_stopped(&session, &mut resp);
                    check_line_mismatch(&session, &mut resp);
                    return Response::ok(id, resp);
                }
                Err(e) => return Response::err(id, e.exit_code(), e.to_string()),
            }
        }

        // Inspection — inspect (composite, multi-command)
        Command::Inspect { expr, max_elements } => {
            match handle_inspect(&session, expr, *max_elements, t) {
                Ok(resp) => return Response::ok(id, resp),
                Err(e) => return Response::err(id, e.exit_code(), e.to_string()),
            }
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
        Command::Dump { expr } => {
            session.execute(&format!("dump {expr}"), CommandKind::normal(CommandHint::Dump).with_timeout_secs(t))
        }
        Command::Eval { expr } => {
            session.execute(&format!("eval {expr}"), CommandKind::normal(CommandHint::Eval).with_timeout_secs(t))
        }
        Command::Threads => session.threads(t),
        Command::Thread { id: tid } => {
            session.execute(&format!("thread {tid}"), CommandKind::normal(CommandHint::Other).with_timeout_secs(t))
        }
        Command::Frame { direction, n } => {
            let cmd = format!("{direction} {n}");
            session.execute(&cmd, CommandKind::normal(CommandHint::Other).with_timeout_secs(t))
        }
        Command::ListSource { line } => session.list_source(*line, t),
        Command::Raw { command } => session.raw(command, t),

        // 不应走到这里（lifecycle/daemon/attach commands 已在上层处理）
        _ => return Response::err(id, 400, "unexpected command in session dispatch"),
    };

    match result {
        Ok(resp) => Response::ok(id, resp),
        Err(e) => Response::err(id, e.exit_code(), e.to_string()),
    }
}

// ─── Enrichment helpers ─────────────────────────────────────────────────────────

/// 线程级 suspend：仅保持命中线程挂起，恢复其他线程（ZK 心跳等）。
/// 利用 suspend count 技巧：先 suspend <id>（count +1），再 resume all（count -1）。
/// 失败时静默退回 SUSPEND_ALL（安全）。
fn apply_suspend_policy(session: &Session, resp: &mut CommandResponse, timeout: Option<u64>) {
    let (spec, thread_name) = match &resp.result {
        CommandResult::Stopped { event: Event::Breakpoint { location, .. }, thread, .. } => {
            (format!("{}:{}", location.class, location.line), thread.clone())
        }
        _ => return,
    };

    let policy = session.get_suspend_policy(&spec).unwrap_or_else(|| "all".into());
    if policy != "thread" {
        return;
    }

    let hex_id = match session.resolve_thread_id(&thread_name, timeout) {
        Some(id) => id,
        None => return,
    };

    // suspend count +1（命中线程 → count=2）
    if session.raw(&format!("suspend {hex_id}"), timeout).is_err() {
        return;
    }

    // resume all（全部 count -1：命中线程 2→1 仍挂，其他 1→0 恢复）
    if session.raw("resume", timeout).is_err() {
        let _ = session.raw(&format!("resume {hex_id}"), timeout);
        return;
    }

    let note_msg = format!(
        "Suspend policy: thread — only \"{}\" ({}) is suspended; other threads continue.",
        thread_name, hex_id
    );
    match &mut resp.note {
        Some(existing) => { existing.push('\n'); existing.push_str(&note_msg); }
        None => resp.note = Some(note_msg),
    }
}

/// 条件断点循环：如果命中的断点有条件且条件为 false，自动 cont 继续。
/// 最多循环 100 次防止无限 loop。
fn eval_condition_loop(session: &Session, mut resp: CommandResponse, timeout: Option<u64>) -> CommandResponse {
    for _ in 0..100 {
        let spec = match &resp.result {
            CommandResult::Stopped { event: Event::Breakpoint { location, .. }, .. } => {
                format!("{}:{}", location.class, location.line)
            }
            _ => return resp,
        };

        let condition = match session.get_condition(&spec) {
            Some(c) => c,
            None => return resp,
        };

        // eval 条件表达式
        let cond_result = session.print(&condition, Some(5));
        let should_stop = match cond_result {
            Ok(ref r) => match &r.result {
                CommandResult::Value { value, .. } => value.trim() == "true",
                _ => true, // eval 失败 → 停下让用户看
            },
            Err(_) => true,
        };

        if should_stop {
            return resp;
        }

        // 条件不满足，自动 cont
        match session.cont(timeout) {
            Ok(next_resp) => resp = next_resp,
            Err(_) => return resp,
        }
    }
    resp
}

/// 阻塞命令返回 Stopped 后，自动获取栈帧 + 源码上下文（best-effort）。
fn enrich_stopped(session: &Session, resp: &mut CommandResponse) {
    let (location_line, frame_ref, source_ref) = match &mut resp.result {
        CommandResult::Stopped { location, frame, source_context, .. } => {
            (location.line, frame, source_context)
        }
        _ => return,
    };

    // 填充 frame（top of stack）
    if frame_ref.is_none() {
        if let Ok(stack_resp) = session.stack(Some(5)) {
            if let CommandResult::StackTrace { frames } = &stack_resp.result {
                if let Some(top) = frames.first() {
                    *frame_ref = Some(top.clone());
                }
            }
        }
    }

    // 填充 source_context
    if source_ref.is_none() && location_line > 0 {
        if let Ok(src_resp) = session.list_source(Some(location_line), Some(5)) {
            if let CommandResult::Source { lines, .. } = src_resp.result {
                *source_ref = Some(lines);
            }
        }
    }
}

/// 断点命中时，与最近设置的 break_at 行号比对；不匹配则添加 note。
fn check_line_mismatch(session: &Session, resp: &mut CommandResponse) {
    let location = match &resp.result {
        CommandResult::Stopped { event: Event::Breakpoint { .. }, location, .. } => location,
        _ => return,
    };

    if let Some((ref cls, req_line)) = session.take_break_target() {
        if cls == &location.class && req_line != location.line {
            resp.note = Some(format!(
                "Breakpoint requested at line {} but hit at line {} — \
                 JVM rounded to nearest executable bytecode.",
                req_line, location.line
            ));
        }
    }
}

/// `inspect` 命令：获取集合/数组的 size + 前 N 个元素。
fn handle_inspect(
    session: &Session,
    expr: &str,
    max_elements: u32,
    timeout: Option<u64>,
) -> Result<CommandResponse> {
    let max = max_elements.min(50);

    // 尝试获取 size（.size() 优先，fallback .length）
    let size = try_eval_int(session, &format!("{expr}.size()"), timeout)
        .or_else(|| try_eval_int(session, &format!("{expr}.length"), timeout));

    let count = match size {
        Some(s) => s.min(max),
        None => max,
    };

    // 逐个取元素
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

/// 写响应（JSONL：一行 JSON + newline）。
fn write_response(mut stream: &Stream, resp: &Response) -> anyhow::Result<()> {
    let json = serde_json::to_string(resp)?;
    stream.write_all(json.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}
