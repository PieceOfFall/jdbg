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
                    // 若入口处把 localhost 规范化成了 127.0.0.1，明确告知（不静默）：
                    // 双栈机器 localhost→::1 而 JDWP 多在 IPv4 监听，规范化避免 connection refused。
                    let note = crate::jdb::process::normalize_attach_host(host)
                        .ne(host)
                        .then(|| format!(
                            "host '{host}' normalized to 127.0.0.1 (IPv4 loopback): on dual-stack \
                             hosts 'localhost' may resolve to IPv6 [::1] but JDWP usually listens \
                             only on IPv4. target shows the address actually connected."
                        ));
                    Response::ok(id, CommandResponse { result, stderr: None, note })
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
            // suspend policy 在设断点时就编码进 jdb 命令（`stop thread at`），命中后无需补救。
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
        Command::BreakIn { class, method, args, condition, suspend } => {
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
            session.execute(&cmd, CommandKind::normal(CommandHint::BreakpointSet).with_timeout_secs(t))
        }
        Command::Watch { field, mode } => {
            let cmd = match mode.as_str() {
                "access" => format!("watch access {field}"),
                "all" => format!("watch all {field}"),
                _ => format!("watch {field}"),
            };
            session.execute(&cmd, CommandKind::normal(CommandHint::WatchSet).with_timeout_secs(t))
        }
        Command::Unwatch { field } => {
            session.execute(&format!("unwatch {field}"), CommandKind::normal(CommandHint::Other).with_timeout_secs(t))
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
                Ok(resp) => return Response::ok(id, resp),
                Err(e) => return Response::err(id, e.exit_code(), e.to_string()),
            }
        }

        // Class/method search
        Command::Classes { pattern } => {
            let cmd = match pattern {
                Some(p) => format!("classes {p}"),
                None => "classes".to_string(),
            };
            let mut r = session.execute(&cmd, CommandKind::normal(CommandHint::Classes).with_timeout_secs(t));
            // handler 注入 class 名到 Methods 结果（parser 无上下文）。
            if let Ok(ref mut resp) = r {
                if let CommandResult::Methods { ref mut class, .. } = resp.result {
                    if let Some(p) = pattern { *class = p.clone(); }
                }
            }
            r
        }
        Command::Methods { class } => {
            let mut r = session.execute(&format!("methods {class}"), CommandKind::normal(CommandHint::Methods).with_timeout_secs(t));
            if let Ok(ref mut resp) = r {
                if let CommandResult::Methods { class: ref mut c, .. } = resp.result {
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
        Command::Dump { expr } => {
            session.execute(&format!("dump {expr}"), CommandKind::normal(CommandHint::Dump).with_timeout_secs(t))
        }
        Command::Eval { expr } => {
            session.execute(&format!("eval {expr}"), CommandKind::normal(CommandHint::Eval).with_timeout_secs(t))
        }
        Command::Threads { filter } => {
            let mut r = session.threads(t);
            // parser 返回全量；过滤在 handler 层做（保持 parser 纯粹，作为测试 oracle）。
            if let Ok(ref mut resp) = r {
                if let CommandResult::Threads { threads } = &mut resp.result {
                    *threads = filter_threads(std::mem::take(threads), filter.as_deref());
                }
            }
            r
        }
        Command::Thread { id: tid } => {
            session.execute(&format!("thread {tid}"), CommandKind::normal(CommandHint::Other).with_timeout_secs(t))
        }
        Command::Frame { direction, n } => {
            let cmd = format!("{direction} {n}");
            session.execute(&cmd, CommandKind::normal(CommandHint::Other).with_timeout_secs(t))
        }
        Command::ListSource { line } => session.list_source(*line, t),
        Command::Raw { command } => session.raw(command, t),

        // Thread control / state mutation / locks — all Normal commands, Raw passthrough.
        Command::Suspend { id } => {
            let cmd = match id {
                Some(i) => format!("suspend {i}"),
                None => "suspend".to_string(),
            };
            session.execute(&cmd, CommandKind::normal(CommandHint::Other).with_timeout_secs(t))
        }
        Command::Resume { id } => {
            let cmd = match id {
                Some(i) => format!("resume {i}"),
                None => "resume".to_string(),
            };
            session.execute(&cmd, CommandKind::normal(CommandHint::Other).with_timeout_secs(t))
        }
        Command::Set { lvalue, value } => {
            session.execute(&format!("set {lvalue} = {value}"), CommandKind::normal(CommandHint::Other).with_timeout_secs(t))
        }
        Command::Ignore { exception, mode } => {
            // 镜像 Catch 的 mode dispatch（对称的异常断点移除）。
            let cmd = match mode.as_str() {
                "caught" => format!("ignore caught {exception}"),
                "uncaught" => format!("ignore uncaught {exception}"),
                _ => format!("ignore {exception}"),
            };
            session.execute(&cmd, CommandKind::normal(CommandHint::Other).with_timeout_secs(t))
        }
        Command::Lock { expr } => {
            session.execute(&format!("lock {expr}"), CommandKind::normal(CommandHint::Other).with_timeout_secs(t))
        }
        Command::ThreadLocks { id } => {
            let cmd = match id {
                Some(i) => format!("threadlocks {i}"),
                None => "threadlocks".to_string(),
            };
            session.execute(&cmd, CommandKind::normal(CommandHint::Other).with_timeout_secs(t))
        }

        // 不应走到这里（lifecycle/daemon/attach commands 已在上层处理）
        _ => return Response::err(id, 400, "unexpected command in session dispatch"),
    };

    match result {
        Ok(resp) => Response::ok(id, resp),
        Err(e) => Response::err(id, e.exit_code(), e.to_string()),
    }
}

// ─── Enrichment helpers ─────────────────────────────────────────────────────────

/// 追加一条消息到 resp.note（多条用换行分隔）。
fn append_note(resp: &mut CommandResponse, msg: &str) {
    match &mut resp.note {
        Some(existing) => { existing.push('\n'); existing.push_str(msg); }
        None => resp.note = Some(msg.to_string()),
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
        let should_stop = match &cond_result {
            Ok(r) => match &r.result {
                CommandResult::Value { value, .. } => value.trim() == "true",
                _ => {
                    append_note(&mut resp, &format!(
                        "WARNING: conditional breakpoint eval of \"{}\" returned unexpected result — \
                         stopping to let you inspect.",
                        condition
                    ));
                    true
                }
            },
            Err(e) => {
                append_note(&mut resp, &format!(
                    "WARNING: conditional breakpoint eval of \"{}\" failed ({}) — \
                     stopping to let you inspect.",
                    condition, e
                ));
                true
            }
        };

        if should_stop {
            return resp;
        }

        // 条件不满足，自动 cont
        match session.cont(timeout) {
            Ok(next_resp) => resp = next_resp,
            Err(e) => {
                append_note(&mut resp, &format!(
                    "WARNING: conditional breakpoint auto-cont failed ({}) — \
                     returning current stop location.",
                    e
                ));
                return resp;
            }
        }
    }
    append_note(&mut resp, "WARNING: conditional breakpoint hit 100 iterations without condition becoming true — stopping to prevent infinite loop.");
    resp
}

/// 阻塞命令返回 Stopped 后，自动获取栈帧 + 源码上下文。
/// 失败时在 note 中报告 warning（不静默忽略）。
fn enrich_stopped(session: &Session, resp: &mut CommandResponse) {
    // 先提取需要的信息（避免长生命周期可变借用）。
    let (location_line, needs_frame, needs_source) = match &resp.result {
        CommandResult::Stopped { location, frame, source_context, .. } => {
            (location.line, frame.is_none(), source_context.is_none())
        }
        _ => return,
    };

    // 收集 enrichment 数据（独立于 resp 的借用）。
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
                warnings.push(format!(
                    "WARNING: failed to enrich stack frame: {e}"
                ));
            }
        }
    }

    // 确定用于 source_context 的行号——如果 location.line==0（如 FieldWatch），从 frame 回填。
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
                warnings.push(format!(
                    "WARNING: failed to enrich source context: {e}"
                ));
            }
        }
    }

    // 写回结果。
    if let CommandResult::Stopped { location, frame, source_context, .. } = &mut resp.result {
        if frame.is_none() {
            *frame = frame_data.clone();
        }
        // 如果 location 是空的（FieldWatch），从 frame 回填。
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

/// 命中（Stopped / ExceptionCaught）后回填命中线程的 jdb id，供 `thread <id>` 直接切换。
///
/// PartialStop 路径已在 session 层填好 id（复用那次 `threads` 查询），此处只处理完整 banner
/// 路径：若 `thread_id` 仍为 None 且事件带线程名/有 at-breakpoint 线程，则跑一次 `threads`
/// 用 `thread_id_for` 反查。查不到写 WARNING（绝不静默）。
fn enrich_thread_id(session: &Session, resp: &mut CommandResponse) {
    // 取出当前线程名 + 是否已有 id（避免长生命周期借用）。
    let (have_id, name) = match &resp.result {
        CommandResult::Stopped { thread_id, thread, .. }
        | CommandResult::ExceptionCaught { thread_id, thread, .. } => {
            (thread_id.is_some(), thread.clone())
        }
        _ => return,
    };
    if have_id {
        return; // PartialStop 路径已填。
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

/// 断点命中时，与最近设置的 break_at 行号比对；不匹配则添加 note。
fn check_line_mismatch(session: &Session, resp: &mut CommandResponse) {
    let location = match &resp.result {
        CommandResult::Stopped { event: Event::Breakpoint { .. }, location, .. } => location,
        _ => return,
    };

    if let Some((ref cls, req_line)) = session.take_break_target() {
        if cls == &location.class && req_line != location.line {
            append_note(resp, &format!(
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

// ─── Thread 辅助纯函数 ────────────────────────────────────────────────────────────

/// 在 `threads` 列表里找出命中线程的 id。
///
/// - `name` 非空 → 优先按线程名**精确**匹配；唯一命中即返回其 id。
/// - 名字为空（PartialStop 截断 banner 兜底）、或同名多个 → 退而取 state 含
///   `"at breakpoint"` 的线程。
/// - 都找不到 → `None`（调用方写 WARNING，绝不静默）。
fn thread_id_for(threads: &[ThreadInfo], name: &str) -> Option<String> {
    if !name.is_empty() {
        let mut matches = threads.iter().filter(|t| t.name == name);
        if let Some(first) = matches.next() {
            // 唯一同名 → 直接用；多个同名 → 落到 at-breakpoint 兜底。
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

/// 按线程名**大小写不敏感子串**过滤；`filter` 为 None/空串时原样返回全部。
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

#[cfg(test)]
mod tests {
    use super::*;

    fn ti(id: &str, name: &str, state: &str) -> ThreadInfo {
        ThreadInfo { id: id.into(), name: name.into(), group: None, state: state.into() }
    }

    #[test]
    fn thread_id_for_exact_name_match() {
        let threads = vec![
            ti("0x1", "main", "running"),
            ti("18315", "http-nio-9702-exec-1", "running (at breakpoint)"),
            ti("18316", "http-nio-9702-exec-2", "cond. waiting"),
        ];
        assert_eq!(thread_id_for(&threads, "http-nio-9702-exec-2").as_deref(), Some("18316"));
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
        // 两个同名线程（线程池常见）→ 精确匹配有歧义，落到 at-breakpoint 那个。
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
        assert!(out.iter().all(|t| t.name.to_lowercase().contains("http-nio")));
    }

    #[test]
    fn filter_threads_none_returns_all() {
        let threads = vec![ti("0x1", "main", "running"), ti("0x2", "worker", "running")];
        assert_eq!(filter_threads(threads.clone(), None).len(), 2);
        assert_eq!(filter_threads(threads, Some("")).len(), 2);
    }
}
