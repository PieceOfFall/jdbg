//! Integration tests: simulate the full daemon handler flow and verify changes against real jdb sessions.
//!
//! These tests require a JDK (jdb available through JAVA_HOME or PATH) and compiled Java fixtures.
//! Before running, ensure: javac -g tests/fixtures/java/CollectionTest.java

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, LazyLock, Mutex, mpsc};
use std::time::{Duration, Instant};

use java_agent_debugger::protocol::*;
use java_agent_debugger::session::Session;
use serde_json::{Value, json};

static JAVAC_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
static JDI_E2E_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

fn jdi_e2e_guard() -> std::sync::MutexGuard<'static, ()> {
    match JDI_E2E_LOCK.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Helper: get the jdb path.
fn jdb_path() -> PathBuf {
    java_agent_debugger::jdkpath::find_jdb(None).expect("jdb not found — is JAVA_HOME set?")
}

/// Helper: fixture directory.
fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("java")
}

fn fixture_line(source_name: &str, needle: &str) -> u32 {
    let path = fixture_dir().join(source_name);
    let source = fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!("failed to read fixture source {}: {e}", path.display());
    });
    source
        .lines()
        .position(|line| line.contains(needle))
        .map(|index| index as u32 + 1)
        .unwrap_or_else(|| {
            panic!(
                "fixture source {} did not contain {needle:?}",
                path.display()
            )
        })
}

fn wait_for_jdi_breakpoint_status(
    session: &java_agent_debugger::jdi::session::JdiSession,
    line: u32,
) -> CommandResult {
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        let status = session.status();
        if matches!(
            &status,
            CommandResult::Status {
                state: RunState::Suspended,
                last_event: Some(Event::Breakpoint { location, .. }),
                ..
            } if location.line == line
        ) {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "JDI async breakpoint did not update status before deadline; last status: {status:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn javac_path() -> PathBuf {
    let exe = if cfg!(windows) { "javac.exe" } else { "javac" };
    jdb_path()
        .parent()
        .map(|bin| bin.join(exe))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from(exe))
}

fn java_path(prefer_windowless: bool) -> PathBuf {
    let exe = if cfg!(windows) && prefer_windowless {
        "javaw.exe"
    } else if cfg!(windows) {
        "java.exe"
    } else {
        "java"
    };
    jdb_path()
        .parent()
        .map(|bin| bin.join(exe))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from(exe))
}

fn hide_console(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
}

fn compile_java_fixture(source_name: &str) {
    let _guard = JAVAC_LOCK.lock().expect("javac fixture lock poisoned");
    let dir = fixture_dir();
    let mut command = Command::new(javac_path());
    command
        .arg("-g")
        .arg("-Xlint:-options")
        .arg("-source")
        .arg("8")
        .arg("-target")
        .arg("8")
        .arg("-d")
        .arg(&dir)
        .arg(dir.join(source_name));
    hide_console(&mut command);
    let status = command.status().expect("failed to spawn javac");
    assert!(status.success(), "javac failed with status {status}");
}

fn run_java_sidecar_self_tests() {
    let sidecar_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("sidecar")
        .join("jdi");
    let mut gradle = gradle_wrapper_command(&sidecar_dir);
    gradle
        .current_dir(&sidecar_dir)
        .arg("--no-daemon")
        .arg("selfTest");
    if let Some(java_home) = gradle_java_home() {
        gradle.env("JAVA_HOME", &java_home);
        if let Some(path) = path_with_java_home(&java_home) {
            gradle.env("PATH", path);
        }
    }
    hide_console(&mut gradle);
    let output = gradle.output().expect("spawn Gradle sidecar self-test");
    assert!(
        output.status.success(),
        "sidecar self-tests failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn gradle_wrapper_command(sidecar_dir: &Path) -> Command {
    if cfg!(windows) {
        Command::new(sidecar_dir.join("gradlew.bat"))
    } else {
        let mut command = Command::new("sh");
        command.arg("./gradlew");
        command
    }
}

fn gradle_java_home() -> Option<PathBuf> {
    std::env::var_os("JDBG_GRADLE_JAVA_HOME")
        .map(PathBuf::from)
        .filter(|home| java_home_major(home).is_some_and(|major| major >= 17))
        .or_else(|| {
            std::env::var_os("JAVA_HOME")
                .map(PathBuf::from)
                .filter(|home| java_home_major(home).is_some_and(|major| major >= 17))
        })
        .or_else(|| {
            common_jdk_homes()
                .into_iter()
                .find(|home| java_home_major(home).is_some_and(|major| major >= 17))
        })
}

fn java_home_major(home: &Path) -> Option<u32> {
    let release = fs::read_to_string(home.join("release")).ok()?;
    for line in release.lines() {
        let Some(version) = line.strip_prefix("JAVA_VERSION=\"") else {
            continue;
        };
        let version = version.trim_end_matches('"');
        let mut parts = version.split(['.', '_']);
        let first = parts.next()?.parse::<u32>().ok()?;
        if first == 1 {
            return parts.next()?.parse().ok();
        }
        return Some(first);
    }
    None
}

fn common_jdk_homes() -> Vec<PathBuf> {
    let mut homes = Vec::new();
    let mut parents = Vec::new();
    if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
        parents.push(PathBuf::from(&home).join(".jdks"));
    }
    if cfg!(windows) {
        parents.push(PathBuf::from(r"C:\Program Files\Java"));
        parents.push(PathBuf::from(r"C:\Program Files\Eclipse Adoptium"));
        parents.push(PathBuf::from(r"C:\Program Files\Microsoft"));
    } else {
        parents.push(PathBuf::from("/usr/lib/jvm"));
        parents.push(PathBuf::from("/Library/Java/JavaVirtualMachines"));
    }

    for parent in parents {
        let Ok(entries) = fs::read_dir(parent) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.join("bin").join(java_exe("java")).is_file() {
                homes.push(path.clone());
            }
            let bundled = path.join("Contents").join("Home");
            if bundled.join("bin").join(java_exe("java")).is_file() {
                homes.push(bundled);
            }
        }
    }
    homes
}

fn path_with_java_home(java_home: &Path) -> Option<std::ffi::OsString> {
    let mut paths = vec![java_home.join("bin")];
    if let Some(existing) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&existing));
    }
    std::env::join_paths(paths).ok()
}

fn java_exe(name: &str) -> std::ffi::OsString {
    if cfg!(windows) {
        format!("{name}.exe").into()
    } else {
        name.into()
    }
}

fn terminate_process(pid: u32) {
    let mut command = if cfg!(windows) {
        let mut command = Command::new("taskkill");
        command.arg("/PID").arg(pid.to_string()).arg("/F");
        command
    } else {
        let mut command = Command::new("kill");
        command.arg("-9").arg(pid.to_string());
        command
    };
    command.stdout(Stdio::null()).stderr(Stdio::null());
    hide_console(&mut command);
    let status = command.status().expect("spawn process terminator");
    assert!(
        status.success(),
        "failed to terminate process {pid}; status={status}"
    );
}

fn free_loopback_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind free port");
    listener.local_addr().unwrap().port()
}

struct TargetJvm {
    child: Child,
    port: u16,
    _output: mpsc::Receiver<String>,
}

impl Drop for TargetJvm {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start_jdwp_fixture(main_class: &str) -> TargetJvm {
    start_jdwp_target(fixture_dir(), main_class)
}

fn start_jdwp_target(classpath: PathBuf, main_class: &str) -> TargetJvm {
    let port = free_loopback_port();
    let mut command = Command::new(java_path(false));
    command
        .arg(format!(
            "-agentlib:jdwp=transport=dt_socket,server=y,suspend=y,address={port}"
        ))
        .arg("-cp")
        .arg(classpath)
        .arg(main_class)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    hide_console(&mut command);
    let mut child = command.spawn().expect("failed to spawn JDWP fixture JVM");
    let (tx, rx) = mpsc::channel();
    if let Some(stdout) = child.stdout.take() {
        spawn_output_reader(stdout, tx.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        spawn_output_reader(stderr, tx);
    }
    wait_for_jdwp_banner(&mut child, &rx, port);
    TargetJvm {
        child,
        port,
        _output: rx,
    }
}

fn temp_test_dir(label: &str) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join(format!(
            "jdbg-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time after epoch")
                .as_millis()
        ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).expect("create temp test dir");
    path
}

fn source_line(source: &str, needle: &str) -> u32 {
    source
        .lines()
        .position(|line| line.contains(needle))
        .map(|index| index as u32 + 1)
        .unwrap_or_else(|| panic!("source did not contain {needle:?}"))
}

struct TestDaemonGuard {
    username: String,
    data_dir: PathBuf,
}

impl TestDaemonGuard {
    fn new(label: &str) -> Self {
        let unique = format!(
            "jdbg-test-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time after epoch")
                .as_millis()
        );
        let data_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("jdbg-test-data-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&data_dir);
        fs::create_dir_all(&data_dir).expect("create isolated jdbg test data dir");
        Self {
            username: unique,
            data_dir,
        }
    }
}

impl Drop for TestDaemonGuard {
    fn drop(&mut self) {
        let mut command = Command::new(env!("CARGO_BIN_EXE_jdbg"));
        command
            .arg("daemon")
            .arg("stop")
            .env("USERNAME", &self.username)
            .env("USER", &self.username)
            .env("JDBG_DATA_DIR", &self.data_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        hide_console(&mut command);
        let _ = command.status();
        let _ = fs::remove_dir_all(&self.data_dir);
    }
}

fn run_mcp_jsonrpc(messages: &[Value], guard: &TestDaemonGuard) -> Vec<Value> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_jdbg"));
    command
        .arg("__mcp")
        .env("USERNAME", &guard.username)
        .env("USER", &guard.username)
        .env("JDBG_DATA_DIR", &guard.data_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    hide_console(&mut command);
    let mut child = command.spawn().expect("failed to spawn jdbg __mcp");

    let mut stdin = child.stdin.take().expect("mcp stdin");
    let stdout = child.stdout.take().expect("mcp stdout");
    let mut stdout = BufReader::new(stdout);
    let stderr = child.stderr.take().expect("mcp stderr");
    let stderr_reader = std::thread::spawn(move || {
        let mut text = String::new();
        let mut reader = BufReader::new(stderr);
        let _ = reader.read_to_string(&mut text);
        text
    });

    let mut responses = Vec::new();
    for msg in messages {
        writeln!(stdin, "{}", serde_json::to_string(msg).unwrap()).expect("write mcp request");
        stdin.flush().expect("flush mcp request");
        if msg.get("id").is_some() {
            let mut line = String::new();
            stdout.read_line(&mut line).expect("read mcp response");
            assert!(!line.trim().is_empty(), "MCP closed stdout before response");
            responses.push(
                serde_json::from_str(&line)
                    .unwrap_or_else(|e| panic!("invalid MCP JSON line {line:?}: {e}")),
            );
        }
    }
    drop(stdin);

    let status = child.wait().expect("wait for mcp process");
    let stderr = stderr_reader.join().expect("join stderr reader");
    assert!(
        status.success(),
        "MCP process failed: status={status}; responses={responses:#?}; stderr={stderr}"
    );

    responses
}

#[test]
fn daemon_stop_exits_gracefully_after_response() {
    let guard = TestDaemonGuard::new("daemon-stop");

    let start = jdbg_command(&guard)
        .arg("daemon")
        .arg("start")
        .output()
        .expect("spawn daemon start");
    assert!(
        start.status.success(),
        "daemon start failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&start.stdout),
        String::from_utf8_lossy(&start.stderr)
    );

    let status = jdbg_command(&guard)
        .arg("daemon")
        .arg("status")
        .output()
        .expect("spawn daemon status");
    assert!(
        status.status.success(),
        "daemon status failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&status.stderr)
    );
    assert!(
        String::from_utf8_lossy(&status.stdout).contains("running"),
        "daemon status should report running, got stdout:\n{}",
        String::from_utf8_lossy(&status.stdout)
    );
    let daemon_pid = parse_daemon_pid(&String::from_utf8_lossy(&status.stdout));

    let stop = jdbg_command(&guard)
        .arg("daemon")
        .arg("stop")
        .output()
        .expect("spawn daemon stop");
    assert!(
        stop.status.success(),
        "daemon stop failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&stop.stdout),
        String::from_utf8_lossy(&stop.stderr)
    );
    assert!(
        String::from_utf8_lossy(&stop.stdout).contains("Daemon stopped."),
        "daemon stop should print confirmation, got stdout:\n{}",
        String::from_utf8_lossy(&stop.stdout)
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    while process_alive(daemon_pid) {
        assert!(
            Instant::now() < deadline,
            "daemon pid {daemon_pid} still running after stop response"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn parse_daemon_pid(output: &str) -> u32 {
    let pid = output
        .split_whitespace()
        .find_map(|part| part.strip_prefix("pid="))
        .expect("daemon status should include pid=<pid>");
    pid.parse().expect("daemon pid should be numeric")
}

#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    use std::ffi::c_void;

    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const STILL_ACTIVE: u32 = 259;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenProcess(dwDesiredAccess: u32, bInheritHandle: i32, dwProcessId: u32) -> *mut c_void;
        fn GetExitCodeProcess(hProcess: *mut c_void, lpExitCode: *mut u32) -> i32;
        fn CloseHandle(hObject: *mut c_void) -> i32;
    }

    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return false;
    }
    let mut code = 0;
    let ok = unsafe { GetExitCodeProcess(handle, &mut code) };
    unsafe {
        CloseHandle(handle);
    }
    ok != 0 && code == STILL_ACTIVE
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn jdbg_command(guard: &TestDaemonGuard) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_jdbg"));
    command
        .env("USERNAME", &guard.username)
        .env("USER", &guard.username)
        .env("JDBG_DATA_DIR", &guard.data_dir)
        .stdin(Stdio::null());
    hide_console(&mut command);
    command
}

fn mcp_response<'a>(responses: &'a [Value], id: i64) -> &'a Value {
    responses
        .iter()
        .find(|resp| resp.get("id").and_then(Value::as_i64) == Some(id))
        .unwrap_or_else(|| panic!("missing MCP response id {id}; responses={responses:#?}"))
}

fn mcp_text(resp: &Value) -> &str {
    resp.pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("missing MCP text content in {resp:#?}"))
}

