//! Java sidecar process lifecycle helpers.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rand::Rng;
use serde_json::json;

use crate::error::{Error, Result};
use crate::jdi::local::PendingSidecarConnection;
use crate::jdi::protocol::{
    HandshakeRequest, SIDECAR_PROTOCOL_VERSION, SidecarMessage, validate_handshake,
};
use crate::jdi::transport::{
    SidecarStream, SidecarTransport, SidecarTransportError, read_framed_message,
    write_framed_message,
};

pub const SIDECAR_JAR_NAME: &str = "jdbg-jdi-sidecar.jar";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidecarEndpoint {
    pub transport: String,
    pub endpoint: String,
}

impl SidecarEndpoint {
    pub fn new(transport: impl Into<String>, endpoint: impl Into<String>) -> Self {
        Self {
            transport: transport.into(),
            endpoint: endpoint.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidecarPaths {
    pub java_path: PathBuf,
    pub jar_path: PathBuf,
    pub tools_jar: Option<PathBuf>,
}

pub struct LaunchedSidecar {
    transport: SidecarTransport,
    child: Mutex<Child>,
    stderr: Arc<Mutex<String>>,
    _stderr_handle: JoinHandle<()>,
}

impl LaunchedSidecar {
    pub fn transport(&self) -> &SidecarTransport {
        &self.transport
    }

    pub fn pid(&self) -> u32 {
        self.child
            .lock()
            .expect("sidecar child mutex poisoned")
            .id()
    }

    pub fn is_alive(&self) -> bool {
        let mut child = self.child.lock().expect("sidecar child mutex poisoned");
        child.try_wait().map(|s| s.is_none()).unwrap_or(false)
    }

    pub fn take_stderr(&self) -> Option<String> {
        let mut stderr = self.stderr.lock().expect("sidecar stderr mutex poisoned");
        if stderr.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut *stderr))
        }
    }

    pub fn shutdown(&self, timeout: Duration) -> Result<()> {
        let _ = self.transport.shutdown(timeout);
        let deadline = Instant::now() + timeout;
        let mut child = self.child.lock().expect("sidecar child mutex poisoned");
        while Instant::now() < deadline {
            if child.try_wait()?.is_some() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let _ = child.kill();
        let _ = child.wait();
        Ok(())
    }
}

impl Drop for LaunchedSidecar {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

pub fn resolve_sidecar_paths() -> std::io::Result<SidecarPaths> {
    let java_path = std::env::var_os("JDBG_JDI_JAVA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("java"));
    let jar_path = match std::env::var_os("JDBG_JDI_SIDECAR_JAR") {
        Some(p) => PathBuf::from(p),
        None => default_sidecar_jar_path()?,
    };
    let tools_jar = resolve_tools_jar(&java_path);
    Ok(SidecarPaths {
        java_path,
        jar_path,
        tools_jar,
    })
}

pub fn default_sidecar_jar_path() -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(default_sidecar_jar_path_from_exe(&exe))
}

pub fn default_sidecar_jar_path_from_exe(exe: &Path) -> PathBuf {
    let dir = exe.parent().unwrap_or_else(|| Path::new("."));
    if dir.file_name().and_then(|name| name.to_str()) == Some("deps") {
        if let Some(profile_dir) = dir.parent() {
            return profile_dir.join(SIDECAR_JAR_NAME);
        }
    }
    dir.join(SIDECAR_JAR_NAME)
}

pub fn generate_auth_token() -> String {
    rand::rng()
        .sample_iter(&rand::distr::Alphanumeric)
        .take(48)
        .map(char::from)
        .collect()
}

pub fn launch_sidecar(paths: SidecarPaths, timeout: Duration) -> Result<LaunchedSidecar> {
    if !paths.jar_path.is_file() {
        return Err(Error::Connection(format!(
            "JDI sidecar jar not found at {}. Set JDBG_JDI_SIDECAR_JAR or place {SIDECAR_JAR_NAME} next to jdbg.",
            paths.jar_path.display()
        )));
    }

    let token = generate_auth_token();
    let pending_connection = PendingSidecarConnection::new(&token[..8])?;
    let endpoint = pending_connection.endpoint();
    let args = sidecar_args(endpoint, &token, SIDECAR_PROTOCOL_VERSION);
    let java_args = sidecar_java_args(&paths.jar_path, paths.tools_jar.as_deref(), &args);

    let mut command = Command::new(&paths.java_path);
    command
        .args(&java_args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command.spawn().map_err(|source| Error::Spawn {
        path: paths.java_path.display().to_string(),
        source,
    })?;
    let stderr = child.stderr.take().expect("stderr piped");
    let (stderr, stderr_handle) = spawn_stderr_drain(stderr);

    let mut stream = match accept_sidecar(pending_connection, &mut child, &stderr, timeout) {
        Ok(stream) => stream,
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
    };

    let transport = match complete_handshake(&mut stream, &token, timeout) {
        Ok(transport) => transport,
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
    };

    Ok(LaunchedSidecar {
        transport,
        child: Mutex::new(child),
        stderr,
        _stderr_handle: stderr_handle,
    })
}

pub fn sidecar_args(endpoint: SidecarEndpoint, token: &str, protocol_version: u32) -> Vec<String> {
    vec![
        "--transport".into(),
        endpoint.transport,
        "--endpoint".into(),
        endpoint.endpoint,
        "--token".into(),
        token.into(),
        "--protocol-version".into(),
        protocol_version.to_string(),
    ]
}

pub fn sidecar_java_args(
    jar_path: &Path,
    tools_jar: Option<&Path>,
    sidecar_args: &[String],
) -> Vec<String> {
    let mut args = Vec::new();
    #[cfg(unix)]
    {
        args.push("-XX:+IgnoreUnrecognizedVMOptions".into());
        args.push("--add-opens=java.base/java.io=ALL-UNNAMED".into());
    }
    if let Some(tools_jar) = tools_jar {
        args.push("-cp".into());
        args.push(format!(
            "{}{}{}",
            jar_path.display(),
            classpath_separator(),
            tools_jar.display()
        ));
        args.push("dev.jdbg.sidecar.SidecarMain".into());
    } else {
        args.push("-jar".into());
        args.push(jar_path.display().to_string());
    }
    args.extend(sidecar_args.iter().cloned());
    args
}

pub fn redact_sidecar_args(args: &[String]) -> Vec<String> {
    let mut redacted = Vec::with_capacity(args.len());
    let mut hide_next = false;
    for arg in args {
        if hide_next {
            redacted.push("<redacted>".into());
            hide_next = false;
            continue;
        }
        if arg == "--token" {
            hide_next = true;
        }
        redacted.push(arg.clone());
    }
    redacted
}

fn accept_sidecar(
    pending_connection: PendingSidecarConnection,
    child: &mut Child,
    stderr: &Arc<Mutex<String>>,
    timeout: Duration,
) -> Result<SidecarStream> {
    let deadline = Instant::now() + timeout;
    let endpoint = pending_connection.endpoint();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(pending_connection.accept());
    });

