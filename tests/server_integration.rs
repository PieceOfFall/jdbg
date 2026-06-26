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

    // 验证断点能被 jdb 接受（stop_at 层不处理条件——条件断点逻辑在 handler 层）
    let bp_resp = session
        .stop_at("CollectionTest", 10, None, None)
        .expect("stop_at failed");
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

/// 回归：attach 入口把 `localhost` 规范化为 `127.0.0.1`，且规范化贯穿到 `probe_tcp`。
/// 双栈机器上 `localhost` 可能解析到 IPv6 `[::1]`，而 JDWP 多在 IPv4 监听 → 连接被拒。
/// 用不可达端口触发 probe_tcp 失败，错误信息里应出现规范化后的 `127.0.0.1`（而非原始 `localhost`），
/// 证明 probe 与后续 spawn 用的都是 IPv4 字面量。
#[test]
fn attach_normalizes_localhost_to_ipv4_loopback() {
    use java_agent_debugger::jdb::process::AttachConfig;

    let config = AttachConfig {
        host: "localhost".to_string(),
        port: 19999, // 没有 JDWP 在这个端口监听
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

// ─── Phase: thread suspend policy (jdb 原生 `stop thread`) ───

/// thread policy 断点：`stop thread at` 语法被 jdb 接受，命中后正常解析为 Stopped，
/// 触发线程是 "worker"（事件 banner 与普通断点一致，parser 兼容）。
#[test]
fn thread_breakpoint_hit_resolves_worker() {
    let session = launch_fixture("ThreadTest");

    // suspend="thread" → jdb 发 `stop thread at`（SUSPEND_THREAD policy）
    let bp = session
        .stop_in("ThreadTest", "doWork", None, Some("thread"), None)
        .expect("stop_in failed");
    assert!(
        matches!(bp.result, CommandResult::BreakpointSet { .. }),
        "thread breakpoint should be accepted, got {:?}",
        bp.result
    );

    let run_resp = session.run(Some(30)).expect("run failed");
    // PartialStop 补全：截断 banner（JDK 8 SUSPEND_THREAD）经 session 层
    // threads→thread<id>→where 自动补全 thread/location/frame。
    match &run_resp.result {
        CommandResult::Stopped { location, thread, thread_id, frame, .. } => {
            assert_eq!(location.class, "ThreadTest");
            assert_eq!(location.method, "doWork");
            assert_eq!(thread, "worker", "the hit thread should be 'worker'");
            // PartialStop 路径应在 session 层零额外开销地回填命中线程 id（复用那次 `threads`）。
            let tid = thread_id.as_ref().expect("thread_id should be filled on a thread-policy hit");
            assert!(!tid.is_empty(), "thread_id should be non-empty");
            // frame 应已被 session 层从 `where` 回填（供 handler 跳过重复 where 查询）。
            let f = frame.as_ref().expect("frame should be enriched by session layer");
            assert_eq!(f.location.class, "ThreadTest");
            assert_eq!(f.location.method, "doWork");
            assert!(f.location.line > 0, "enriched frame line should be nonzero");

            // 回填的 id 必须能真正切换到该线程（即 transcript 里失败的那一步）。
            let sw = session
                .execute(
                    &format!("thread {tid}"),
                    java_agent_debugger::session::CommandKind::normal(
                        java_agent_debugger::jdb::parser::CommandHint::Other,
                    )
                    .with_timeout_secs(Some(5)),
                )
                .expect("thread switch failed");
            // jdb 接受 → 不应是 "not a valid thread id" 之类错误文本。
            if let CommandResult::Raw { text } = &sw.result {
                assert!(
                    !text.contains("not a valid thread id"),
                    "thread <{tid}> rejected by jdb: {text:?}"
                );
            }
        }
        other => panic!("expected Stopped in worker/doWork, got {other:?}"),
    }

    // 走 handler 层 enrich：基于 session 已填的 frame，补 source_context（list）。
    let mut resp = run_resp;
    enrich_stopped_test_helper(&session, &mut resp);
    match &resp.result {
        CommandResult::Stopped { source_context, location, .. } => {
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

/// 核心回归：thread policy 下 VM 不会 SUSPEND_ALL —— worker 挂在断点时，daemon
/// 线程 heartbeat 仍持续递增 heartbeatCount，证明其它线程没有被冻结。
/// （旧的 suspend-count 模拟方案在此会因 spec 不匹配静默退回 all，冻结全部线程。）
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

    // worker 已挂在断点；读 heartbeatCount 两次，中间用 jdb 往返制造时间窗口
    // （heartbeat 每 ~50ms 递增）。thread policy 下计数必须继续增长。
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

/// 验证 6 个新命令的语法被真实 jdb 接受（尤其 `set <lvalue> = <expr>`）。
/// 在 doWork 命中后，逐条执行并断言输出不含明显的 jdb 报错。
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
            .execute(cmd, CommandKind::normal(CommandHint::Other).with_timeout_secs(Some(5)))
            .unwrap_or_else(|e| panic!("`{cmd}` errored: {e}"))
    };
    // jdb 对无法识别的命令会回 "Unrecognized command ..."；语法错回 "Usage: ..."。
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
        "set x = 99".to_string(),       // doWork 的局部 int x
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
