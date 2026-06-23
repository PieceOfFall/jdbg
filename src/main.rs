//! 临时 spike 驱动（第三版）：基于 Session API 的端到端验证。
//! 展示 session 层封装——语义方法直接产出结构化 CommandResponse（含 Stopped/VmExited）。
//! 正式 CLI 入口将在后续阶段替换本文件。

use std::path::PathBuf;

use java_agent_debugger::jdb::process::LaunchConfig;
use java_agent_debugger::jdkpath;
use java_agent_debugger::protocol::CommandResponse;
use java_agent_debugger::session::Session;

fn main() -> anyhow::Result<()> {
    println!("=== jdbg spike v3: Session 层端到端验证 ===\n");

    let jdb_path = jdkpath::find_jdb(None)?;
    println!("[+] jdb: {}", jdb_path.display());

    let config = LaunchConfig {
        main_class: "Main".into(),
        classpath: vec![PathBuf::from("fixtures")],
        sourcepath: vec![PathBuf::from("fixtures")],
        ..Default::default()
    };

    // 启动会话（spawn jdb + 起线程 + 读初始 prompt，状态 Loaded）。
    let session = Session::launch(&jdb_path, &config, "spike01".into(), None)?;
    println!("[+] session={} pid={} state={:?}\n", session.meta.id, session.meta.jdb_pid, session.state());

    // 语义方法驱动调试流程——每步直接得到结构化 CommandResponse。
    show("stop at Main:9", session.stop_at("Main", 9)?);
    show("run", session.run()?);            // → Stopped @ Main:9 ? 实际先命中默认入口? 无 method bp，故停 line 9
    show("locals", session.locals()?);
    show("where", session.stack()?);
    show("print count", session.print("count")?);
    show("print label", session.print("label")?);
    show("threads", session.threads()?);
    show("list", session.list_source(None)?);
    show("step", session.step()?);
    show("cont (to exit)", session.cont()?);

    println!("[+] final state: {:?}", session.state());
    session.kill()?;
    println!("\n[+] Session 端到端验证完成。");
    Ok(())
}

fn show(label: &str, resp: CommandResponse) {
    println!("── [{label}] ──");
    println!("{}", serde_json::to_string_pretty(&resp).unwrap());
    println!();
}