    loop {
        match rx.recv_timeout(Duration::from_millis(10)) {
            Ok(Ok(stream)) => return Ok(stream),
            Ok(Err(e)) => return Err(e.into()),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(Error::Connection(
                    "JDI sidecar local transport accept thread disconnected".into(),
                ));
            }
        }

        if let Some(status) = child.try_wait()? {
            nudge_accept(&endpoint);
            let detail = take_stderr(stderr).unwrap_or_else(|| "no sidecar stderr".into());
            return Err(Error::Connection(format!(
                "JDI sidecar exited before connecting back (status {status}): {}",
                detail.trim()
            )));
        }
        if Instant::now() >= deadline {
            nudge_accept(&endpoint);
            let detail = take_stderr(stderr).unwrap_or_else(|| "no sidecar stderr".into());
            return Err(Error::Connection(format!(
                "timed out waiting for JDI sidecar connection: {}",
                detail.trim()
            )));
        }
    }
}

fn complete_handshake(
    stream: &mut SidecarStream,
    token: &str,
    _timeout: Duration,
) -> Result<SidecarTransport> {
    let msg = read_framed_message(stream).map_err(sidecar_transport_error)?;
    let SidecarMessage::Request { id, method, params } = msg else {
        return Err(Error::Connection(
            "JDI sidecar sent non-request handshake frame".into(),
        ));
    };
    if method != "handshake" {
        return Err(Error::Connection(format!(
            "JDI sidecar sent unexpected handshake method '{method}'"
        )));
    }
    let request: HandshakeRequest =
        serde_json::from_value(params).map_err(|e| Error::Connection(e.to_string()))?;
    let response =
        validate_handshake(&request, token).map_err(|e| Error::Connection(e.to_string()))?;
    write_framed_message(
        stream,
        &SidecarMessage::Response {
            id,
            result: Some(json!(response)),
            error: None,
        },
    )
    .map_err(sidecar_transport_error)?;
    SidecarTransport::start(stream.try_clone()?).map_err(|e| Error::Connection(e.to_string()))
}

