//! CLI 端 client：connect-or-auto-spawn daemon，发一条 Request，收一条 Response。

use std::io::{BufRead, BufReader, Write};
use std::time::{Duration, Instant};

use interprocess::local_socket::{prelude::*, Stream as LocalStream};

use crate::protocol::{Request, Response};
use crate::registry;

/// 连接超时。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// 自动 spawn 后轮询间隔。
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// 发送请求并接收响应（公共 API，daemon 模块 `stop_daemon` 也用到）。
pub fn send_request(req: &Request) -> anyhow::Result<Response> {
    let stream = connect_or_spawn()?;
    let mut writer = stream;
    let json = serde_json::to_string(req)?;
    writer.write_all(json.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()?;

    let mut reader = BufReader::new(&writer);
    let mut line = String::new();
    reader.read_line(&mut line)?;

    let resp: Response = serde_json::from_str(line.trim())?;
    Ok(resp)
}

/// 连接到 daemon socket；若连接失败则自动 spawn daemon 并重试。
fn connect_or_spawn() -> anyhow::Result<LocalStream> {
    let sock_name = registry::socket_name();

    // 先尝试直连。
    if let Ok(stream) = try_connect(&sock_name) {
        return Ok(stream);
    }

    // 连接失败——自动拉起 daemon。
    crate::daemon::spawn_daemon_detached()?;

    // 轮询等待 daemon 就绪（bounded by CONNECT_TIMEOUT）。
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    loop {
        if Instant::now() >= deadline {
            anyhow::bail!(
                "daemon did not start within {}s (socket: {sock_name})",
                CONNECT_TIMEOUT.as_secs()
            );
        }
        std::thread::sleep(POLL_INTERVAL);
        if let Ok(stream) = try_connect(&sock_name) {
            return Ok(stream);
        }
    }
}

/// 尝试连接一次。
fn try_connect(sock_name: &str) -> Result<LocalStream, std::io::Error> {
    let name = sock_name
        .to_ns_name::<interprocess::local_socket::GenericNamespaced>()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    LocalStream::connect(name)
}
