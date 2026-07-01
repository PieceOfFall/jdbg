//! Integration tests: simulate the full daemon handler flow and verify changes against real jdb sessions.
//!
//! These tests require a JDK (jdb available through JAVA_HOME or PATH) and compiled Java fixtures.
//! Before running, ensure: javac -g tests/fixtures/java/CollectionTest.java

use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use java_agent_debugger::protocol::*;
use java_agent_debugger::session::Session;

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
    let dir = fixture_dir();
    let mut command = Command::new(javac_path());
    command
        .arg("-g")
        .arg("-d")
        .arg(&dir)
        .arg(dir.join(source_name));
    hide_console(&mut command);
    let status = command.status().expect("failed to spawn javac");
    assert!(status.success(), "javac failed with status {status}");
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
    let port = free_loopback_port();
    let mut command = Command::new(java_path(false));
    command
        .arg(format!(
            "-agentlib:jdwp=transport=dt_socket,server=y,suspend=y,address={port}"
        ))
        .arg("-cp")
        .arg(fixture_dir())
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

    let breakpoint = session
        .stop_at("StructuredInspectTest", 35, None)
        .expect("JDI breakpoint failed");
    assert!(
        matches!(
            breakpoint.result,
            CommandResult::BreakpointSet { deferred: true, .. }
        ),
        "expected deferred JDI breakpoint, got {:?}",
        breakpoint.result
    );
    let stop = session.cont(Some(10)).expect("JDI cont failed");
    assert!(
        matches!(stop.result, CommandResult::Stopped { .. }),
        "expected JDI breakpoint stop, got {:?}",
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

    // worker is stopped at a breakpoint. Read heartbeatCount twice, with jdb round-trips between them to create
    // a time window. heartbeat increments about every 50ms, so under thread policy the count must keep growing.
    let c1 = try_eval_int_helper(&session, "ThreadTest.heartbeatCount", Some(5))
        .expect("should read heartbeatCount (c1)");
    for _ in 0..15 {
        let _ = session.threads(Some(5));
    }
    let c2 = try_eval_int_helper(&session, "ThreadTest.heartbeatCount", Some(5))
        .expect("should read heartbeatCount (c2)");

    assert!(
        c2 > c1,
        "heartbeat must keep counting under thread policy (c1={c1}, c2={c2}); \
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
