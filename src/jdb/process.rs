//! Spawn and control the jdb child process.
//!
//! Uses `std::process::Command` with piped stdio (**not ConPTY**, §5).
//! Always include the mandatory `-J` flags to force the English locale; otherwise this machine's jdb emits
//! localized Chinese output and parsing fails.

use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};

use crate::error::{Error, Result};

/// jdb flags that force English locale + UTF-8 (§5, mandatory).
const LOCALE_FLAGS: &[&str] = &[
    "-J-Duser.language=en",
    "-J-Duser.country=US",
    "-J-Dfile.encoding=UTF-8",
];

/// Platform-specific classpath/sourcepath separator.
#[cfg(windows)]
const PATH_SEP: &str = ";";
#[cfg(not(windows))]
const PATH_SEP: &str = ":";

/// Launch-mode config (`jdbg launch`).
#[derive(Debug, Clone, Default)]
pub struct LaunchConfig {
    pub main_class: String,
    pub classpath: Vec<PathBuf>,
    pub sourcepath: Vec<PathBuf>,
    pub app_args: Vec<String>,
    /// Extra arguments passed through to jdb (`--jdb-arg`).
    pub jdb_args: Vec<String>,
}

/// Attach-mode config (`jdbg attach`): connect to a running JVM's JDWP port.
#[derive(Debug, Clone)]
pub struct AttachConfig {
    pub host: String,
    pub port: u16,
    pub sourcepath: Vec<PathBuf>,
}

/// Handles returned after spawn: process + stdin in `JdbProcess`, stdout/stderr handed to reader threads.
pub struct Spawned {
    pub process: JdbProcess,
    pub stdout: ChildStdout,
    pub stderr: ChildStderr,
}

/// Owns the jdb child process and its stdin. Write only one command at a time (§5: one in-flight command per session).
pub struct JdbProcess {
    child: Child,
    stdin: ChildStdin,
}

/// Spawn jdb in launch mode.
///
/// Argument order is payload-sensitive:
/// `jdb <-J flags> [-sourcepath SP] [-classpath CP] [jdb_args] MainClass [app_args]`.
/// All `-` flags must come before MainClass, and app args come last.
pub fn spawn_launch(jdb_path: &Path, config: &LaunchConfig) -> Result<Spawned> {
    let args = build_launch_args(config);
    spawn(jdb_path, &args)
}

/// Build the full launch-mode argument list, excluding the jdb executable itself.
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

/// Spawn jdb in attach mode.
///
/// Command line: `jdb <-J flags> -connect com.sun.jdi.SocketAttach:hostname=H,port=P [-sourcepath SP]`.
/// **Must use the explicit SocketAttach connector**: on Windows, `jdb -attach host:port` defaults to
/// shared-memory (dt_shmem), which does not match JDWP `dt_socket` and causes `Unable to attach` followed
/// by immediate jdb exit (§10). `-connect` forces socket transport consistently across platforms.
pub fn spawn_attach(jdb_path: &Path, config: &AttachConfig) -> Result<Spawned> {
    let args = build_attach_args(config);
    spawn(jdb_path, &args)
}

/// Normalize an attach target host: replace case-insensitive `localhost` with the IPv4 loopback literal
/// `127.0.0.1`, and leave other hostnames (literal IPs, `::1`, remote hosts) unchanged.
///
/// **Why this is needed**: on dual-stack machines, `localhost` often resolves to IPv6 `::1` first, while
/// JDWP defaults (`address=5005` or `*:5005`) usually listen only on IPv4 `0.0.0.0`. Then both `probe_tcp`
/// and jdb's `SocketAttach:hostname=localhost` connect to `::1`, get connection refused, and attach fails.
/// Forcing the IPv4 loopback literal bypasses DNS's IPv6 preference and matches the address family JDWP is
/// actually listening on. Users who explicitly want IPv6 loopback can pass `::1`, which is preserved.
pub fn normalize_attach_host(host: &str) -> String {
    if host.eq_ignore_ascii_case("localhost") {
        "127.0.0.1".to_string()
    } else {
        host.to_string()
    }
}