#[cfg(windows)]
fn nudge_accept(endpoint: &SidecarEndpoint) {
    if endpoint.transport == "named-pipe" {
        let _ = std::fs::File::open(format!("{}-to-sidecar", endpoint.endpoint));
        let _ = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(format!("{}-from-sidecar", endpoint.endpoint));
    }
}

#[cfg(not(windows))]
fn nudge_accept(_endpoint: &SidecarEndpoint) {}

fn sidecar_transport_error(e: SidecarTransportError) -> Error {
    Error::Connection(e.to_string())
}

fn spawn_stderr_drain(stderr: std::process::ChildStderr) -> (Arc<Mutex<String>>, JoinHandle<()>) {
    let buf = Arc::new(Mutex::new(String::new()));
    let buf2 = Arc::clone(&buf);
    let handle = std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    if let Ok(mut b) = buf2.lock() {
                        b.push_str(&line);
                    }
                }
                Err(_) => break,
            }
        }
    });
    (buf, handle)
}

fn take_stderr(stderr: &Arc<Mutex<String>>) -> Option<String> {
    let mut stderr = stderr.lock().ok()?;
    if stderr.is_empty() {
        None
    } else {
        Some(std::mem::take(&mut *stderr))
    }
}

fn resolve_tools_jar(java_path: &Path) -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("JDBG_JDI_TOOLS_JAR").map(PathBuf::from) {
        return path.is_file().then_some(path);
    }
    if let Some(path) = std::env::var_os("JAVA_HOME")
        .map(PathBuf::from)
        .map(|home| home.join("lib").join("tools.jar"))
        .filter(|path| path.is_file())
    {
        return Some(path);
    }
    let java_home_tools = java_path
        .parent()
        .and_then(|bin| bin.parent())
        .map(|home| home.join("lib").join("tools.jar"));
    java_home_tools.filter(|path| path.is_file())
}

fn classpath_separator() -> char {
    if cfg!(windows) { ';' } else { ':' }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn launch_args_include_transport_endpoint_protocol_and_token() {
        let args = sidecar_args(
            SidecarEndpoint::new("named-pipe", r"\\.\pipe\jdbg-jdi-test"),
            "secret-token",
            1,
        );

        assert_eq!(
            args,
            vec![
                "--transport",
                "named-pipe",
                "--endpoint",
                r"\\.\pipe\jdbg-jdi-test",
                "--token",
                "secret-token",
                "--protocol-version",
                "1"
            ]
        );
    }

    #[test]
    fn java_args_use_classpath_when_tools_jar_is_available() {
        let args = sidecar_java_args(
            Path::new("jdbg-jdi-sidecar.jar"),
            Some(Path::new("tools.jar")),
            &sidecar_args(
                SidecarEndpoint::new("named-pipe", "pipe-name"),
                "secret-token",
                1,
            ),
        );

        let cp_index = args.iter().position(|arg| arg == "-cp").unwrap();
        assert!(args[cp_index + 1].contains("jdbg-jdi-sidecar.jar"));
        assert!(args[cp_index + 1].contains("tools.jar"));
        assert_eq!(args[cp_index + 2], "dev.jdbg.sidecar.SidecarMain");
    }

    #[test]
    fn redacted_args_never_print_auth_token() {
        let args = sidecar_args(
            SidecarEndpoint::new("named-pipe", "pipe-name"),
            "secret-token",
            1,
        );

        let redacted = redact_sidecar_args(&args);

        assert_eq!(
            redacted,
            vec![
                "--transport",
                "named-pipe",
                "--endpoint",
                "pipe-name",
                "--token",
                "<redacted>",
                "--protocol-version",
                "1"
            ]
        );
        assert!(!redacted.iter().any(|arg| arg == "secret-token"));
    }

    #[test]
    fn default_jar_path_lives_next_to_binary() {
        let exe = PathBuf::from(r"C:\tools\jdbg.exe");

        assert_eq!(
            default_sidecar_jar_path_from_exe(&exe),
            PathBuf::from(r"C:\tools\jdbg-jdi-sidecar.jar")
        );
    }

    #[test]
    fn default_jar_path_for_cargo_test_binary_lives_in_profile_dir() {
        let exe = PathBuf::from(r"C:\repo\target\debug\deps\server_integration.exe");

        assert_eq!(
            default_sidecar_jar_path_from_exe(&exe),
            PathBuf::from(r"C:\repo\target\debug\jdbg-jdi-sidecar.jar")
        );
    }

    #[test]
    fn generated_tokens_are_url_safe_and_not_empty() {
        let token = generate_auth_token();

        assert!(token.len() >= 32);
        assert!(token.chars().all(|c| c.is_ascii_alphanumeric()));
    }
}
