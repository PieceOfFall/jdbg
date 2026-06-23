//! Spawn 并控制 jdb 子进程。
//!
//! 用 `std::process::Command` + piped stdio（**不用 ConPTY**，§5）。
//! 始终带上 MANDATORY 的 `-J` flags 强制英文 locale，否则本机 jdb 输出乱码中文、解析失败。

use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};

use crate::error::{Error, Result};

/// 强制英文 locale + UTF-8 的 jdb flags（§5，不可省略）。
const LOCALE_FLAGS: &[&str] = &[
    "-J-Duser.language=en",
    "-J-Duser.country=US",
    "-J-Dfile.encoding=UTF-8",
];

/// 平台相关的 classpath/sourcepath 分隔符。
#[cfg(windows)]
const PATH_SEP: &str = ";";
#[cfg(not(windows))]
const PATH_SEP: &str = ":";

/// launch 模式配置（`jdbg launch`）。
#[derive(Debug, Clone, Default)]
pub struct LaunchConfig {
    pub main_class: String,
    pub classpath: Vec<PathBuf>,
    pub sourcepath: Vec<PathBuf>,
    pub app_args: Vec<String>,
    /// 透传给 jdb 的额外参数（`--jdb-arg`）。
    pub jdb_args: Vec<String>,
}

/// spawn 后交出的句柄集合：进程 + stdin 封装在 `JdbProcess`，stdout/stderr 交给 reader 线程。
pub struct Spawned {
    pub process: JdbProcess,
    pub stdout: ChildStdout,
    pub stderr: ChildStderr,
}

/// 持有 jdb 子进程与其 stdin。一次只写一条命令（§5：每会话单命令在飞）。
pub struct JdbProcess {
    child: Child,
    stdin: ChildStdin,
}

/// 按 launch 模式 spawn jdb。
///
/// 参数顺序载荷敏感：`jdb <-J flags> [-sourcepath SP] [-classpath CP] [jdb_args] MainClass [app_args]`
/// ——所有 `-` flag 必须在 MainClass 之前，app args 在最后。
pub fn spawn_launch(jdb_path: &Path, config: &LaunchConfig) -> Result<Spawned> {
    let args = build_launch_args(config);
    spawn(jdb_path, &args)
}

/// 构造 launch 模式的完整参数列表（不含 jdb 可执行文件本身）。
pub fn build_launch_args(config: &LaunchConfig) -> Vec<String> {
    let mut args: Vec<String> = LOCALE_FLAGS.iter().map(|s| s.to_string()).collect();

    if !config.sourcepath.is_empty() {
        args.push("-sourcepath".into());
        args.push(join_paths(&config.sourcepath));
    }
    if !config.classpath.is_empty() {
        args.push("-classpath".into());
        args.push(join_paths(&config.classpath));
    }
    args.extend(config.jdb_args.iter().cloned());
    args.push(config.main_class.clone());
    args.extend(config.app_args.iter().cloned());
    args
}

/// 用平台分隔符拼接多个路径。
fn join_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(PATH_SEP)
}

/// 实际 spawn：piped stdin/stdout/stderr。
fn spawn(jdb_path: &Path, args: &[String]) -> Result<Spawned> {
    let mut child = Command::new(jdb_path)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| Error::Spawn {
            path: jdb_path.display().to_string(),
            source,
        })?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| Error::SessionDead("failed to capture jdb stdin".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::SessionDead("failed to capture jdb stdout".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| Error::SessionDead("failed to capture jdb stderr".into()))?;

    Ok(Spawned {
        process: JdbProcess { child, stdin },
        stdout,
        stderr,
    })
}

impl JdbProcess {
    /// 向 jdb stdin 写一条命令（自动补 `\n` 并 flush）。
    pub fn write_command(&mut self, cmd: &str) -> Result<()> {
        use std::io::Write;
        self.stdin.write_all(cmd.as_bytes())?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
        Ok(())
    }

    /// 子进程 PID。
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// 是否仍在运行（非阻塞探测）。
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// 强制结束 jdb 子进程。
    pub fn kill(&mut self) -> Result<()> {
        self.child.kill()?;
        let _ = self.child.wait();
        Ok(())
    }
}
