//! `jdbg` 入口。
//!
//! 两种运行模式：
//! - `jdbg __daemon`：作为后台 daemon 运行（隐藏子命令，CLI 自动拉起）。
//! - `jdbg <subcommand> …`：CLI client——构造 Request 发给 daemon，打印 Response。
//!
//! 本阶段（roadmap 4）用最简手动 arg 解析验证 IPC 链路；
//! 完整 clap 命令面 + 文本/JSON 渲染留到 roadmap 6（cli.rs + output.rs）。

use std::process::ExitCode;

use java_agent_debugger::daemon;
use java_agent_debugger::protocol::{Command, Request, Response};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // 隐藏的 daemon 模式。
    if args.first().map(String::as_str) == Some("__daemon") {
        if let Err(e) = daemon::run_daemon() {
            eprintln!("daemon error: {e}");
            return ExitCode::from(1);
        }
        return ExitCode::SUCCESS;
    }

    run_client(&args).unwrap_or_else(|e| {
        eprintln!("error: {e}");
        ExitCode::from(1)
    })
}

/// CLI client 流程：解析 args → Command → 发送 → 打印。
fn run_client(args: &[String]) -> anyhow::Result<ExitCode> {
    let Some(sub) = args.first() else {
        print_usage();
        return Ok(ExitCode::from(2));
    };

    // 解析全局 --session / --json（极简版）。
    let mut session: Option<String> = None;
    let mut json = false;
    let mut positional: Vec<String> = Vec::new();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--session" => {
                i += 1;
                session = args.get(i).cloned();
            }
            "--json" => json = true,
            "--classpath" | "--sourcepath" => {
                // 收集为 positional 形式 key=value，供 Launch 解析。
                let key = args[i].clone();
                i += 1;
                if let Some(v) = args.get(i) {
                    positional.push(format!("{key}={v}"));
                }
            }
            other => positional.push(other.to_string()),
        }
        i += 1;
    }

    let cmd = build_command(sub, &positional)?;

    // daemon 控制命令特殊处理（stop 不需要 session）。
    let req = Request::new(cmd, session);
    let resp = java_agent_debugger::client::send_request(&req)?;
    print_response(&resp, json);

    Ok(if resp.ok { ExitCode::SUCCESS } else { ExitCode::from(1) })
}

/// 把子命令 + 参数构造成 Command（极简映射，覆盖验证所需）。
fn build_command(sub: &str, pos: &[String]) -> anyhow::Result<Command> {
    let get_opt = |key: &str| -> Option<String> {
        pos.iter()
            .find_map(|s| s.strip_prefix(&format!("{key}=")).map(|v| v.to_string()))
    };
    let bare: Vec<&String> = pos.iter().filter(|s| !s.contains('=')).collect();

    let cmd = match sub {
        "launch" => {
            let main_class = bare.first().map(|s| s.to_string())
                .ok_or_else(|| anyhow::anyhow!("launch needs <MainClass>"))?;
            Command::Launch {
                main_class,
                classpath: get_opt("--classpath").into_iter().collect(),
                sourcepath: get_opt("--sourcepath").into_iter().collect(),
                app_args: vec![],
                jdb_args: vec![],
                name: None,
                jdb_path: None,
            }
        }
        "status" => Command::Status,
        "list" => Command::List,
        "kill" => Command::Kill,
        "break-at" => Command::BreakAt {
            class: bare.first().map(|s| s.to_string()).ok_or_else(|| anyhow::anyhow!("break-at <Class> <line>"))?,
            line: bare.get(1).and_then(|s| s.parse().ok()).ok_or_else(|| anyhow::anyhow!("break-at needs <line>"))?,
        },
        "break-in" => Command::BreakIn {
            class: bare.first().map(|s| s.to_string()).ok_or_else(|| anyhow::anyhow!("break-in <Class> <method>"))?,
            method: bare.get(1).map(|s| s.to_string()).ok_or_else(|| anyhow::anyhow!("break-in needs <method>"))?,
            args: None,
        },
        "run" => Command::Run,
        "cont" => Command::Cont,
        "step" => Command::Step,
        "next" => Command::Next,
        "step-out" => Command::StepOut,
        "where" => Command::Where { all: false },
        "locals" => Command::Locals,
        "print" => Command::Print { expr: bare.first().map(|s| s.to_string()).ok_or_else(|| anyhow::anyhow!("print <expr>"))? },
        "threads" => Command::Threads,
        "list-source" => Command::ListSource { line: bare.first().and_then(|s| s.parse().ok()) },
        "raw" => Command::Raw { command: bare.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" ") },
        "daemon-status" => Command::DaemonStatus,
        "daemon-stop" => Command::DaemonStop,
        other => anyhow::bail!("unknown subcommand: {other}"),
    };
    Ok(cmd)
}

/// 打印响应（本阶段直接 JSON；roadmap 6 接 output.rs 做文本渲染）。
fn print_response(resp: &Response, _json: bool) {
    if resp.ok {
        if let Some(result) = &resp.result {
            println!("{}", serde_json::to_string_pretty(result).unwrap());
        } else {
            println!("ok");
        }
    } else if let Some(e) = &resp.error {
        eprintln!("[{}] {}", e.code, e.message);
    }
}

fn print_usage() {
    eprintln!(
        "jdbg — Java debugger CLI\n\n\
         USAGE:\n  \
         jdbg launch <MainClass> [--classpath CP] [--sourcepath SP]\n  \
         jdbg break-at <Class> <line> | break-in <Class> <method>\n  \
         jdbg run | cont | step | next | step-out\n  \
         jdbg where | locals | print <expr> | threads | list-source [line]\n  \
         jdbg status | list | kill [--session ID]\n  \
         jdbg raw <jdb command...>\n  \
         jdbg daemon-status | daemon-stop\n\n\
         GLOBAL: --session <id>  --json"
    );
}