fn assert_mcp_success(resp: &Value) {
    assert!(
        resp.get("error").is_none(),
        "MCP protocol error response: {resp:#?}"
    );
    assert_eq!(
        resp.pointer("/result/isError").and_then(Value::as_bool),
        Some(false),
        "MCP tool returned error: {resp:#?}"
    );
}

fn spawn_output_reader<R>(reader: R, tx: mpsc::Sender<String>)
where
    R: Read + Send + 'static,
{
    std::thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => return,
                Ok(_) => {
                    let _ = tx.send(line.trim_end().to_string());
                }
                Err(_) => return,
            }
        }
    });
}

fn wait_for_jdwp_banner(child: &mut Child, rx: &mpsc::Receiver<String>, port: u16) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let expected = format!("address: {port}");
    let mut output = Vec::new();
    loop {
        match rx.recv_timeout(Duration::from_millis(25)) {
            Ok(line) => {
                if line.contains("Listening for transport") && line.contains(&expected) {
                    return;
                }
                output.push(line);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("target JVM output closed before JDWP banner; output={output:?}");
            }
        }
        if let Some(status) = child.try_wait().expect("check target JVM") {
            panic!("target JVM exited before JDWP banner: {status}; output={output:?}");
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for JDWP fixture banner containing {expected}; output={output:?}"
        );
    }
}

/// Helper: launch one fixture session.
fn launch_fixture(main_class: &str) -> Arc<Session> {
    use java_agent_debugger::jdb::process::LaunchConfig;

    let source_name = format!(
        "{}.java",
        main_class.rsplit('.').next().unwrap_or(main_class)
    );
    compile_java_fixture(&source_name);

    let dir = fixture_dir();
    let config = LaunchConfig {
        main_class: main_class.to_string(),
        classpath: vec![dir.clone()],
        sourcepath: vec![dir],
        app_args: vec![],
        jdb_args: vec![],
    };
    let session = Session::launch(&jdb_path(), &config, "test-session".into(), None)
        .expect("failed to launch jdb session");
    Arc::new(session)
}

// ─── Phase 1: Tool descriptions are implicitly tested by existing spec tests ───

// ─── Phase 2: break_target recording and line mismatch ───

#[test]
fn break_target_record_and_take() {
    let session = launch_fixture("CollectionTest");

    // Initial state: no target.
    assert!(session.take_break_target().is_none());

    // Record.
    session.record_break_target("CollectionTest", 9);
    let target = session.take_break_target();
    assert_eq!(target, Some(("CollectionTest".to_string(), 9)));

    // take is one-shot.
    assert!(session.take_break_target().is_none());

    // Cleanup.
    let _ = session.kill();
}

// ─── Phase 3: enrich_stopped — breakpoint hit returns source_context + frame ───

#[test]
fn breakpoint_hit_includes_source_context_and_frame() {
    let session = launch_fixture("CollectionTest");

    // Set breakpoint: line 10 (`int size = fruits.size();`).
    let bp_resp = session
        .stop_at("CollectionTest", 10, None, None)
        .expect("stop_at failed");
    assert!(
        matches!(bp_resp.result, CommandResult::BreakpointSet { .. }),
        "expected BreakpointSet, got {:?}",
        bp_resp.result
    );

    // Run.
    let run_resp = session.run(Some(30)).expect("run failed");

    // run returns raw Stopped because the session layer does not enrich; enrichment lives in the handler layer.
    // Manually simulate the handler enrichment logic here.
    let mut resp = run_resp;
    enrich_stopped_test_helper(&session, &mut resp);

    match &resp.result {
        CommandResult::Stopped {
            location,
            frame,
            source_context,
            ..
        } => {
            assert_eq!(location.class, "CollectionTest");
            assert_eq!(location.method, "main");
            assert!(location.line > 0, "line should be nonzero");

            // frame should be filled.
            assert!(frame.is_some(), "frame should be enriched, got None");
            let f = frame.as_ref().unwrap();
            assert_eq!(f.index, 1); // jdb uses 1-based frame indices.
            assert_eq!(f.location.class, "CollectionTest");

            // source_context should be filled when sourcepath is correct.
            assert!(
                source_context.is_some(),
                "source_context should be enriched, got None"
            );
            let lines = source_context.as_ref().unwrap();
            assert!(!lines.is_empty(), "source lines should not be empty");
            // Should include the breakpoint line.
            assert!(
                lines.iter().any(|l| l.number == location.line),
                "source_context should contain the breakpoint line {}",
                location.line
            );
        }
        other => panic!("expected Stopped, got {other:?}"),
    }

    let _ = session.kill();
}

// ─── Phase 2+3: line mismatch check ───

#[test]
fn line_mismatch_note_when_hit_differs() {
    let session = launch_fixture("CollectionTest");

    // Set breakpoint and record target.
    session
        .stop_at("CollectionTest", 10, None, None)
        .expect("stop_at failed");
    session.record_break_target("CollectionTest", 10);

    let run_resp = session.run(Some(30)).expect("run failed");
    let mut resp = run_resp;

    // Simulate check_line_mismatch.
    check_line_mismatch_test_helper(&session, &mut resp);

    // If the actual hit line equals the requested line, note should be None.
    if let CommandResult::Stopped { location, .. } = &resp.result {
        if location.line == 10 {
            // Line numbers match; no note expected.
            assert!(
                resp.note.is_none(),
                "no mismatch note expected when lines match"
            );
        } else {
            // Line numbers differ; note expected.
            assert!(
                resp.note.is_some(),
                "mismatch note expected when lines differ"
            );
            let note = resp.note.as_ref().unwrap();
            assert!(note.contains("requested at line 10"));
            assert!(note.contains(&format!("hit at line {}", location.line)));
        }
    }

    let _ = session.kill();
}

// ─── Phase 4: inspect tool ───

#[test]
fn inspect_collection_returns_elements() {
    let session = launch_fixture("CollectionTest");

    // Break after the collection has been populated (line 10: int size = fruits.size();).
    session
        .stop_at("CollectionTest", 10, None, None)
        .expect("stop_at failed");
    let run_resp = session.run(Some(30)).expect("run failed");
    assert!(
        matches!(run_resp.result, CommandResult::Stopped { .. }),
        "expected Stopped, got {:?}",
        run_resp.result
    );

    // Call inspect.
    let inspect_resp = handle_inspect_test_helper(&session, "fruits", 10, None);
    match &inspect_resp.result {
        CommandResult::Inspection {
            expr,
            size,
            elements,
            truncated,
        } => {
            assert_eq!(expr, "fruits");
            assert_eq!(*size, Some(3), "fruits.size() should be 3");
            assert_eq!(elements.len(), 3, "should have 3 elements");
            // Verify element contents.
            assert!(
                elements[0].value.contains("apple"),
                "first element should be apple, got: {}",
                elements[0].value
            );
            assert!(
                elements[1].value.contains("banana"),
                "second element should be banana, got: {}",
                elements[1].value
            );
            assert!(
                elements[2].value.contains("cherry"),
                "third element should be cherry, got: {}",
                elements[2].value
            );
            assert_eq!(*truncated, Some(false), "should not be truncated");
        }
        other => panic!("expected Inspection, got {other:?}"),
    }

    let _ = session.kill();
}

#[test]
fn inspect_with_max_elements_truncates() {
    let session = launch_fixture("CollectionTest");

    session
        .stop_at("CollectionTest", 10, None, None)
        .expect("stop_at failed");
    let run_resp = session.run(Some(30)).expect("run failed");
    assert!(matches!(run_resp.result, CommandResult::Stopped { .. }));

    // Fetch only 2 elements.
    let inspect_resp = handle_inspect_test_helper(&session, "fruits", 2, None);
    match &inspect_resp.result {
        CommandResult::Inspection {
            size,
            elements,
            truncated,
            ..
        } => {
            assert_eq!(*size, Some(3));
            assert_eq!(elements.len(), 2, "should only have 2 elements with max=2");
            assert_eq!(*truncated, Some(true), "should be marked truncated");
        }
        other => panic!("expected Inspection, got {other:?}"),
    }

    let _ = session.kill();
}

#[test]
fn inspect_empty_returns_size_zero() {
    let session = launch_fixture("CollectionTest");

    // Break at line 7, where fruits has just been created and is still empty.
    // line 6: List<String> fruits = new ArrayList<>();
    // line 7: fruits.add("apple");
    session
        .stop_at("CollectionTest", 7, None, None)
        .expect("stop_at failed");
    let run_resp = session.run(Some(30)).expect("run failed");
    assert!(matches!(run_resp.result, CommandResult::Stopped { .. }));

    let inspect_resp = handle_inspect_test_helper(&session, "fruits", 10, None);
    match &inspect_resp.result {
        CommandResult::Inspection { size, elements, .. } => {
            assert_eq!(*size, Some(0));
            assert!(elements.is_empty());
        }
        other => panic!("expected Inspection, got {other:?}"),
    }

    let _ = session.kill();
}

// ─── Phase: conditional breakpoint ───

