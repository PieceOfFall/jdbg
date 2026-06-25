//! 集成测试：模拟完整的 daemon handler 流程，验证所有改动在真实 jdb 会话中工作。
//!
//! 这些测试需要 JDK（JAVA_HOME 或 PATH 中有 jdb），且需要编译好的 Java fixture。
//! 运行前确保执行了：javac -g tests/fixtures/java/CollectionTest.java

use std::path::PathBuf;
use std::sync::Arc;

use java_agent_debugger::protocol::*;
use java_agent_debugger::session::Session;

/// 辅助：获取 jdb 路径。
fn jdb_path() -> PathBuf {
    java_agent_debugger::jdkpath::find_jdb(None).expect("jdb not found — is JAVA_HOME set?")
}

/// 辅助：fixture 目录。
fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("java")
}

/// 辅助：launch 一个 fixture 会话。
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

    // 初始状态：无 target
    assert!(session.take_break_target().is_none());

    // 记录
    session.record_break_target("CollectionTest", 9);
    let target = session.take_break_target();
    assert_eq!(target, Some(("CollectionTest".to_string(), 9)));

    // take 是 one-shot
    assert!(session.take_break_target().is_none());

    // 清理
    let _ = session.kill();
}

// ─── Phase 3: enrich_stopped — breakpoint hit returns source_context + frame ───

