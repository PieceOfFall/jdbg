//! 单连接处理器：解码 JSONL Request，路由命令，编码 Response。

use std::io::{BufRead, BufReader, Write};
use std::sync::Arc;

use interprocess::local_socket::Stream;

use crate::jdb::parser::CommandHint;
use crate::protocol::*;
use crate::session::CommandKind;
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
        Command::List => {
            let result = mgr.list();
            Response::ok(id, CommandResponse { result, stderr: None, note: None })
        }
        Command::Kill => {
            let session_id = req.session.as_deref();
            match session_id {
                Some(sid) => match mgr.kill(sid) {
                    Ok(()) => Response::ok(id, CommandResponse {
                        result: CommandResult::Raw { text: format!("session {sid} killed") },
                        stderr: None, note: None,
                    }),
                    Err(e) => Response::err(id, e.exit_code(), e.to_string()),
                },
                None => Response::err(id, 400, "specify --session for kill"),
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

    let result = match &req.cmd {
        // Breakpoints
        Command::BreakAt { class, line } => session.stop_at(class, *line),
        Command::BreakIn { class, method, args } => {
            session.stop_in(class, method, args.as_deref())
        }
        Command::Catch { exception, mode } => {
            let cmd = match mode.as_str() {
                "caught" => format!("catch caught {exception}"),
                "uncaught" => format!("catch uncaught {exception}"),
                _ => format!("catch {exception}"),
            };
            session.execute(&cmd, CommandKind::normal(CommandHint::BreakpointSet))
        }
        Command::Breakpoints => {
            session.execute("clear", CommandKind::normal(CommandHint::Breakpoints))
        }
        Command::Clear { spec } => {
            session.execute(&format!("clear {spec}"), CommandKind::normal(CommandHint::Other))
        }

        // Execution control
        Command::Run => session.run(),
        Command::Cont => session.cont(),
        Command::Step => session.step(),
        Command::Next => session.next(),
        Command::StepOut => session.step_out(),

        // Inspection
        Command::Where { all } => {
            let cmd = if *all { "where all" } else { "where" };
            session.execute(cmd, CommandKind::normal(CommandHint::Where))
        }
        Command::Locals => session.locals(),
        Command::Print { expr } => session.print(expr),
        Command::Dump { expr } => {
            session.execute(&format!("dump {expr}"), CommandKind::normal(CommandHint::Dump))
        }
        Command::Eval { expr } => {
            session.execute(&format!("eval {expr}"), CommandKind::normal(CommandHint::Eval))
        }
        Command::Threads => session.threads(),
        Command::Thread { id: tid } => {
            session.execute(&format!("thread {tid}"), CommandKind::normal(CommandHint::Other))
        }
        Command::Frame { direction, n } => {
            let cmd = format!("{direction} {n}");
            session.execute(&cmd, CommandKind::normal(CommandHint::Other))
        }
        Command::ListSource { line } => session.list_source(*line),
        Command::Raw { command } => session.raw(command),

        // Attach（TODO：后续实现 Session::attach）
        Command::Attach { .. } => {
            return Response::err(id, 501, "attach mode not yet implemented");
        }

        // 不应走到这里（lifecycle/daemon commands 已在上层处理）
        _ => return Response::err(id, 400, "unexpected command in session dispatch"),
    };

    match result {
        Ok(resp) => Response::ok(id, resp),
        Err(e) => Response::err(id, e.exit_code(), e.to_string()),
    }
}

/// 写响应（JSONL：一行 JSON + newline）。
fn write_response(mut stream: &Stream, resp: &Response) -> anyhow::Result<()> {
    let json = serde_json::to_string(resp)?;
    stream.write_all(json.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}