#[test]
fn jdi_inspect_renders_structured_value_kinds() {
    use java_agent_debugger::jdi::session::JdiSession;

    let _guard = jdi_e2e_guard();
    compile_java_fixture("StructuredInspectTest.java");
    let target = start_jdwp_fixture("StructuredInspectTest");
    let sourcepath = vec![fixture_dir().display().to_string()];
    let session = JdiSession::attach(
        "127.0.0.1",
        target.port,
        &sourcepath,
        "jdi-structured-inspect".into(),
        None,
    )
    .expect("JDI attach failed");

    let inspect_line = fixture_line(
        "StructuredInspectTest.java",
        "System.out.println(root.name);",
    );
    let breakpoint = session
        .stop_at("StructuredInspectTest", inspect_line, None)
        .expect("JDI structured breakpoint failed");
    assert!(
        matches!(
            breakpoint.result,
            CommandResult::BreakpointSet { deferred: true, .. }
        ),
        "expected deferred JDI structured breakpoint, got {:?}",
        breakpoint.result
    );
    let stop = session.cont(Some(30)).expect("JDI cont failed");
    assert!(
        matches!(
            stop.result,
            CommandResult::Stopped {
                event: Event::Breakpoint { .. },
                ..
            }
        ),
        "expected JDI structured breakpoint stop, got {:?}",
        stop.result
    );

    let inspect = session.inspect("root", 2).expect("JDI inspect failed");
    let text = match inspect.result {
        CommandResult::Raw { text } => text,
        other => panic!("expected raw JSON inspect result, got {other:?}"),
    };

    assert!(text.contains(r#""kind": "object""#), "{text}");
    assert!(text.contains(r#""kind": "collection""#), "{text}");
    assert!(text.contains(r#""kind": "map""#), "{text}");
    assert!(text.contains(r#""kind": "enum""#), "{text}");
    assert!(text.contains(r#""kind": "cycle""#), "{text}");
    assert!(text.contains(r#""truncated": true"#), "{text}");
    assert!(text.contains(r#""name": "ACTIVE""#), "{text}");

    let _ = session.kill();
}

#[test]
fn jdi_inspect_specializes_advanced_collections_and_maps() {
    use java_agent_debugger::jdi::session::JdiSession;

    let _guard = jdi_e2e_guard();
    compile_java_fixture("AdvancedCollectionsTest.java");
    let target = start_jdwp_fixture("AdvancedCollectionsTest");
    let sourcepath = vec![fixture_dir().display().to_string()];
    let session = JdiSession::attach(
        "127.0.0.1",
        target.port,
        &sourcepath,
        "jdi-advanced-collections".into(),
        None,
    )
    .expect("JDI attach failed");

    let inspect_line = fixture_line(
        "AdvancedCollectionsTest.java",
        "System.out.println(holder.linkedList.size());",
    );
    session
        .stop_at("AdvancedCollectionsTest", inspect_line, None)
        .expect("JDI advanced breakpoint failed");
    let stop = session.cont(Some(30)).expect("JDI cont failed");
    assert!(
        matches!(
            stop.result,
            CommandResult::Stopped {
                event: Event::Breakpoint { .. },
                ..
            }
        ),
        "expected JDI advanced breakpoint stop, got {:?}",
        stop.result
    );

    let inspect = session
        .inspect("holder", 10)
        .expect("JDI advanced inspect failed");
    let text = match inspect.result {
        CommandResult::Raw { text } => text,
        other => panic!("expected raw JSON inspect result, got {other:?}"),
    };
    let root: Value = serde_json::from_str(&text).expect("JDI inspect should be valid JSON");

    assert_jdi_collection_field(&root, "linkedList", &["linked-a", "linked-b", "linked-c"]);
    assert_jdi_collection_field(&root, "deque", &["deque-a", "deque-b", "deque-c"]);
    assert_jdi_collection_field(&root, "hashSet", &["set-a", "set-b"]);
    assert_jdi_collection_field(&root, "linkedSet", &["linked-set-a", "linked-set-b"]);
    assert_jdi_map_field(&root, "treeMap", &["one", "two"]);
    assert_jdi_collection_field(&root, "treeSet", &["tree-a", "tree-b"]);
    assert_jdi_collection_field(&root, "unmodifiableList", &["linked-a", "linked-c"]);
    assert_jdi_map_field(&root, "unmodifiableMap", &["one", "two"]);

    let _ = session.kill();
}

#[test]
fn jdi_cont_surfaces_vm_disconnect_and_marks_session_exited() {
    use java_agent_debugger::jdi::session::JdiSession;

    let _guard = jdi_e2e_guard();
    compile_java_fixture("Main.java");
    let target = start_jdwp_fixture("Main");
    let sourcepath = vec![fixture_dir().display().to_string()];
    let session = JdiSession::attach(
        "127.0.0.1",
        target.port,
        &sourcepath,
        "jdi-vm-disconnect".into(),
        None,
    )
    .expect("JDI attach failed");

    let response = session.cont(Some(10)).expect("JDI cont failed");

    match &response.result {
        CommandResult::VmExited { tail, .. } => {
            assert!(
                tail.as_deref()
                    .unwrap_or_default()
                    .contains("target VM disconnected"),
                "vmDisconnected tail should explain the target exit, got {tail:?}"
            );
        }
        other => panic!("expected VmExited from target disconnect, got {other:?}"),
    }
    match session.status() {
        CommandResult::Status {
            state,
            last_event: Some(Event::VmExit),
            ..
        } => assert_eq!(state, RunState::Exited),
        other => panic!("expected exited JDI status with VmExit last_event, got {other:?}"),
    }

    let _ = session.kill();
}

#[test]
fn jdi_kill_detaches_and_marks_session_dead() {
    use java_agent_debugger::jdi::session::JdiSession;

    let _guard = jdi_e2e_guard();
    compile_java_fixture("Loop.java");
    let target = start_jdwp_fixture("Loop");
    let sourcepath = vec![fixture_dir().display().to_string()];
    let session = JdiSession::attach(
        "127.0.0.1",
        target.port,
        &sourcepath,
        "jdi-detach".into(),
        None,
    )
    .expect("JDI attach failed");

    session.kill().expect("JDI kill/detach failed");

    match session.status() {
        CommandResult::Status {
            state, jdb_alive, ..
        } => {
            assert_eq!(state, RunState::Dead);
            assert!(!jdb_alive, "sidecar process should be stopped after kill");
        }
        other => panic!("expected dead JDI status after kill, got {other:?}"),
    }
}

#[test]
fn jdi_status_marks_dead_when_sidecar_process_exits_unexpectedly() {
    use java_agent_debugger::jdi::session::JdiSession;

    let _guard = jdi_e2e_guard();
    compile_java_fixture("Loop.java");
    let target = start_jdwp_fixture("Loop");
    let sourcepath = vec![fixture_dir().display().to_string()];
    let session = JdiSession::attach(
        "127.0.0.1",
        target.port,
        &sourcepath,
        "jdi-sidecar-crash".into(),
        None,
    )
    .expect("JDI attach failed");

    terminate_process(session.meta.sidecar_pid);

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match session.status() {
            CommandResult::Status {
                state: RunState::Dead,
                jdb_alive,
                ..
            } => {
                assert!(!jdb_alive, "sidecar process should not be alive");
                break;
            }
            other if Instant::now() < deadline => {
                assert!(
                    !matches!(
                        other,
                        CommandResult::Status {
                            state: RunState::Exited,
                            ..
                        }
                    ),
                    "unexpected exited status after sidecar process death: {other:?}"
                );
                std::thread::yield_now();
            }
            other => panic!("expected dead JDI status after sidecar exit, got {other:?}"),
        }
    }
}

#[test]
fn jdi_step_over_returns_step_event_with_stack_and_locals() {
    use java_agent_debugger::jdi::session::JdiSession;

    let _guard = jdi_e2e_guard();
    compile_java_fixture("StructuredInspectTest.java");
    let target = start_jdwp_fixture("StructuredInspectTest");
    let sourcepath = vec![fixture_dir().display().to_string()];
    let session = JdiSession::attach(
        "127.0.0.1",
        target.port,
        &sourcepath,
        "jdi-step-over".into(),
        None,
    )
    .expect("JDI attach failed");

    let inspect_line = fixture_line(
        "StructuredInspectTest.java",
        "System.out.println(root.name);",
    );
    session
        .stop_at("StructuredInspectTest", inspect_line, None)
        .expect("JDI step breakpoint failed");
    let stop = session.cont(Some(30)).expect("JDI cont failed");
    assert!(
        matches!(
            stop.result,
            CommandResult::Stopped {
                event: Event::Breakpoint { .. },
                ..
            }
        ),
        "expected step breakpoint stop, got {:?}",
        stop.result
    );

    let step = session.next(Some(30)).expect("JDI step-over failed");
    match &step.result {
        CommandResult::Stopped {
            event: Event::Step { .. },
            location,
            frame: Some(frame),
            ..
        } => {
            assert_eq!(location.class, "StructuredInspectTest");
            assert_eq!(location.method, "main");
            assert_eq!(frame.location.class, "StructuredInspectTest");
        }
        other => panic!("expected JDI step Stopped with top frame, got {other:?}"),
    }

    let locals = session.locals().expect("JDI locals after step failed");
    match &locals.result {
        CommandResult::Locals { vars } => {
            assert!(
                vars.iter().any(|v| v.name == "root"),
                "locals should include root after step-over, got {vars:?}"
            );
        }
        other => panic!("expected locals after JDI step, got {other:?}"),
    }

    let _ = session.kill();
}

#[test]
fn jdi_command_surface_supports_metadata_source_raw_and_array_length() {
    use java_agent_debugger::jdi::session::JdiSession;

    let _guard = jdi_e2e_guard();
    compile_java_fixture("JdiLaunchTest.java");
    let classpath = vec![fixture_dir().display().to_string()];
    let sourcepath = vec![fixture_dir().display().to_string()];
    let session = JdiSession::launch(
        "JdiLaunchTest",
        &classpath,
        &sourcepath,
        &[],
        "jdi-command-surface-metadata".into(),
        None,
    )
    .expect("JDI launch failed");

    let line = fixture_line("JdiLaunchTest.java", "System.out.println(label");
    session
        .stop_at("JdiLaunchTest", line, None)
        .expect("JDI launch breakpoint failed");

    let breakpoints = session.breakpoints().expect("JDI breakpoints failed");
    match &breakpoints.result {
        CommandResult::BreakpointList { breakpoints } => assert!(
            breakpoints
                .iter()
                .any(|bp| bp.contains(&format!("JdiLaunchTest:{line}"))),
            "breakpoints should include deferred line breakpoint, got {breakpoints:?}"
        ),
        other => panic!("expected JDI BreakpointList, got {other:?}"),
    }

    let clear = session
        .clear(&format!("JdiLaunchTest:{line}"))
        .expect("JDI clear failed");
    assert!(
        matches!(clear.result, CommandResult::Raw { ref text } if text.contains("Removed 1")),
        "expected clear raw removal, got {:?}",
        clear.result
    );
    session
        .stop_at("JdiLaunchTest", line, None)
        .expect("JDI launch breakpoint reset failed");

    let stop = session.run(Some(30)).expect("JDI launch run failed");
    assert!(
        matches!(
            stop.result,
            CommandResult::Stopped {
                event: Event::Breakpoint { .. },
                ..
            }
        ),
        "expected JDI breakpoint stop, got {:?}",
        stop.result
    );

    let classes = session
        .classes(Some("JdiLaunchTest"))
        .expect("JDI classes failed");
    match &classes.result {
        CommandResult::Classes { classes } => assert!(
            classes.iter().any(|class| class == "JdiLaunchTest"),
            "classes should include JdiLaunchTest, got {classes:?}"
        ),
        other => panic!("expected JDI Classes, got {other:?}"),
    }

    let methods = session
        .methods("JdiLaunchTest")
        .expect("JDI methods failed");
    match &methods.result {
        CommandResult::Methods { methods, .. } => assert!(
            methods.iter().any(|method| method.contains(".main(")),
            "methods should include main, got {methods:?}"
        ),
        other => panic!("expected JDI Methods, got {other:?}"),
    }

    let source = session
        .list_source(Some(line))
        .expect("JDI list_source failed");
    match &source.result {
        CommandResult::Source { lines, .. } => assert!(
            lines
                .iter()
                .any(|source_line| source_line.text.contains("System.out.println(label")),
            "source should include println line, got {lines:?}"
        ),
        other => panic!("expected JDI Source, got {other:?}"),
    }

    let args_len = session
        .evaluate("args.length")
        .expect("JDI args.length eval failed");
    match &args_len.result {
        CommandResult::Value { value, ty, .. } => {
            assert_eq!(value, "0");
            assert_eq!(ty.as_deref(), Some("int"));
        }
        other => panic!("expected JDI Value for args.length, got {other:?}"),
    }

    let raw = session
        .raw("classes JdiLaunchTest", Some(5))
        .expect("JDI raw classes failed");
    assert!(
        matches!(raw.result, CommandResult::Classes { .. }),
        "raw classes should dispatch to JDI classes, got {:?}",
        raw.result
    );

    let _ = session.kill();
}

#[test]
fn jdi_attach_list_source_infers_maven_source_root_from_target_classpath() {
    use java_agent_debugger::jdi::session::JdiSession;

    let _guard = jdi_e2e_guard();
    let root = temp_test_dir("maven-sourcepath");
    let module = root.join("mall-portal");
    let source_dir = module
        .join("src")
        .join("main")
        .join("java")
        .join("com")
        .join("example")
        .join("web");
    let classes_dir = module.join("target").join("classes");
    fs::create_dir_all(&source_dir).expect("create source dir");
    fs::create_dir_all(&classes_dir).expect("create classes dir");
    let source = r#"package com.example.web;

public class MavenSourcePathTest {
    public static void main(String[] args) throws Exception {
        new MavenSourcePathTest().serve();
        Thread.sleep(300000);
    }

    void serve() {
        int marker = 7; // MAVEN_SOURCE_BREAKPOINT
        System.out.println("marker=" + marker);
    }
}
"#;
    let source_file = source_dir.join("MavenSourcePathTest.java");
    fs::write(&source_file, source).expect("write MavenSourcePathTest.java");
    let line = source_line(source, "MAVEN_SOURCE_BREAKPOINT");

    let _javac_guard = JAVAC_LOCK.lock().expect("javac fixture lock poisoned");
    let mut javac = Command::new(javac_path());
    javac
        .arg("-g")
        .arg("-Xlint:-options")
        .arg("-source")
        .arg("8")
        .arg("-target")
        .arg("8")
        .arg("-d")
        .arg(&classes_dir)
        .arg(&source_file);
    hide_console(&mut javac);
    let status = javac
        .status()
        .expect("spawn javac for Maven sourcepath test");
    assert!(status.success(), "javac failed with status {status}");
    drop(_javac_guard);

    let target = start_jdwp_target(classes_dir.clone(), "com.example.web.MavenSourcePathTest");
    let session = JdiSession::attach(
        "127.0.0.1",
        target.port,
        &[],
        "jdi-maven-sourcepath".into(),
        None,
    )
    .expect("JDI attach failed");
    session
        .stop_at("com.example.web.MavenSourcePathTest", line, None)
        .expect("JDI Maven source breakpoint failed");
    let stop = session
        .cont(Some(30))
        .expect("JDI cont to Maven source breakpoint failed");
    match &stop.result {
        CommandResult::Stopped {
            event: Event::Breakpoint { location, .. },
            ..
        } => {
            assert_eq!(location.class, "com.example.web.MavenSourcePathTest");
            assert_eq!(location.method, "serve");
            assert_eq!(location.line, line);
        }
        other => panic!("expected Maven source breakpoint stop, got {other:?}"),
    }

    let source_response = session
        .list_source(Some(line))
        .expect("JDI list_source should infer Maven source root");
    match &source_response.result {
        CommandResult::Source { lines, .. } => assert!(
            lines
                .iter()
                .any(|source_line| source_line.text.contains("MAVEN_SOURCE_BREAKPOINT")),
            "source should include Maven breakpoint line, got {lines:?}"
        ),
        other => panic!("expected JDI Source, got {other:?}"),
    }

    let _ = session.kill();
    drop(target);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn jdi_frame_step_out_suspend_and_lock_commands_work() {
    use java_agent_debugger::jdi::session::JdiSession;

    let _guard = jdi_e2e_guard();
    compile_java_fixture("EvalMutationTest.java");
    let target = start_jdwp_fixture("EvalMutationTest");
    let sourcepath = vec![fixture_dir().display().to_string()];
    let session = JdiSession::attach(
        "127.0.0.1",
        target.port,
        &sourcepath,
        "jdi-command-surface-runtime".into(),
        None,
    )
    .expect("JDI attach failed");

    let compute_line = fixture_line("EvalMutationTest.java", "int before = box.add(values[1]);");
    session
        .stop_at("EvalMutationTest", compute_line, None)
        .expect("JDI compute breakpoint failed");
    let stop = session.cont(Some(30)).expect("JDI cont to compute failed");
    assert!(
        matches!(
            stop.result,
            CommandResult::Stopped {
                event: Event::Breakpoint { .. },
                ..
            }
        ),
        "expected JDI breakpoint stop, got {:?}",
        stop.result
    );

    let frame_up = session.frame("up", 1).expect("JDI frame up failed");
    assert!(
        matches!(frame_up.result, CommandResult::Raw { ref text } if text.contains(".main")),
        "frame up should select main, got {:?}",
        frame_up.result
    );
    let frame_down = session.frame("down", 1).expect("JDI frame down failed");
    assert!(
        matches!(frame_down.result, CommandResult::Raw { ref text } if text.contains(".compute")),
        "frame down should return to compute, got {:?}",
        frame_down.result
    );

    let locks = session.threadlocks(None).expect("JDI threadlocks failed");
    assert!(
        matches!(locks.result, CommandResult::Raw { ref text } if text.contains("locks")),
        "threadlocks should return raw lock info, got {:?}",
        locks.result
    );
    let lock = session.lock("box").expect("JDI lock failed");
    assert!(
        matches!(lock.result, CommandResult::Raw { ref text } if text.contains("owner")),
        "lock should return monitor owner info, got {:?}",
        lock.result
    );

    let threads = session.threads(None).expect("JDI threads failed");
    let thread_id = match &threads.result {
        CommandResult::Threads { threads } => threads
            .iter()
            .find(|thread| thread.name == "main")
            .map(|thread| thread.id.clone())
            .expect("main thread should be present"),
        other => panic!("expected JDI Threads, got {other:?}"),
    };
    session
        .suspend(Some(&thread_id))
        .expect("JDI suspend thread failed");
    session
        .resume(Some(&thread_id))
        .expect("JDI resume thread failed");

    let step = session.step(Some(30)).expect("JDI step failed");
    match &step.result {
        CommandResult::Stopped {
            event: Event::Step { .. },
            location,
            ..
        } => {
            assert_eq!(location.class, "EvalMutationTest$Box");
            assert_eq!(location.method, "add");
        }
        other => panic!("expected JDI step into Box.add, got {other:?}"),
    }

    let step_out = session.step_out(Some(30)).expect("JDI step_out failed");
    assert!(
        matches!(
            step_out.result,
            CommandResult::Stopped {
                event: Event::Step { .. },
                ..
            }
        ),
        "expected JDI step_out stop, got {:?}",
        step_out.result
    );

    let _ = session.kill();
}

#[test]
fn jdi_catch_and_ignore_exception_commands_work() {
    use java_agent_debugger::jdi::session::JdiSession;

    let _guard = jdi_e2e_guard();
    compile_java_fixture("Throw.java");

    let sourcepath = vec![fixture_dir().display().to_string()];
    let target = start_jdwp_fixture("Throw");
    let session = JdiSession::attach(
        "127.0.0.1",
        target.port,
        &sourcepath,
        "jdi-catch-exception".into(),
        None,
    )
    .expect("JDI attach failed");
    let catch = session
        .catch_exception("java.lang.NullPointerException", "all")
        .expect("JDI catch failed");
    assert!(
        matches!(
            catch.result,
            CommandResult::BreakpointSet {
                bp_kind: BreakpointKind::Catch,
                ..
            }
        ),
        "expected catch breakpoint set, got {:?}",
        catch.result
    );
    let caught = session
        .cont(Some(30))
        .expect("JDI cont to exception failed");
    match &caught.result {
        CommandResult::ExceptionCaught {
            exception, caught, ..
        } => {
            assert_eq!(exception, "java.lang.NullPointerException");
            assert!(!caught, "Throw fixture exception should be uncaught");
        }
        other => panic!("expected JDI ExceptionCaught, got {other:?}"),
    }
    let _ = session.kill();

    let target = start_jdwp_fixture("Throw");
    let session = JdiSession::attach(
        "127.0.0.1",
        target.port,
        &sourcepath,
        "jdi-ignore-exception".into(),
        None,
    )
    .expect("JDI attach failed");
    session
        .catch_exception("java.lang.NullPointerException", "all")
        .expect("JDI catch before ignore failed");
    let ignored = session
        .ignore_exception("java.lang.NullPointerException", "all")
        .expect("JDI ignore failed");
    assert!(
        matches!(ignored.result, CommandResult::Raw { ref text } if text.contains("Ignored 1")),
        "expected ignore raw removal, got {:?}",
        ignored.result
    );
    let exited = session
        .cont(Some(30))
        .expect("JDI cont after ignore failed");
    assert!(
        matches!(exited.result, CommandResult::VmExited { .. }),
        "expected VM exit after ignore, got {:?}",
        exited.result
    );
    let _ = session.kill();
}

#[test]
fn jdi_eval_set_and_force_return_execute_in_stopped_frame() {
    use java_agent_debugger::jdi::session::JdiSession;

    let _guard = jdi_e2e_guard();
    compile_java_fixture("EvalMutationTest.java");
    let target = start_jdwp_fixture("EvalMutationTest");
    let sourcepath = vec![fixture_dir().display().to_string()];
    let session = JdiSession::attach(
        "127.0.0.1",
        target.port,
        &sourcepath,
        "jdi-eval-set-force-return".into(),
        None,
    )
    .expect("JDI attach failed");

    let compute_line = fixture_line("EvalMutationTest.java", "int before = box.add(values[1]);");
    session
        .stop_at("EvalMutationTest", compute_line, None)
        .expect("JDI compute breakpoint failed");
    let stop = session.cont(Some(30)).expect("JDI cont to compute failed");
    match &stop.result {
        CommandResult::Stopped {
            event: Event::Breakpoint { .. },
            location,
            ..
        } => {
            assert_eq!(location.class, "EvalMutationTest");
            assert_eq!(location.method, "compute");
        }
        other => panic!("expected compute breakpoint stop, got {other:?}"),
    }

    let evaluated = session
        .evaluate("box.add(values[1]) + local + EvalMutationTest.staticAdd(1, 2)")
        .expect("JDI expression eval failed");
    match &evaluated.result {
        CommandResult::Value { expr, value, .. } => {
            assert_eq!(
                expr,
                "box.add(values[1]) + local + EvalMutationTest.staticAdd(1, 2)"
            );
            assert_eq!(value, "15");
        }
        other => panic!("expected JDI Value result, got {other:?}"),
    }

    session
        .set_value("box.count", "10")
        .expect("JDI field set failed");
    session
        .set_value("values[1]", "5")
        .expect("JDI array set failed");
    let post_return_line = fixture_line("EvalMutationTest.java", "System.out.println(\"result=\"");
    session
        .stop_at("EvalMutationTest", post_return_line, None)
        .expect("JDI post-return breakpoint failed");
    session
        .force_return("123")
        .expect("JDI force return failed");
    let stack_after_force_return = session
        .stack(false)
        .expect("JDI stack after force_return failed");
    match &stack_after_force_return.result {
        CommandResult::StackTrace { frames } => {
            let top = frames.first().expect("stack should include top frame");
            assert_eq!(top.location.class, "EvalMutationTest");
            assert_eq!(top.location.method, "compute");
        }
        other => panic!("expected stack after force_return, got {other:?}"),
    }
    assert!(
        stack_after_force_return
            .note
            .as_deref()
            .is_some_and(|note| note.contains("force_return takes effect")),
        "force_return stack should explain pending frame refresh, got {:?}",
        stack_after_force_return.note
    );
    let post_return = session
        .cont(Some(30))
        .expect("JDI cont after force return failed");
    match &post_return.result {
        CommandResult::Stopped {
            event: Event::Breakpoint { .. },
            location,
            ..
        } => {
            assert_eq!(location.class, "EvalMutationTest");
            assert_eq!(location.method, "main");
        }
        other => panic!("expected post-return breakpoint stop, got {other:?}"),
    }

    for (expr, expected) in [("result", "123"), ("box.count", "10"), ("values[1]", "5")] {
        let value = session.evaluate(expr).expect("JDI post-return eval failed");
        match &value.result {
            CommandResult::Value { value, .. } => assert_eq!(value, expected, "{expr}"),
            other => panic!("expected Value for {expr}, got {other:?}"),
        }
    }

    let _ = session.kill();
}

#[test]
fn jdi_launch_breakpoint_run_locals_and_cont() {
    use java_agent_debugger::jdi::session::JdiSession;

    let _guard = jdi_e2e_guard();
    compile_java_fixture("JdiLaunchTest.java");
    let classpath = vec![fixture_dir().display().to_string()];
    let sourcepath = vec![fixture_dir().display().to_string()];
    let session = JdiSession::launch(
        "JdiLaunchTest",
        &classpath,
        &sourcepath,
        &[],
        "jdi-launch-main".into(),
        None,
    )
    .expect("JDI launch failed");
    assert_eq!(session.state(), RunState::Loaded);

    let line = fixture_line("JdiLaunchTest.java", "System.out.println(label");
    session
        .stop_at("JdiLaunchTest", line, None)
        .expect("JDI launch breakpoint failed");

    let stop = session.run(Some(30)).expect("JDI launch run failed");
    match &stop.result {
        CommandResult::Stopped {
            event: Event::Breakpoint { .. },
            location,
            ..
        } => {
            assert_eq!(location.class, "JdiLaunchTest");
            assert_eq!(location.method, "main");
            assert_eq!(location.line, line);
        }
        other => panic!("expected JDI launch breakpoint stop, got {other:?}"),
    }

    let locals = session.locals().expect("JDI launch locals failed");
    match &locals.result {
        CommandResult::Locals { vars } => {
            assert!(
                vars.iter()
                    .any(|var| var.name == "label" && var.value.contains("hello")),
                "locals should include label=hello, got {vars:?}"
            );
        }
        other => panic!("expected locals after JDI launch stop, got {other:?}"),
    }

    let exited = session.cont(Some(30)).expect("JDI launch cont failed");
    assert!(
        matches!(exited.result, CommandResult::VmExited { .. }),
        "expected JDI launched VM to exit, got {:?}",
        exited.result
    );

    let _ = session.kill();
}

#[test]
fn jdi_async_breakpoint_after_timeout_updates_status_and_resumes_cleanly() {
    use java_agent_debugger::jdi::session::JdiSession;

    let _guard = jdi_e2e_guard();
    compile_java_fixture("AsyncBreakpointTest.java");
    let classpath = vec![fixture_dir().display().to_string()];
    let sourcepath = vec![fixture_dir().display().to_string()];
    let session = JdiSession::launch(
        "AsyncBreakpointTest",
        &classpath,
        &sourcepath,
        &[],
        "jdi-async-breakpoint-timeout".into(),
        None,
    )
    .expect("JDI launch failed");

    let line = fixture_line("AsyncBreakpointTest.java", "ASYNC_BREAKPOINT");
    session
        .stop_at("AsyncBreakpointTest", line, None)
        .expect("JDI async breakpoint failed");

    let timed_out = session
        .run(Some(1))
        .expect("JDI launch run should return timeout while worker sleeps");
    assert!(
        matches!(
            timed_out.result,
            CommandResult::Timeout {
                state: RunState::Running,
                ..
            }
        ),
        "expected initial run timeout, got {:?}",
        timed_out.result
    );

    let status = wait_for_jdi_breakpoint_status(&session, line);
    match status {
        CommandResult::Status {
            state,
            last_event: Some(Event::Breakpoint { location, thread }),
            ..
        } => {
            assert_eq!(state, RunState::Suspended);
            assert_eq!(location.class, "AsyncBreakpointTest");
            assert_eq!(location.method, "hit");
            assert_eq!(location.line, line);
            assert_eq!(thread, "delayed-worker");
        }
        other => panic!("expected async breakpoint status, got {other:?}"),
    }

    let stack = session
        .stack(false)
        .expect("JDI stack should work after async stop event");
    match &stack.result {
        CommandResult::StackTrace { frames } => {
            let top = frames.first().expect("stack should include top frame");
            assert_eq!(top.location.class, "AsyncBreakpointTest");
            assert_eq!(top.location.method, "hit");
            assert_eq!(top.location.line, line);
        }
        other => panic!("expected stack trace after async stop, got {other:?}"),
    }

    let this_value = session
        .evaluate("this")
        .expect("JDI print this should work after async stop event");
    match &this_value.result {
        CommandResult::Value { value, .. } => {
            assert!(
                value.contains("AsyncBreakpointTest@"),
                "this should render as AsyncBreakpointTest object, got {value}"
            );
        }
        other => panic!("expected value for this, got {other:?}"),
    }

    let queued_stop = session
        .cont(Some(1))
        .expect("JDI cont after async stop should surface queued stop");
    match &queued_stop.result {
        CommandResult::Stopped {
            event: Event::Breakpoint { location, thread },
            ..
        } => {
            assert_eq!(location.class, "AsyncBreakpointTest");
            assert_eq!(location.method, "hit");
            assert_eq!(location.line, line);
            assert_eq!(thread, "delayed-worker");
        }
        other => panic!("expected queued async breakpoint stop, got {other:?}"),
    }

    let resumed = session
        .cont(Some(1))
        .expect("JDI cont after queued stop should resume current stop");
    assert!(
        matches!(
            resumed.result,
            CommandResult::Timeout {
                state: RunState::Running,
                ..
            }
        ),
        "second cont should resume the async stop: {:?}",
        resumed.result
    );

    let _ = session.kill();
}

#[test]
fn jdi_method_entry_and_exit_events_stop_with_return_value() {
    use java_agent_debugger::jdi::session::JdiSession;

    let _guard = jdi_e2e_guard();
    compile_java_fixture("MethodEventTest.java");
    let target = start_jdwp_fixture("MethodEventTest");
    let sourcepath = vec![fixture_dir().display().to_string()];
    let session = JdiSession::attach(
        "127.0.0.1",
        target.port,
        &sourcepath,
        "jdi-method-entry-exit".into(),
        None,
    )
    .expect("JDI attach failed");

    let method_bp = session
        .break_in(
            "MethodEventTest",
            "work",
            Some("int,java.lang.String"),
            MethodEventKind::Entry,
            Some("thread"),
        )
        .expect("JDI method entry breakpoint failed");
    match &method_bp.result {
        CommandResult::BreakpointSet {
            bp_kind: BreakpointKind::Method,
            deferred: true,
            ..
        } => {}
        other => panic!("expected deferred method breakpoint, got {other:?}"),
    }

    let entry = session
        .cont(Some(30))
        .expect("JDI method entry cont failed");
    match &entry.result {
        CommandResult::Stopped {
            event: Event::MethodEntry { thread, .. },
            location,
            thread_id,
            ..
        } => {
            assert_eq!(location.class, "MethodEventTest");
            assert_eq!(location.method, "work");
            assert!(
                !thread.is_empty(),
                "method entry should include thread name"
            );
            assert!(
                thread_id.is_some(),
                "method entry should include a selectable thread id"
            );
        }
        other => panic!("expected method entry stop, got {other:?}"),
    }

    session
        .break_in(
            "MethodEventTest",
            "work",
            Some("int,java.lang.String"),
            MethodEventKind::Exit,
            Some("thread"),
        )
        .expect("JDI method exit breakpoint failed");
    let exit = session.cont(Some(30)).expect("JDI method exit cont failed");
    match &exit.result {
        CommandResult::Stopped {
            event:
                Event::MethodExit {
                    return_value,
                    return_type,
                    ..
                },
            location,
            ..
        } => {
            assert_eq!(location.class, "MethodEventTest");
            assert_eq!(location.method, "work");
            assert_eq!(return_value.as_deref(), Some("4"));
            assert_eq!(return_type.as_deref(), Some("int"));
        }
        other => panic!("expected method exit stop, got {other:?}"),
    }

    let _ = session.kill();
}

#[test]
fn jdi_method_both_stops_on_entry_then_exit() {
    use java_agent_debugger::jdi::session::JdiSession;

    let _guard = jdi_e2e_guard();
    compile_java_fixture("MethodEventTest.java");
    let target = start_jdwp_fixture("MethodEventTest");
    let sourcepath = vec![fixture_dir().display().to_string()];
    let session = JdiSession::attach(
        "127.0.0.1",
        target.port,
        &sourcepath,
        "jdi-method-both".into(),
        None,
    )
    .expect("JDI attach failed");

    session
        .break_in(
            "MethodEventTest",
            "work",
            Some("int,java.lang.String"),
            MethodEventKind::Both,
            None,
        )
        .expect("JDI method both breakpoint failed");

    let entry = session.cont(Some(30)).expect("JDI both entry cont failed");
    assert!(
        matches!(
            entry.result,
            CommandResult::Stopped {
                event: Event::MethodEntry { .. },
                ..
            }
        ),
        "expected method entry first, got {:?}",
        entry.result
    );
    let exit = session.cont(Some(30)).expect("JDI both exit cont failed");
    assert!(
        matches!(
            exit.result,
            CommandResult::Stopped {
                event: Event::MethodExit { .. },
                ..
            }
        ),
        "expected method exit second, got {:?}",
        exit.result
    );

    let _ = session.kill();
}

#[test]
fn mcp_jdi_launch_breakpoint_run_locals_smoke() {
    let _guard = jdi_e2e_guard();
    compile_java_fixture("JdiLaunchTest.java");
    let guard = TestDaemonGuard::new("mcp-jdi-launch-smoke");
    let sourcepath = fixture_dir().display().to_string();
    let classpath = fixture_dir().display().to_string();
    let line = fixture_line("JdiLaunchTest.java", "System.out.println(label");
    let messages = vec![
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "jdbg-test", "version": "0"}
            }
        }),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "launch",
                "arguments": {
                    "backend": "jdi",
                    "main_class": "JdiLaunchTest",
                    "classpath": classpath,
                    "sourcepath": sourcepath
                }
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "break_at",
                "arguments": {"class": "JdiLaunchTest", "line": line}
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "run",
                "arguments": {"timeout": 10}
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": {"name": "locals", "arguments": {}}
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "tools/call",
            "params": {"name": "kill", "arguments": {}}
        }),
    ];

    let responses = run_mcp_jsonrpc(&messages, &guard);
    for id in 2..=6 {
        assert_mcp_success(mcp_response(&responses, id));
    }
    assert!(mcp_text(mcp_response(&responses, 2)).contains("Jdi Launch"));
    assert!(mcp_text(mcp_response(&responses, 4)).contains("Breakpoint hit"));
    assert!(mcp_text(mcp_response(&responses, 5)).contains("label"));
}

#[test]
fn jdb_method_exit_break_in_is_explicitly_unsupported() {
    let guard = TestDaemonGuard::new("jdb-method-exit-unsupported");
    compile_java_fixture("MethodEventTest.java");
    let classpath = fixture_dir().display().to_string();
    let sourcepath = fixture_dir().display().to_string();

    let launch = jdbg_command(&guard)
        .arg("launch")
        .arg("MethodEventTest")
        .arg("--classpath")
        .arg(&classpath)
        .arg("--sourcepath")
        .arg(&sourcepath)
        .output()
        .expect("spawn jdbg launch");
    assert!(
        launch.status.success(),
        "launch failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&launch.stdout),
        String::from_utf8_lossy(&launch.stderr)
    );

    let break_in = jdbg_command(&guard)
        .arg("break-in")
        .arg("MethodEventTest")
        .arg("work")
        .arg("--args")
        .arg("int,java.lang.String")
        .arg("--event")
        .arg("exit")
        .output()
        .expect("spawn jdbg break-in");

    assert!(
        !break_in.status.success(),
        "jdb break-in --event exit should fail"
    );
    let stderr = String::from_utf8_lossy(&break_in.stderr);
    assert!(
        stderr.contains("not supported") && stderr.contains("jdb") && stderr.contains("break_in"),
        "error should be explicit unsupported-backend message, got {stderr}"
    );

    let _ = jdbg_command(&guard).arg("kill").output();
}

#[test]
fn four_concurrent_jdi_cli_clients_debug_distinct_targets() {
    let _guard = jdi_e2e_guard();
    compile_java_fixture("JdiLaunchTest.java");
    let guard = TestDaemonGuard::new("jdi-four-clients");
    let sourcepath = fixture_dir().display().to_string();
    let line = fixture_line("JdiLaunchTest.java", "System.out.println(label");
    let targets: Vec<_> = (0..4)
        .map(|_| start_jdwp_fixture("JdiLaunchTest"))
        .collect();
    let ports: Vec<_> = targets.iter().map(|target| target.port).collect();

    let handles: Vec<_> = ports
        .into_iter()
        .enumerate()
        .map(|(index, port)| {
            let username = guard.username.clone();
            let data_dir = guard.data_dir.clone();
            let sourcepath = sourcepath.clone();
            std::thread::spawn(move || {
                let run = |args: &[String]| {
                    let mut command = Command::new(env!("CARGO_BIN_EXE_jdbg"));
                    command
                        .env("USERNAME", &username)
                        .env("USER", &username)
                        .env("JDBG_DATA_DIR", &data_dir)
                        .stdin(Stdio::null())
                        .args(args);
                    hide_console(&mut command);
                    let output = command.output().expect("spawn jdbg client");
                    assert!(
                        output.status.success(),
                        "jdbg {:?} failed\nstdout:\n{}\nstderr:\n{}",
                        args,
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    );
                    String::from_utf8_lossy(&output.stdout).to_string()
                };

                let attach = run(&[
                    "attach".into(),
                    "--backend".into(),
                    "jdi".into(),
                    "--host".into(),
                    "127.0.0.1".into(),
                    "--port".into(),
                    port.to_string(),
                    "--sourcepath".into(),
                    sourcepath,
                    "--name".into(),
                    format!("agent-{index}"),
                ]);
                let session = attach
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_else(|| panic!("missing session id in attach output: {attach}"))
                    .to_string();
                assert!(attach.contains("Jdi Attach"), "{attach}");

                let breakpoint = run(&[
                    "--session".into(),
                    session.clone(),
                    "break-at".into(),
                    "JdiLaunchTest".into(),
                    line.to_string(),
                ]);
                assert!(breakpoint.contains("Breakpoint set"), "{breakpoint}");

                let stop = run(&[
                    "--session".into(),
                    session.clone(),
                    "--timeout".into(),
                    "20".into(),
                    "cont".into(),
                ]);
                assert!(stop.contains("Breakpoint hit"), "{stop}");
                assert!(stop.contains("JdiLaunchTest.main()"), "{stop}");

                let locals = run(&["--session".into(), session.clone(), "locals".into()]);
                assert!(locals.contains("label"), "{locals}");
                assert!(locals.contains("hello"), "{locals}");

                let exited = run(&[
                    "--session".into(),
                    session,
                    "--timeout".into(),
                    "20".into(),
                    "cont".into(),
                ]);
                assert!(
                    exited.contains("The application exited"),
                    "expected VM exit, got {exited}"
                );
            })
        })
        .collect();

    for handle in handles {
        handle.join().expect("join JDI client thread");
    }
}

#[test]
fn jdi_watchpoint_modification_hit() {
    use java_agent_debugger::jdi::session::JdiSession;

    let _guard = jdi_e2e_guard();
    compile_java_fixture("WatchTest.java");
    let target = start_jdwp_fixture("WatchTest");
    let sourcepath = vec![fixture_dir().display().to_string()];
    let session = JdiSession::attach(
        "127.0.0.1",
        target.port,
        &sourcepath,
        "jdi-watch-modification".into(),
        None,
    )
    .expect("JDI attach failed");

    let watch = session
        .watch("WatchTest.name", "modification")
        .expect("JDI watch failed");
    match &watch.result {
        CommandResult::WatchSet {
            spec,
            mode,
            deferred,
        } => {
            assert_eq!(spec, "WatchTest.name");
            assert_eq!(mode, "modification");
            assert!(deferred, "watchpoint should defer until WatchTest loads");
        }
        other => panic!("expected JDI WatchSet, got {other:?}"),
    }

    let stop = session.cont(Some(30)).expect("JDI cont failed");
    match &stop.result {
        CommandResult::Stopped {
            event:
                Event::FieldWatch {
                    field,
                    access_type,
                    thread,
                },
            location,
            thread_id,
            frame: Some(frame),
            ..
        } => {
            assert_eq!(field, "WatchTest.name");
            assert_eq!(access_type, "modified");
            assert!(!thread.is_empty(), "watchpoint thread should be named");
            assert!(
                thread_id.is_some(),
                "JDI watchpoint stop should include thread id"
            );
            assert_eq!(location.class, "WatchTest");
            assert_eq!(frame.location.class, "WatchTest");
        }
        other => panic!("expected JDI FieldWatch stop, got {other:?}"),
    }

    let _ = session.kill();
}

#[test]
fn jdi_unwatch_removes_deferred_watchpoint_before_hit() {
    use java_agent_debugger::jdi::session::JdiSession;

    let _guard = jdi_e2e_guard();
    compile_java_fixture("WatchTest.java");
    let target = start_jdwp_fixture("WatchTest");
    let sourcepath = vec![fixture_dir().display().to_string()];
    let session = JdiSession::attach(
        "127.0.0.1",
        target.port,
        &sourcepath,
        "jdi-unwatch-deferred".into(),
        None,
    )
    .expect("JDI attach failed");

    session
        .watch("WatchTest.name", "modification")
        .expect("JDI watch failed");
    let removed = session
        .unwatch("WatchTest.name", "modification")
        .expect("JDI unwatch failed");
    match &removed.result {
        CommandResult::Raw { text } => {
            assert!(text.contains("Watch removed"), "{text}");
            assert!(text.contains("WatchTest.name"), "{text}");
        }
        other => panic!("expected raw unwatch result, got {other:?}"),
    }

    let response = session.cont(Some(10)).expect("JDI cont failed");
    match &response.result {
        CommandResult::VmExited { .. } => {}
        other => panic!("expected VM exit after deferred unwatch, got {other:?}"),
    }

    let _ = session.kill();
}

#[test]
fn jdi_unwatch_removes_active_watchpoint_after_first_hit() {
    use java_agent_debugger::jdi::session::JdiSession;

    let _guard = jdi_e2e_guard();
    compile_java_fixture("WatchTwiceTest.java");
    let target = start_jdwp_fixture("WatchTwiceTest");
    let sourcepath = vec![fixture_dir().display().to_string()];
    let session = JdiSession::attach(
        "127.0.0.1",
        target.port,
        &sourcepath,
        "jdi-unwatch-active".into(),
        None,
    )
    .expect("JDI attach failed");

    session
        .watch("WatchTwiceTest.phase", "modification")
        .expect("JDI phase watch failed");
    let phase_stop = session
        .cont(Some(30))
        .expect("JDI cont to phase watch failed");
    assert!(
        matches!(
            phase_stop.result,
            CommandResult::Stopped {
                event: Event::FieldWatch { .. },
                ..
            }
        ),
        "expected phase watch before active watchpoint setup, got {:?}",
        phase_stop.result
    );

    let watch = session
        .watch("WatchTwiceTest.name", "modification")
        .expect("JDI active watch failed");
    match &watch.result {
        CommandResult::WatchSet {
            deferred: false, ..
        } => {}
        other => panic!("expected active JDI WatchSet, got {other:?}"),
    }

    let first_hit = session.cont(Some(30)).expect("JDI cont to watch failed");
    match &first_hit.result {
        CommandResult::Stopped {
            event: Event::FieldWatch {
                field, access_type, ..
            },
            location,
            ..
        } => {
            assert_eq!(field, "WatchTwiceTest.name");
            assert_eq!(access_type, "modified");
            assert_eq!(location.class, "WatchTwiceTest");
        }
        other => panic!("expected first active JDI watchpoint hit, got {other:?}"),
    }

    session
        .unwatch("WatchTwiceTest.name", "modification")
        .expect("JDI active unwatch failed");

    let response = session
        .cont(Some(10))
        .expect("JDI cont after unwatch failed");
    match &response.result {
        CommandResult::VmExited { .. } => {}
        other => panic!("expected VM exit after active unwatch, got {other:?}"),
    }

    let _ = session.kill();
}

#[test]
fn jdi_unwatch_modification_from_deferred_all_leaves_access_watchpoint() {
    use java_agent_debugger::jdi::session::JdiSession;

    let _guard = jdi_e2e_guard();
    compile_java_fixture("WatchTest.java");
    let target = start_jdwp_fixture("WatchTest");
    let sourcepath = vec![fixture_dir().display().to_string()];
    let session = JdiSession::attach(
        "127.0.0.1",
        target.port,
        &sourcepath,
        "jdi-unwatch-all-partial".into(),
        None,
    )
    .expect("JDI attach failed");

    let watch = session
        .watch("WatchTest.name", "all")
        .expect("JDI all watch failed");
    match &watch.result {
        CommandResult::WatchSet {
            mode,
            deferred: true,
            ..
        } => assert_eq!(mode, "all"),
        other => panic!("expected deferred all WatchSet, got {other:?}"),
    }

    session
        .unwatch("WatchTest.name", "modification")
        .expect("JDI partial unwatch failed");

    let stop = session.cont(Some(30)).expect("JDI cont failed");
    match &stop.result {
        CommandResult::Stopped {
            event: Event::FieldWatch {
                field, access_type, ..
            },
            ..
        } => {
            assert_eq!(field, "WatchTest.name");
            assert_eq!(access_type, "accessed");
        }
        other => panic!("expected remaining access watchpoint hit, got {other:?}"),
    }

    let _ = session.kill();
}

#[test]
fn mcp_jdi_attach_breakpoint_locals_and_inspect_smoke() {
    let _guard = jdi_e2e_guard();
    compile_java_fixture("StructuredInspectTest.java");
    let target = start_jdwp_fixture("StructuredInspectTest");
    let guard = TestDaemonGuard::new("mcp-jdi-smoke");
    let sourcepath = fixture_dir().display().to_string();
    let inspect_line = fixture_line(
        "StructuredInspectTest.java",
        "System.out.println(root.name);",
    );
    let messages = vec![
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "jdbg-test", "version": "0"}
            }
        }),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "attach",
                "arguments": {
                    "backend": "jdi",
                    "host": "127.0.0.1",
                    "port": target.port,
                    "sourcepath": sourcepath
                }
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "break_at",
                "arguments": {"class": "StructuredInspectTest", "line": inspect_line}
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "cont",
                "arguments": {"timeout": 10}
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": {"name": "locals", "arguments": {}}
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "tools/call",
            "params": {
                "name": "inspect",
                "arguments": {"expr": "root", "max_elements": 2}
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": {"name": "kill", "arguments": {}}
        }),
    ];

    let responses = run_mcp_jsonrpc(&messages, &guard);
    assert!(
        mcp_response(&responses, 1)
            .pointer("/result/capabilities/tools")
            .is_some(),
        "initialize should expose tools capability"
    );
    for id in 2..=7 {
        assert_mcp_success(mcp_response(&responses, id));
    }

    let attach = mcp_text(mcp_response(&responses, 2));
    assert!(attach.contains("Jdi Attach"), "{attach}");
    let breakpoint = mcp_text(mcp_response(&responses, 3));
    assert!(breakpoint.contains("Breakpoint set"), "{breakpoint}");
    let stopped = mcp_text(mcp_response(&responses, 4));
    assert!(stopped.contains("Breakpoint hit"), "{stopped}");
    assert!(stopped.contains("StructuredInspectTest"), "{stopped}");
    let locals = mcp_text(mcp_response(&responses, 5));
    assert!(locals.contains("root"), "{locals}");
    let inspect = mcp_text(mcp_response(&responses, 6));
    assert!(inspect.contains("\"kind\": \"object\""), "{inspect}");
    assert!(inspect.contains("\"kind\": \"collection\""), "{inspect}");
    let kill = mcp_text(mcp_response(&responses, 7));
    assert!(kill.contains("killed"), "{kill}");
}

#[test]
fn mcp_jdi_eval_set_force_return_smoke() {
    let _guard = jdi_e2e_guard();
    compile_java_fixture("EvalMutationTest.java");
    let target = start_jdwp_fixture("EvalMutationTest");
    let guard = TestDaemonGuard::new("mcp-jdi-eval-set-force-return");
    let sourcepath = fixture_dir().display().to_string();
    let compute_line = fixture_line("EvalMutationTest.java", "int before = box.add(values[1]);");
    let post_return_line = fixture_line("EvalMutationTest.java", "System.out.println(\"result=\"");
    let messages = vec![
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "jdbg-test", "version": "0"}
            }
        }),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "attach",
                "arguments": {
                    "backend": "jdi",
                    "host": "127.0.0.1",
                    "port": target.port,
                    "sourcepath": sourcepath
                }
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "break_at",
                "arguments": {"class": "EvalMutationTest", "line": compute_line}
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "cont",
                "arguments": {"timeout": 10}
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": {
                "name": "print",
                "arguments": {
                    "expr": "box.add(values[1]) + local + EvalMutationTest.staticAdd(1, 2)"
                }
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "tools/call",
            "params": {
                "name": "set",
                "arguments": {"lvalue": "box.count", "value": "10"}
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": {
                "name": "set",
                "arguments": {"lvalue": "values[1]", "value": "5"}
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 8,
            "method": "tools/call",
            "params": {
                "name": "break_at",
                "arguments": {"class": "EvalMutationTest", "line": post_return_line}
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 9,
            "method": "tools/call",
            "params": {
                "name": "force_return",
                "arguments": {"value": "123"}
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "tools/call",
            "params": {
                "name": "cont",
                "arguments": {"timeout": 10}
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 11,
            "method": "tools/call",
            "params": {
                "name": "eval",
                "arguments": {"expr": "result"}
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 12,
            "method": "tools/call",
            "params": {
                "name": "inspect",
                "arguments": {"expr": "box", "max_elements": 2}
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 13,
            "method": "tools/call",
            "params": {"name": "kill", "arguments": {}}
        }),
    ];

    let responses = run_mcp_jsonrpc(&messages, &guard);
    for id in 2..=13 {
        assert_mcp_success(mcp_response(&responses, id));
    }

    let compute_breakpoint = mcp_text(mcp_response(&responses, 3));
    assert!(
        compute_breakpoint.contains("Breakpoint set"),
        "{compute_breakpoint}"
    );
    let compute_stop = mcp_text(mcp_response(&responses, 4));
    assert!(compute_stop.contains("Breakpoint hit"), "{compute_stop}");
    assert!(compute_stop.contains("EvalMutationTest"), "{compute_stop}");
    let printed = mcp_text(mcp_response(&responses, 5));
    assert!(printed.contains("= 15"), "{printed}");
    let set_field = mcp_text(mcp_response(&responses, 6));
    assert!(set_field.contains("box.count = 10"), "{set_field}");
    let set_array = mcp_text(mcp_response(&responses, 7));
    assert!(set_array.contains("values[1] = 5"), "{set_array}");
    let post_breakpoint = mcp_text(mcp_response(&responses, 8));
    assert!(
        post_breakpoint.contains("Breakpoint set"),
        "{post_breakpoint}"
    );
    let forced = mcp_text(mcp_response(&responses, 9));
    assert!(
        forced.contains("Forced current method to return 123"),
        "{forced}"
    );
    let stopped = mcp_text(mcp_response(&responses, 10));
    assert!(stopped.contains("Breakpoint hit"), "{stopped}");
    assert!(stopped.contains("EvalMutationTest"), "{stopped}");
    let result = mcp_text(mcp_response(&responses, 11));
    assert!(result.contains("= 123"), "{result}");
    let inspect = mcp_text(mcp_response(&responses, 12));
    assert!(inspect.contains("\"name\": \"count\""), "{inspect}");
    assert!(inspect.contains("\"value\": \"10\""), "{inspect}");
    let kill = mcp_text(mcp_response(&responses, 13));
    assert!(kill.contains("killed"), "{kill}");
}

#[test]
fn mcp_jdi_watch_unwatch_smoke() {
    let _guard = jdi_e2e_guard();
    compile_java_fixture("WatchTwiceTest.java");
    let target = start_jdwp_fixture("WatchTwiceTest");
    let guard = TestDaemonGuard::new("mcp-jdi-watch-smoke");
    let sourcepath = fixture_dir().display().to_string();
    let messages = vec![
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "jdbg-test", "version": "0"}
            }
        }),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "attach",
                "arguments": {
                    "backend": "jdi",
                    "host": "127.0.0.1",
                    "port": target.port,
                    "sourcepath": sourcepath
                }
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "watch",
                "arguments": {"field": "WatchTwiceTest.phase", "mode": "modification"}
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "cont",
                "arguments": {"timeout": 10}
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": {
                "name": "watch",
                "arguments": {"field": "WatchTwiceTest.name", "mode": "modification"}
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "tools/call",
            "params": {
                "name": "cont",
                "arguments": {"timeout": 10}
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": {
                "name": "unwatch",
                "arguments": {"field": "WatchTwiceTest.name", "mode": "modification"}
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 8,
            "method": "tools/call",
            "params": {
                "name": "cont",
                "arguments": {"timeout": 10}
            }
        }),
    ];

    let responses = run_mcp_jsonrpc(&messages, &guard);
    for id in 2..=8 {
        assert_mcp_success(mcp_response(&responses, id));
    }

    let watch = mcp_text(mcp_response(&responses, 5));
    assert!(watch.contains("Watch set (modification)"), "{watch}");
    assert!(watch.contains("WatchTwiceTest.name"), "{watch}");
    let stopped = mcp_text(mcp_response(&responses, 6));
    assert!(
        stopped.contains("Field watchpoint hit (modified)"),
        "{stopped}"
    );
    assert!(stopped.contains("WatchTwiceTest.name"), "{stopped}");
    let removed = mcp_text(mcp_response(&responses, 7));
    assert!(removed.contains("Watch removed"), "{removed}");
    let exited = mcp_text(mcp_response(&responses, 8));
    assert!(exited.contains("The application exited"), "{exited}");
}

