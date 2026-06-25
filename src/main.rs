//! `jdbg` 入口——用 clap 解析命令行，分发到 daemon 模式或 client 模式。

use std::process::ExitCode;

use clap::Parser;

use java_agent_debugger::cli::{Cli, Commands, DaemonAction};
use java_agent_debugger::client;
use java_agent_debugger::daemon;
use java_agent_debugger::mcp;
use java_agent_debugger::output;
use java_agent_debugger::protocol::*;
use java_agent_debugger::setup;
use java_agent_debugger::update;

fn main() -> ExitCode {
    let cli = Cli::parse();

    // 隐藏的 daemon 模式。
    if matches!(cli.command, Commands::Daemon_) {
        if let Err(e) = daemon::run_daemon() {
            eprintln!("daemon error: {e}");
            return ExitCode::from(1);
        }
        return ExitCode::SUCCESS;
    }

    // 隐藏的 MCP server 模式（stdio，由 Claude Code 拉起）。
    if matches!(cli.command, Commands::Mcp_) {
        if let Err(e) = mcp::run_mcp() {
            eprintln!("mcp error: {e}");
            return ExitCode::from(1);
        }
        return ExitCode::SUCCESS;
    }

    match run_client(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}

fn run_client(cli: Cli) -> anyhow::Result<ExitCode> {
    // Daemon control shortcuts (不需要 session，先于 build_command 处理)。
    if let Commands::Daemon { action } = &cli.command {
        return match action {
            DaemonAction::Stop => { daemon::stop_daemon()?; Ok(ExitCode::SUCCESS) }
            DaemonAction::Start => { daemon::spawn_daemon_detached()?; println!("Daemon started."); Ok(ExitCode::SUCCESS) }
            DaemonAction::Status => {
                let req = Request::new(Command::DaemonStatus, None);
                let resp = client::send_request(&req)?;
                if let Some(result) = resp.result.as_ref() {
                    println!("{}", output::render(result, cli.json));
                }
                Ok(ExitCode::SUCCESS)
            }
        };
    }

    // Setup: 纯客户端操作，不联系 daemon。
    if let Commands::Setup { remove, print } = &cli.command {
        setup::run_setup(*remove, *print)?;
        return Ok(ExitCode::SUCCESS);
    }

    // Update: remove → install latest → setup。
    if let Commands::Update = &cli.command {
        update::run_update()?;
        return Ok(ExitCode::SUCCESS);
    }

    let cmd = build_command(&cli)?;
    let req = Request::new(cmd, cli.session.clone()).with_timeout(cli.timeout);
    let resp = client::send_request(&req)?;

    if resp.ok {
        if let Some(result) = &resp.result {
            if cli.json {
                println!("{}", serde_json::to_string_pretty(result).unwrap());
            } else {
                println!("{}", output::render(result, false));
            }
        }
        Ok(ExitCode::SUCCESS)
    } else {
        if let Some(e) = &resp.error {
            eprintln!("[{}] {}", e.code, e.message);
        }
        Ok(ExitCode::from(1))
    }
}

/// 把 clap Commands 转换为 protocol Command。
fn build_command(cli: &Cli) -> anyhow::Result<Command> {
    let cmd = match &cli.command {
        Commands::Launch { main_class, classpath, sourcepath, app_args, jdb_args, name } => {
            Command::Launch {
                main_class: main_class.clone(),
                classpath: classpath.as_deref().map(|s| vec![s.to_string()]).unwrap_or_default(),
                sourcepath: sourcepath.as_deref().map(|s| vec![s.to_string()]).unwrap_or_default(),
                app_args: app_args.clone(),
                jdb_args: jdb_args.clone(),
                name: name.clone(),
                jdb_path: cli.jdb_path.clone(),
            }
        }
        Commands::Attach { host, port, sourcepath, name } => {
            Command::Attach {
                host: host.clone(),
                port: *port,
                sourcepath: sourcepath.as_deref().map(|s| vec![s.to_string()]).unwrap_or_default(),
                name: name.clone(),
                jdb_path: cli.jdb_path.clone(),
            }
        }
        Commands::Status => Command::Status,
        Commands::List => Command::List,
        Commands::Kill => Command::Kill,
        Commands::BreakAt { class, line, condition, suspend } => Command::BreakAt { class: class.clone(), line: *line, condition: condition.clone(), suspend: suspend.clone() },
        Commands::BreakIn { class, method, args, condition, suspend } => {
            Command::BreakIn { class: class.clone(), method: method.clone(), args: args.clone(), condition: condition.clone(), suspend: suspend.clone() }
        }
        Commands::Catch { exception, mode } => {
            Command::Catch { exception: exception.clone(), mode: mode.clone() }
        }
        Commands::Watch { field, mode } => Command::Watch { field: field.clone(), mode: mode.clone() },
        Commands::Unwatch { field } => Command::Unwatch { field: field.clone() },
        Commands::Breakpoints => Command::Breakpoints,
        Commands::Clear { spec } => Command::Clear { spec: spec.clone() },
        Commands::Classes { pattern } => Command::Classes { pattern: pattern.clone() },
        Commands::Methods { class } => Command::Methods { class: class.clone() },
        Commands::Run => Command::Run,
        Commands::Cont => Command::Cont,
        Commands::Step => Command::Step,
        Commands::Next => Command::Next,
        Commands::StepOut => Command::StepOut,
        Commands::Where { all } => Command::Where { all: *all },
        Commands::Locals => Command::Locals,
        Commands::Print { expr } => Command::Print { expr: expr.clone() },
        Commands::Dump { expr } => Command::Dump { expr: expr.clone() },
        Commands::Eval { expr } => Command::Eval { expr: expr.clone() },
        Commands::Threads => Command::Threads,
        Commands::Thread { id } => Command::Thread { id: id.clone() },
        Commands::Frame { direction, n } => Command::Frame { direction: direction.clone(), n: *n },
        Commands::ListSource { line } => Command::ListSource { line: *line },
        Commands::Inspect { expr, max_elements } => Command::Inspect { expr: expr.clone(), max_elements: *max_elements },
        Commands::Raw { command } => Command::Raw { command: command.join(" ") },
        Commands::Daemon { .. } => unreachable!("handled above"),
        Commands::Setup { .. } => unreachable!("handled above"),
        Commands::Update => unreachable!("handled above"),
        Commands::Daemon_ => unreachable!("handled above"),
        Commands::Mcp_ => unreachable!("handled above"),
    };
    Ok(cmd)
}