/// Build the full attach-mode argument list, excluding the jdb executable itself.
pub fn build_attach_args(config: &AttachConfig) -> Vec<String> {
    let mut args: Vec<String> = LOCALE_FLAGS.iter().map(|s| s.to_string()).collect();
    args.push("-connect".into());
    args.push(format!(
        "com.sun.jdi.SocketAttach:hostname={},port={}",
        config.host, config.port
    ));
    if !config.sourcepath.is_empty() {
        args.push("-sourcepath".into());
        args.push(join_paths(&config.sourcepath));
    }
    args
}

/// Join multiple paths with the platform separator.
fn join_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(PATH_SEP)
}

/// Actual spawn with piped stdin/stdout/stderr.
fn spawn(jdb_path: &Path, args: &[String]) -> Result<Spawned> {
    let mut cmd = Command::new(jdb_path);
    cmd.args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Windows: avoid popping an empty console window for jdb. The daemon runs as DETACHED_PROCESS
    // without a console, so the system would create a new console window for console-subsystem jdb.exe.
    // stdio is fully piped, so that window is empty noise. CREATE_NO_WINDOW runs jdb in an invisible
    // console inherited by the child JVM, without affecting pipe I/O.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = cmd.spawn().map_err(|source| Error::Spawn {
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
    /// Write one command to jdb stdin, automatically appending `\n` and flushing.
    pub fn write_command(&mut self, cmd: &str) -> Result<()> {
        use std::io::Write;
        self.stdin.write_all(cmd.as_bytes())?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
        Ok(())
    }

    /// Child process PID.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Whether the process is still running, checked non-blockingly.
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Force-kill the jdb child process.
    pub fn kill(&mut self) -> Result<()> {
        self.child.kill()?;
        let _ = self.child.wait();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn starts_with_locale(args: &[String]) -> bool {
        let head: Vec<&str> = args
            .iter()
            .take(LOCALE_FLAGS.len())
            .map(|s| s.as_str())
            .collect();
        head.as_slice() == LOCALE_FLAGS
    }

    #[test]
    fn launch_args_have_locale_flags_and_order() {
        let cfg = LaunchConfig {
            main_class: "Main".into(),
            classpath: vec![PathBuf::from("fixtures")],
            ..Default::default()
        };
        let args = build_launch_args(&cfg);
        assert!(starts_with_locale(&args));
        // MainClass must come after all `-` flags.
        let cp = args.iter().position(|a| a == "-classpath").unwrap();
        let main = args.iter().position(|a| a == "Main").unwrap();
        assert!(cp < main, "args: {args:?}");
    }

    #[test]
    fn attach_args_use_socket_connector_not_dash_attach() {
        // Key regression: on Windows `-attach host:port` defaults to shared memory, so use SocketAttach.
        let cfg = AttachConfig {
            host: "localhost".into(),
            port: 5005,
            sourcepath: vec![PathBuf::from("src")],
        };
        let args = build_attach_args(&cfg);
        assert!(starts_with_locale(&args));
        assert!(args.iter().any(|a| a == "-connect"), "args: {args:?}");
        assert!(
            args.iter()
                .any(|a| a == "com.sun.jdi.SocketAttach:hostname=localhost,port=5005"),
            "args: {args:?}"
        );
        assert!(
            !args.iter().any(|a| a == "-attach"),
            "must not use -attach: {args:?}"
        );
        assert!(args.iter().any(|a| a == "-sourcepath"), "args: {args:?}");
    }

    #[test]
    fn normalize_attach_host_maps_localhost_to_ipv4_loopback() {
        // On dual-stack machines localhost→::1, but JDWP usually listens only on IPv4, so use 127.0.0.1.
        assert_eq!(normalize_attach_host("localhost"), "127.0.0.1");
        // Case-insensitive because DNS names are case-insensitive.
        assert_eq!(normalize_attach_host("LocalHost"), "127.0.0.1");
        assert_eq!(normalize_attach_host("LOCALHOST"), "127.0.0.1");
    }

    #[test]
    fn normalize_attach_host_leaves_other_hosts_untouched() {
        // Preserve literal IPs, remote hosts, and explicit IPv6 loopback.
        assert_eq!(normalize_attach_host("127.0.0.1"), "127.0.0.1");
        assert_eq!(normalize_attach_host("::1"), "::1");
        assert_eq!(normalize_attach_host("10.0.0.5"), "10.0.0.5");
        assert_eq!(
            normalize_attach_host("debug.example.com"),
            "debug.example.com"
        );
    }
}