#[test]
fn java_sidecar_self_tests_cover_protocol_errors_and_value_limits() {
    run_java_sidecar_self_tests();
}

#[test]
fn conditional_breakpoint_set_accepted() {
    let session = launch_fixture("CollectionTest");

    // Verify the breakpoint is accepted by jdb. stop_at does not handle conditions; conditional logic is in handler.
    let bp_resp = session
        .stop_at("CollectionTest", 10, None, None)
        .expect("stop_at failed");
    assert!(
        matches!(bp_resp.result, CommandResult::BreakpointSet { .. }),
        "conditional breakpoint should be accepted, got {:?}",
        bp_resp.result
    );

    // Condition is true, so execution should stop.
    let run_resp = session.run(Some(30)).expect("run failed");
    assert!(
        matches!(run_resp.result, CommandResult::Stopped { .. }),
        "breakpoint with condition=true should stop, got {:?}",
        run_resp.result
    );

    let _ = session.kill();
}

#[test]
fn attach_to_unreachable_port_gives_clear_error() {
    use java_agent_debugger::jdb::process::AttachConfig;

    let config = AttachConfig {
        host: "127.0.0.1".to_string(),
        port: 19999, // No JDWP listener on this port.
        sourcepath: vec![],
    };
    let result = Session::attach(&jdb_path(), &config, "test-unreachable".into(), None);
    let err = match result {
        Err(e) => e,
        Ok(_) => panic!("should fail on unreachable port"),
    };

    let msg = err.to_string();
    assert!(
        msg.contains("not reachable") || msg.contains("Connection refused"),
        "error should mention unreachable/refused, got: {msg}"
    );
    assert!(
        msg.contains("JDWP"),
        "error should mention JDWP for diagnosis, got: {msg}"
    );
}