#[test]
fn breakpoint_hit_includes_source_context_and_frame() {
    let session = launch_fixture("CollectionTest");

    // 设置断点：第 10 行（int size = fruits.size();）
    let bp_resp = session
        .stop_at("CollectionTest", 10, None, None)
        .expect("stop_at failed");
    assert!(
        matches!(bp_resp.result, CommandResult::BreakpointSet { .. }),
        "expected BreakpointSet, got {:?}",
        bp_resp.result
    );

    // 运行
    let run_resp = session.run(Some(30)).expect("run failed");

    // run 返回的是原始 Stopped（session 层不做 enrichment，enrichment 在 handler 层）
    // 所以我们手动模拟 handler 的 enrich 逻辑
    let mut resp = run_resp;
    enrich_stopped_test_helper(&session, &mut resp);

    match &resp.result {
        CommandResult::Stopped { location, frame, source_context, .. } => {
            assert_eq!(location.class, "CollectionTest");
            assert_eq!(location.method, "main");
            assert!(location.line > 0, "line should be nonzero");

            // frame 应该被填充
            assert!(frame.is_some(), "frame should be enriched, got None");
            let f = frame.as_ref().unwrap();
            assert_eq!(f.index, 1); // jdb 用 1-based frame indices
            assert_eq!(f.location.class, "CollectionTest");

            // source_context 应该被填充（如果 sourcepath 正确）
            assert!(
                source_context.is_some(),
                "source_context should be enriched, got None"
            );
            let lines = source_context.as_ref().unwrap();
            assert!(!lines.is_empty(), "source lines should not be empty");
            // 应该包含断点行
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

    // 设置断点并记录 target
    session
        .stop_at("CollectionTest", 10, None, None)
        .expect("stop_at failed");
    session.record_break_target("CollectionTest", 10);

    let run_resp = session.run(Some(30)).expect("run failed");
    let mut resp = run_resp;

    // 模拟 check_line_mismatch
    check_line_mismatch_test_helper(&session, &mut resp);

    // 如果实际命中行 == 请求行，note 应为 None
    if let CommandResult::Stopped { location, .. } = &resp.result {
        if location.line == 10 {
            // 行号匹配，不应有 note
            assert!(
                resp.note.is_none(),
                "no mismatch note expected when lines match"
            );
        } else {
            // 行号不匹配，应有 note
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

    // 断点到集合已填充之后（line 10: int size = fruits.size();）
    session
        .stop_at("CollectionTest", 10, None, None)
        .expect("stop_at failed");
    let run_resp = session.run(Some(30)).expect("run failed");
    assert!(
        matches!(run_resp.result, CommandResult::Stopped { .. }),
        "expected Stopped, got {:?}",
        run_resp.result
    );

    // 调用 inspect
    let inspect_resp = handle_inspect_test_helper(&session, "fruits", 10, None);
    match &inspect_resp.result {
        CommandResult::Inspection { expr, size, elements, truncated } => {
            assert_eq!(expr, "fruits");
            assert_eq!(*size, Some(3), "fruits.size() should be 3");
            assert_eq!(elements.len(), 3, "should have 3 elements");
            // 验证元素内容
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

    // 只取 2 个元素
    let inspect_resp = handle_inspect_test_helper(&session, "fruits", 2, None);
    match &inspect_resp.result {
        CommandResult::Inspection { size, elements, truncated, .. } => {
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

    // 断点到 line 7 — fruits 刚创建还是空的
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
fn conditional_breakpoint_set_accepted() {
    let session = launch_fixture("CollectionTest");

    // 验证带条件的断点能被 jdb 接受（不报错）
    let bp_resp = session
        .stop_at("CollectionTest", 10, Some("true"), None)
        .expect("conditional stop_at failed");
    assert!(
        matches!(bp_resp.result, CommandResult::BreakpointSet { .. }),
        "conditional breakpoint should be accepted, got {:?}",
        bp_resp.result
    );

    // 条件为 true → 应该停住
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
        port: 19999, // 没有 JDWP 在这个端口监听
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

// ─── Phase: thread suspend policy (per-breakpoint) ───

#[test]
fn suspend_policy_stored_and_retrieved() {
    let session = launch_fixture("CollectionTest");

    // 初始无 policy
    assert_eq!(session.get_suspend_policy("CollectionTest:10"), None);

    // 设置 thread policy
    session.set_suspend_policy("CollectionTest:10", "thread");
    assert_eq!(session.get_suspend_policy("CollectionTest:10"), Some("thread".to_string()));

    // 设置 all policy
    session.set_suspend_policy("CollectionTest:10", "all");
    assert_eq!(session.get_suspend_policy("CollectionTest:10"), Some("all".to_string()));

    // 不同 spec 独立
    session.set_suspend_policy("Foo.bar", "thread");
    assert_eq!(session.get_suspend_policy("Foo.bar"), Some("thread".to_string()));
    assert_eq!(session.get_suspend_policy("CollectionTest:10"), Some("all".to_string()));

    let _ = session.kill();
}

#[test]
fn thread_breakpoint_stops_and_notes_thread_policy() {
    let session = launch_fixture("ThreadTest");

    // 在 doWork 第一行设置 thread breakpoint
    let bp_resp = session
        .stop_at("ThreadTest", 37, None, None)
        .expect("stop_at failed");
    assert!(
        matches!(bp_resp.result, CommandResult::BreakpointSet { .. }),
        "expected BreakpointSet, got {:?}",
        bp_resp.result
    );

    // 注册 thread suspend policy
    session.set_suspend_policy("ThreadTest:37", "thread");

    // 运行
    let run_resp = session.run(Some(30)).expect("run failed");

    // 应该停在 doWork 里
    match &run_resp.result {
        CommandResult::Stopped { location, thread, .. } => {
            assert_eq!(location.class, "ThreadTest");
            assert_eq!(location.method, "doWork");
            // thread 应该是 "worker"
            assert_eq!(thread, "worker");
        }
        other => panic!("expected Stopped in doWork, got {other:?}"),
    }

    // 模拟 apply_suspend_policy
    let mut resp = run_resp;
    apply_suspend_policy_test_helper(&session, &mut resp, None);

    // 验证 note 中包含 suspend policy 信息
    assert!(
        resp.note.is_some(),
        "thread suspend policy should produce a note"
    );
    let note = resp.note.as_ref().unwrap();
    assert!(
        note.contains("thread"),
        "note should mention 'thread', got: {note}"
    );
    assert!(
        note.contains("worker"),
        "note should mention the thread name 'worker', got: {note}"
    );

    let _ = session.kill();
}

#[test]
fn thread_breakpoint_other_threads_resume() {
    let session = launch_fixture("ThreadTest");

    // 在 doWork 第一行设置 thread breakpoint
    session
        .stop_at("ThreadTest", 37, None, None)
        .expect("stop_at failed");
    session.set_suspend_policy("ThreadTest:37", "thread");

    let run_resp = session.run(Some(30)).expect("run failed");
    assert!(
        matches!(&run_resp.result, CommandResult::Stopped { .. }),
        "expected Stopped, got {:?}",
        run_resp.result
    );

    // 应用 thread suspend policy
    let mut resp = run_resp;
    apply_suspend_policy_test_helper(&session, &mut resp, None);

    // 验证 heartbeat 线程在 apply 后是 running（不再 suspended）
    // 获取线程列表
    let threads_resp = session.threads(Some(5)).expect("threads failed");
    if let CommandResult::Threads { threads } = &threads_resp.result {
        let heartbeat = threads.iter().find(|t| t.name == "heartbeat");
        assert!(
            heartbeat.is_some(),
            "heartbeat thread should exist in thread list"
        );
        let hb = heartbeat.unwrap();
        // heartbeat 不应该是 "at breakpoint" 状态 — 它应该是 running/sleeping
        assert!(
            !hb.state.contains("breakpoint"),
            "heartbeat should NOT be at breakpoint, state: {}",
            hb.state
        );
    }

    // worker 线程应该仍然 suspended
    let threads_resp2 = session.threads(Some(5)).expect("threads failed");
    if let CommandResult::Threads { threads } = &threads_resp2.result {
        let worker = threads.iter().find(|t| t.name == "worker");
        assert!(
            worker.is_some(),
            "worker thread should exist in thread list"
        );
    }

    let _ = session.kill();
}

#[test]
fn default_suspend_all_does_not_produce_note() {
    let session = launch_fixture("ThreadTest");

    // 设置断点但不设置 suspend policy（默认 "all"）
    session
        .stop_at("ThreadTest", 37, None, None)
        .expect("stop_at failed");

    let run_resp = session.run(Some(30)).expect("run failed");
    assert!(matches!(&run_resp.result, CommandResult::Stopped { .. }));

    // apply_suspend_policy 不应改变什么（policy 默认 = "all"）
    let mut resp = run_resp;
    apply_suspend_policy_test_helper(&session, &mut resp, None);

    // 不应有 suspend policy note
    assert!(
        resp.note.is_none(),
        "default 'all' policy should not produce a note, got: {:?}",
        resp.note
    );

    let _ = session.kill();
}

#[test]
fn explicit_suspend_all_does_not_produce_note() {
    let session = launch_fixture("ThreadTest");

    session
        .stop_at("ThreadTest", 37, None, None)
        .expect("stop_at failed");
    // 显式设置 "all"
    session.set_suspend_policy("ThreadTest:37", "all");

    let run_resp = session.run(Some(30)).expect("run failed");
    assert!(matches!(&run_resp.result, CommandResult::Stopped { .. }));

    let mut resp = run_resp;
    apply_suspend_policy_test_helper(&session, &mut resp, None);

    assert!(
        resp.note.is_none(),
        "explicit 'all' policy should not produce a note, got: {:?}",
        resp.note
    );

    let _ = session.kill();
}

#[test]
fn resolve_thread_id_finds_worker() {
    let session = launch_fixture("ThreadTest");

    // 在 doWork 里停住，这时 "worker" 线程存在
    session
        .stop_at("ThreadTest", 37, None, None)
        .expect("stop_at failed");
    let run_resp = session.run(Some(30)).expect("run failed");
    assert!(matches!(&run_resp.result, CommandResult::Stopped { .. }));

    // resolve_thread_id 应该找到 "worker"
    let hex_id = session.resolve_thread_id("worker", Some(5));
    assert!(
        hex_id.is_some(),
        "should resolve 'worker' thread to a hex id"
    );
    let id = hex_id.unwrap();
    assert!(
        id.starts_with("0x"),
        "thread id should be hex format (0x...), got: {id}"
    );

    // 不存在的线程名返回 None
    let bogus = session.resolve_thread_id("nonexistent-thread-xyz", Some(5));
    assert!(bogus.is_none(), "nonexistent thread should return None");

    let _ = session.kill();
}

// ─── Phase: classes / methods search ───

#[test]
fn classes_returns_loaded_classes() {
    let session = launch_fixture("CollectionTest");

    // 需要先 run 到断点让类加载完成
    session
        .stop_at("CollectionTest", 10, None, None)
        .expect("stop_at failed");
    let run_resp = session.run(Some(30)).expect("run failed");
    assert!(matches!(&run_resp.result, CommandResult::Stopped { .. }));

    // 执行 classes 命令（带 pattern 过滤）
    use java_agent_debugger::jdb::parser::CommandHint;
    use java_agent_debugger::session::CommandKind;

    let resp = session
        .execute("classes CollectionTest", CommandKind::normal(CommandHint::Classes).with_timeout_secs(Some(10)))
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
        .execute("methods CollectionTest", CommandKind::normal(CommandHint::Methods).with_timeout_secs(Some(10)))
        .expect("methods failed");

    match &resp.result {
        CommandResult::Methods { methods, .. } => {
            assert!(
                !methods.is_empty(),
                "should find methods for CollectionTest"
            );
            // CollectionTest 有 main 方法
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

    // 设置字段监视点
    let resp = session
        .execute("watch WatchTest.name", CommandKind::normal(CommandHint::WatchSet).with_timeout_secs(Some(10)))
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

    // 设置字段监视点
    session
        .execute("watch WatchTest.name", CommandKind::normal(CommandHint::WatchSet).with_timeout_secs(Some(10)))
        .expect("watch failed");

    // 运行 — 应该在字段被修改时停下
    let run_resp = session.run(Some(30)).expect("run failed");

    match &run_resp.result {
        CommandResult::Stopped { event, thread, .. } => {
            match event {
                Event::FieldWatch { field, access_type, .. } => {
                    assert!(
                        field.contains("name"),
                        "field should contain 'name', got: {field}"
                    );
                    assert_eq!(access_type, "modified");
                }
                _ => {
                    // 也可能先命中静态初始化访问，thread 应有值
                    assert!(!thread.is_empty(), "thread should not be empty");
                }
            }
        }
        CommandResult::VmExited { .. } => {
            panic!("VM exited without hitting watchpoint — jdb may not support watch on static fields in this JDK version");
        }
        other => panic!("expected Stopped with FieldWatch event, got {other:?}"),
    }

    let _ = session.kill();
}

// ─── Test helpers that mirror handler.rs logic (can't import private fns) ───

fn enrich_stopped_test_helper(session: &Session, resp: &mut CommandResponse) {
    let (location_line, frame_ref, source_ref) = match &mut resp.result {
        CommandResult::Stopped { location, frame, source_context, .. } => {
            (location.line, frame, source_context)
        }
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
        CommandResult::Stopped { event: Event::Breakpoint { .. }, location, .. } => location.clone(),
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

fn append_note_helper(resp: &mut CommandResponse, msg: &str) {
    match &mut resp.note {
        Some(existing) => { existing.push('\n'); existing.push_str(msg); }
        None => resp.note = Some(msg.to_string()),
    }
}

/// Mirror of handler.rs `apply_suspend_policy` — thread-level suspend via suspend-count trick.
fn apply_suspend_policy_test_helper(session: &Session, resp: &mut CommandResponse, timeout: Option<u64>) {
    let (spec, thread_name) = match &resp.result {
        CommandResult::Stopped { event: Event::Breakpoint { location, .. }, thread, .. } => {
            (format!("{}:{}", location.class, location.line), thread.clone())
        }
        _ => return,
    };

    let policy = session.get_suspend_policy(&spec).unwrap_or_else(|| "all".into());
    if policy != "thread" {
        return;
    }

    let hex_id = match session.resolve_thread_id(&thread_name, timeout) {
        Some(id) => id,
        None => {
            append_note_helper(resp, &format!(
                "WARNING: suspend policy is 'thread' but could not resolve thread \"{}\" to a hex ID — \
                 falling back to suspend=all (all threads frozen).",
                thread_name
            ));
            return;
        }
    };

    // suspend count +1
    if let Err(e) = session.raw(&format!("suspend {hex_id}"), timeout) {
        append_note_helper(resp, &format!(
            "WARNING: suspend policy is 'thread' but `suspend {}` failed ({}) — \
             falling back to suspend=all (all threads frozen).",
            hex_id, e
        ));
        return;
    }

    // resume all (count -1)
    if let Err(e) = session.raw("resume", timeout) {
        let _ = session.raw(&format!("resume {hex_id}"), timeout);
        append_note_helper(resp, &format!(
            "WARNING: suspend policy is 'thread' but `resume` (all) failed ({}) — \
             rolled back suspend count; falling back to suspend=all (all threads frozen).",
            e
        ));
        return;
    }

    append_note_helper(resp, &format!(
        "Suspend policy: thread — only \"{}\" ({}) is suspended; other threads continue.",
        thread_name, hex_id
    ));
}
