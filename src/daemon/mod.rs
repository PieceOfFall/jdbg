//! Daemon 子系统：IPC 监听、会话管理、生命周期。
//!
//! 架构（§4）：
//! - 一个 daemon per user login，长驻后台。
//! - 用 `interprocess` LocalSocket（Windows 命名管道 / Unix 抽象 socket）。
//! - Accept loop 每连接 spawn 一个短命处理线程。

pub mod handler;
pub mod manager;

use std::io;
use std::sync::Arc;

use interprocess::local_socket::{ListenerOptions, prelude::*};

use crate::registry::{self, DaemonInfo, Registry};
use manager::SessionManager;

/// Daemon 主循环入口——绑定 socket、注册磁盘信息、accept 循环。
///
/// 幂等 bind：如果 socket 已被占用（另一个 daemon 先启动），返回 `Err` 让调用方 exit 0。
pub fn run_daemon() -> anyhow::Result<()> {
    let sock_name = registry::socket_name();
    let name = sock_name.clone().to_ns_name::<interprocess::local_socket::GenericNamespaced>()?;

    let listener = match ListenerOptions::new().name(name).create_sync() {
        Ok(l) => l,
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
            // 幂等 bind 失败——另一个 daemon 已在服务，静默退出。
            eprintln!("[daemon] socket already in use, another daemon is serving. Exiting.");
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };

    // 注册磁盘信息。
    let registry = Registry::open()?;
    let info = DaemonInfo {
        pid: std::process::id(),
        socket_name: sock_name.clone(),
        version: env!("CARGO_PKG_VERSION").into(),
        started_at: jiff::Zoned::now().to_string(),
    };
    registry.write_daemon(&info)?;

    eprintln!("[daemon] listening on {sock_name} (pid={})", info.pid);

    let mgr = Arc::new(SessionManager::new(registry));

    // Accept loop——每连接 spawn 一个短命线程。
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let mgr = Arc::clone(&mgr);
                std::thread::spawn(move || {
                    if let Err(e) = handler::handle_connection(stream, &mgr) {
                        eprintln!("[daemon] connection error: {e}");
                    }
                });
            }
            Err(e) => {
                eprintln!("[daemon] accept error: {e}");
            }
        }
    }

    // 清理（正常不会到这里，incoming() 是无限迭代器）。
    mgr.shutdown();
    Ok(())
}

/// Detached spawn helper——CLI 用来自动拉起 daemon。
///
/// Windows: `CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS`，stdio null。
/// Unix: `setsid` via pre_exec，stdio null。
pub fn spawn_daemon_detached() -> io::Result<()> {
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("__daemon");
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NEW_PROCESS_GROUP (0x200) | DETACHED_PROCESS (0x08)
        cmd.creation_flags(0x0000_0208);
    }

    #[cfg(unix)]
    {
        // setsid 需要 libc/nix（§9 排除）；当前依赖 stdio null + 父进程不 wait 做基本 detach。
        // 如需完整脱离控制终端，后续可在此补 setsid。
    }

    cmd.spawn()?;
    Ok(())
}

/// 停止 daemon：连接到 socket 发 `DaemonStop` 命令。
pub fn stop_daemon() -> anyhow::Result<()> {
    use crate::protocol::{Command, Request};
    let req = Request::new(Command::DaemonStop, None);
    let resp = crate::client::send_request(&req)?;
    if resp.ok {
        println!("Daemon stopped.");
    } else if let Some(e) = resp.error {
        eprintln!("Failed to stop daemon: {}", e.message);
    }
    Ok(())
}