/// Regression: attach entry normalizes `localhost` to `127.0.0.1`, and that normalization reaches `probe_tcp`.
/// On dual-stack machines, `localhost` may resolve to IPv6 `[::1]`, while JDWP often listens on IPv4, causing refusal.
/// Use an unreachable port to trigger probe_tcp failure; the error should mention normalized `127.0.0.1`
/// instead of raw `localhost`, proving both probe and later spawn use the IPv4 literal.
#[test]
fn attach_normalizes_localhost_to_ipv4_loopback() {
    use java_agent_debugger::jdb::process::AttachConfig;

    let config = AttachConfig {
        host: "localhost".to_string(),
        port: 19999, // No JDWP listener on this port.
        sourcepath: vec![],
    };
    let result = Session::attach(&jdb_path(), &config, "test-localhost-norm".into(), None);
    let msg = match result {
        Err(e) => e.to_string(),
        Ok(_) => panic!("should fail on unreachable port"),
    };

    assert!(
        msg.contains("127.0.0.1"),
        "error should show the normalized IPv4 loopback, got: {msg}"
    );
    assert!(
        !msg.contains("localhost"),
        "raw 'localhost' must have been normalized away, got: {msg}"
    );
}

// ─── Phase: thread suspend policy (native jdb `stop thread`) ───

/// Thread-policy breakpoint: `stop thread at` syntax is accepted by jdb, and a hit parses normally as Stopped.
/// The triggering thread is "worker"; the event banner matches normal breakpoints, so parser remains compatible.
#[test]
fn thread_breakpoint_hit_resolves_worker() {
    let session = launch_fixture("ThreadTest");

    // suspend="thread" makes jdb issue `stop thread at` (SUSPEND_THREAD policy).
    let bp = session
        .stop_in("ThreadTest", "doWork", None, Some("thread"), None)
        .expect("stop_in failed");
    assert!(
        matches!(bp.result, CommandResult::BreakpointSet { .. }),
        "thread breakpoint should be accepted, got {:?}",
        bp.result
    );

    let run_resp = session.run(Some(30)).expect("run failed");
    // PartialStop enrichment: truncated banner (JDK 8 SUSPEND_THREAD) is automatically enriched by the
    // session layer through threads→thread<id>→where to fill thread/location/frame.
    match &run_resp.result {
        CommandResult::Stopped {
            location,
            thread,
            thread_id,
            frame,
            ..
        } => {
            assert_eq!(location.class, "ThreadTest");
            assert_eq!(location.method, "doWork");
            assert_eq!(thread, "worker", "the hit thread should be 'worker'");
            // PartialStop path should fill hit thread id in the session layer without extra cost, reusing that `threads`.
            let tid = thread_id
                .as_ref()
                .expect("thread_id should be filled on a thread-policy hit");
            assert!(!tid.is_empty(), "thread_id should be non-empty");
            // frame should be backfilled by the session layer from `where`, letting handler skip a duplicate where query.
            let f = frame
                .as_ref()
                .expect("frame should be enriched by session layer");
            assert_eq!(f.location.class, "ThreadTest");
            assert_eq!(f.location.method, "doWork");
            assert!(f.location.line > 0, "enriched frame line should be nonzero");

            // The backfilled id must actually switch to that thread; this is the step that failed in the transcript.
            let sw = session
                .execute(
                    &format!("thread {tid}"),
                    java_agent_debugger::session::CommandKind::normal(
                        java_agent_debugger::jdb::parser::CommandHint::Other,
                    )
                    .with_timeout_secs(Some(5)),
                )
                .expect("thread switch failed");
            // jdb accepted it, so this should not be an error like "not a valid thread id".
            if let CommandResult::Raw { text } = &sw.result {
                assert!(
                    !text.contains("not a valid thread id"),
                    "thread <{tid}> rejected by jdb: {text:?}"
                );
            }
        }
        other => panic!("expected Stopped in worker/doWork, got {other:?}"),
    }

    // Run handler-layer enrichment: use the frame already filled by session to add source_context via list.
    let mut resp = run_resp;
    enrich_stopped_test_helper(&session, &mut resp);
    match &resp.result {
        CommandResult::Stopped {
            source_context,
            location,
            ..
        } => {
            let lines = source_context
                .as_ref()
                .expect("source_context should be enriched via handler layer");
            assert!(
                lines.iter().any(|l| l.number == location.line),
                "source_context should contain the hit line {}",
                location.line
            );
        }
        other => panic!("expected Stopped after enrich, got {other:?}"),
    }

    let _ = session.kill();
}

/// Core regression: under thread policy the VM does not SUSPEND_ALL. While worker is stopped at a breakpoint,
/// the daemon heartbeat thread keeps incrementing heartbeatCount, proving other threads are not frozen.
/// The old suspend-count simulation would silently fall back to all here due to spec mismatch and freeze all threads.
#[test]
fn thread_breakpoint_keeps_other_threads_running() {
    let session = launch_fixture("ThreadTest");

    session
        .stop_in("ThreadTest", "doWork", None, Some("thread"), None)
        .expect("stop_in failed");
    let run_resp = session.run(Some(30)).expect("run failed");
    assert!(
        matches!(&run_resp.result, CommandResult::Stopped { .. }),
        "expected Stopped, got {:?}",
        run_resp.result
    );

    // worker is stopped at a breakpoint. Poll the actual condition instead of assuming a fixed number of
    // debugger round-trips is enough time for the heartbeat thread to be scheduled on every CI platform.
    let c1 = try_eval_int_helper(&session, "ThreadTest.heartbeatCount", Some(5))
        .expect("should read heartbeatCount (c1)");
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut c2 = c1;
    let mut polls = 0;
    while c2 <= c1 && Instant::now() < deadline {
        polls += 1;
        let _ = session.threads(Some(5));
        if let Some(value) = try_eval_int_helper(&session, "ThreadTest.heartbeatCount", Some(5)) {
            c2 = value;
        }
    }

    assert!(
        c2 > c1,
        "heartbeat must keep counting under thread policy (c1={c1}, c2={c2}, polls={polls}); \
         if equal, the whole VM was frozen — a SUSPEND_ALL regression"
    );

    let _ = session.kill();
}

// ─── Phase: new commands (suspend/resume/set/ignore/lock/threadlocks) ───

/// Verify the 6 new command syntaxes are accepted by real jdb, especially `set <lvalue> = <expr>`.
/// After doWork is hit, execute them one by one and assert output has no obvious jdb error.
#[test]
fn new_commands_accepted_by_jdb() {
    use java_agent_debugger::jdb::parser::CommandHint;
    use java_agent_debugger::session::CommandKind;

    let session = launch_fixture("ThreadTest");
    session
        .stop_in("ThreadTest", "doWork", None, Some("thread"), None)
        .expect("stop_in failed");
    let run_resp = session.run(Some(30)).expect("run failed");
    let tid = match &run_resp.result {
        CommandResult::Stopped { thread_id, .. } => {
            thread_id.clone().expect("thread_id should be populated")
        }
        other => panic!("expected Stopped, got {other:?}"),
    };

    let exec = |cmd: &str| {
        session
            .execute(
                cmd,
                CommandKind::normal(CommandHint::Other).with_timeout_secs(Some(5)),
            )
            .unwrap_or_else(|e| panic!("`{cmd}` errored: {e}"))
    };
    // jdb returns "Unrecognized command ..." for unknown commands and "Usage: ..." for syntax errors.
    let assert_ok = |cmd: &str, resp: &CommandResponse| {
        if let CommandResult::Raw { text } = &resp.result {
            assert!(
                !text.contains("Unrecognized command") && !text.starts_with("Usage:"),
                "`{cmd}` rejected by jdb: {text:?}"
            );
        }
    };

    for cmd in [
        format!("suspend {tid}"),
        format!("resume {tid}"),
        "set x = 99".to_string(), // local int x in doWork.
        "ignore java.lang.NullPointerException".to_string(),
        "threadlocks".to_string(),
    ] {
        let r = exec(&cmd);
        assert_ok(&cmd, &r);
    }

    let _ = session.kill();
}

// ─── Phase: classes / methods search ───

#[test]
fn classes_returns_loaded_classes() {
    let session = launch_fixture("CollectionTest");

    // First run to the breakpoint so the class is loaded.
    session
        .stop_at("CollectionTest", 10, None, None)
        .expect("stop_at failed");
    let run_resp = session.run(Some(30)).expect("run failed");
    assert!(matches!(&run_resp.result, CommandResult::Stopped { .. }));

    // Execute classes command with pattern filtering.
    use java_agent_debugger::jdb::parser::CommandHint;
    use java_agent_debugger::session::CommandKind;

    let resp = session
        .execute(
            "classes CollectionTest",
            CommandKind::normal(CommandHint::Classes).with_timeout_secs(Some(10)),
        )
        .expect("classes failed");

    match &resp.result {
        CommandResult::Classes { classes } => {
            assert!(
                !classes.is_empty(),
                "should find at least one class matching 'CollectionTest'"
            );
            assert!(
                classes.iter().any(|c| c.contains("CollectionTest")),
                "should contain CollectionTest, got: {:?}",
                classes
            );
        }
        other => panic!("expected Classes, got {other:?}"),
    }

    let _ = session.kill();
}

#[test]
fn methods_returns_class_methods() {
    let session = launch_fixture("CollectionTest");

    session
        .stop_at("CollectionTest", 10, None, None)
        .expect("stop_at failed");
    let run_resp = session.run(Some(30)).expect("run failed");
    assert!(matches!(&run_resp.result, CommandResult::Stopped { .. }));

    use java_agent_debugger::jdb::parser::CommandHint;
    use java_agent_debugger::session::CommandKind;

    let resp = session
        .execute(
            "methods CollectionTest",
            CommandKind::normal(CommandHint::Methods).with_timeout_secs(Some(10)),
        )
        .expect("methods failed");

    match &resp.result {
        CommandResult::Methods { methods, .. } => {
            assert!(
                !methods.is_empty(),
                "should find methods for CollectionTest"
            );
            // CollectionTest has a main method.
            assert!(
                methods.iter().any(|m| m.contains("main")),
                "should contain main method, got: {:?}",
                methods
            );
        }
        other => panic!("expected Methods, got {other:?}"),
    }

    let _ = session.kill();
}

// ─── Phase: watch / unwatch ───

#[test]
fn watch_set_accepted() {
    let session = launch_fixture("WatchTest");

    use java_agent_debugger::jdb::parser::CommandHint;
    use java_agent_debugger::session::CommandKind;

    // Set field watchpoint.
    let resp = session
        .execute(
            "watch WatchTest.name",
            CommandKind::normal(CommandHint::WatchSet).with_timeout_secs(Some(10)),
        )
        .expect("watch failed");

    match &resp.result {
        CommandResult::WatchSet { mode, .. } => {
            assert_eq!(mode, "modification");
        }
        other => panic!("expected WatchSet, got {other:?}"),
    }

    let _ = session.kill();
}

#[test]
fn watch_field_modification_hit() {
    let session = launch_fixture("WatchTest");

    use java_agent_debugger::jdb::parser::CommandHint;
    use java_agent_debugger::session::CommandKind;

    // Set field watchpoint.
    session
        .execute(
            "watch WatchTest.name",
            CommandKind::normal(CommandHint::WatchSet).with_timeout_secs(Some(10)),
        )
        .expect("watch failed");

    // Run; should stop when the field is modified.
    let run_resp = session.run(Some(30)).expect("run failed");

    match &run_resp.result {
        CommandResult::Stopped { event, thread, .. } => {
            match event {
                Event::FieldWatch {
                    field, access_type, ..
                } => {
                    assert!(
                        field.contains("name"),
                        "field should contain 'name', got: {field}"
                    );
                    assert_eq!(access_type, "modified");
                }
                _ => {
                    // It may also hit static initialization access first; thread should still be present.
                    assert!(!thread.is_empty(), "thread should not be empty");
                }
            }
        }
        CommandResult::VmExited { .. } => {
            panic!(
                "VM exited without hitting watchpoint — jdb may not support watch on static fields in this JDK version"
            );
        }
        other => panic!("expected Stopped with FieldWatch event, got {other:?}"),
    }

    let _ = session.kill();
}

// ─── Test helpers that mirror handler.rs logic (can't import private fns) ───

fn jdi_inspect_field<'a>(root: &'a Value, name: &str) -> &'a Value {
    let fields = root
        .pointer("/value/fields")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("missing inspect object fields in {root:#?}"));
    fields
        .iter()
        .find(|field| field.get("name").and_then(Value::as_str) == Some(name))
        .and_then(|field| field.get("value"))
        .unwrap_or_else(|| panic!("missing inspect field {name} in {root:#?}"))
}

fn assert_jdi_collection_field(root: &Value, name: &str, expected_values: &[&str]) {
    let field = jdi_inspect_field(root, name);
    assert_eq!(
        field.get("kind").and_then(Value::as_str),
        Some("collection"),
        "field {name} should render as collection: {field:#?}"
    );
    assert_ne!(
        field.get("unavailable").and_then(Value::as_bool),
        Some(true),
        "field {name} should be available: {field:#?}"
    );
    let elements = field
        .get("elements")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("field {name} missing collection elements: {field:#?}"));
    assert!(
        elements.len() >= expected_values.len(),
        "field {name} should render at least {} elements: {field:#?}",
        expected_values.len()
    );
    let rendered = field.to_string();
    for expected in expected_values {
        assert!(
            rendered.contains(expected),
            "field {name} should contain {expected}: {field:#?}"
        );
    }
}

fn assert_jdi_map_field(root: &Value, name: &str, expected_keys: &[&str]) {
    let field = jdi_inspect_field(root, name);
    assert_eq!(
        field.get("kind").and_then(Value::as_str),
        Some("map"),
        "field {name} should render as map: {field:#?}"
    );
    assert_ne!(
        field.get("unavailable").and_then(Value::as_bool),
        Some(true),
        "field {name} should be available: {field:#?}"
    );
    let entries = field
        .get("entries")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("field {name} missing map entries: {field:#?}"));
    assert!(
        entries.len() >= expected_keys.len(),
        "field {name} should render at least {} entries: {field:#?}",
        expected_keys.len()
    );
    let rendered = field.to_string();
    for expected in expected_keys {
        assert!(
            rendered.contains(expected),
            "field {name} should contain {expected}: {field:#?}"
        );
    }
}

fn enrich_stopped_test_helper(session: &Session, resp: &mut CommandResponse) {
    let (location_line, frame_ref, source_ref) = match &mut resp.result {
        CommandResult::Stopped {
            location,
            frame,
            source_context,
            ..
        } => (location.line, frame, source_context),
        _ => return,
    };

    if frame_ref.is_none() {
        if let Ok(stack_resp) = session.stack(Some(5)) {
            if let CommandResult::StackTrace { frames } = &stack_resp.result {
                if let Some(top) = frames.first() {
                    *frame_ref = Some(top.clone());
                }
            }
        }
    }

    if source_ref.is_none() && location_line > 0 {
        if let Ok(src_resp) = session.list_source(Some(location_line), Some(5)) {
            if let CommandResult::Source { lines, .. } = src_resp.result {
                *source_ref = Some(lines);
            }
        }
    }
}

fn check_line_mismatch_test_helper(session: &Session, resp: &mut CommandResponse) {
    let location = match &resp.result {
        CommandResult::Stopped {
            event: Event::Breakpoint { .. },
            location,
            ..
        } => location.clone(),
        _ => return,
    };

    if let Some((ref cls, req_line)) = session.take_break_target() {
        if cls == &location.class && req_line != location.line {
            resp.note = Some(format!(
                "Breakpoint requested at line {} but hit at line {} — \
                 JVM rounded to nearest executable bytecode.",
                req_line, location.line
            ));
        }
    }
}

fn handle_inspect_test_helper(
    session: &Session,
    expr: &str,
    max_elements: u32,
    timeout: Option<u64>,
) -> CommandResponse {
    let max = max_elements.min(50);

    let size = try_eval_int_helper(session, &format!("{expr}.size()"), timeout)
        .or_else(|| try_eval_int_helper(session, &format!("{expr}.length"), timeout));

    let count = match size {
        Some(s) => s.min(max),
        None => max,
    };

    let mut elements = Vec::new();
    for i in 0..count {
        if let Some(val) = try_get_element_helper(session, expr, i, timeout) {
            elements.push(val);
        } else {
            break;
        }
    }

    let truncated = size.map(|s| s > max);
    CommandResponse {
        result: CommandResult::Inspection {
            expr: expr.to_string(),
            size,
            elements,
            truncated,
        },
        stderr: None,
        note: None,
    }
}

fn try_eval_int_helper(session: &Session, expr: &str, timeout: Option<u64>) -> Option<u32> {
    let resp = session.print(expr, timeout).ok()?;
    if let CommandResult::Value { ref value, .. } = resp.result {
        value.trim().parse().ok()
    } else {
        None
    }
}

fn try_get_element_helper(
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
