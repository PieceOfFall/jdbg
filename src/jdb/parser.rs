//! 将 reader 返回的原始文本 block 分类解析为 `CommandResult`。
//!
//! 这里是**正确性核心**——单元测试用真实 jdb transcript fixture 覆盖。
//! 正则契约来源：CLAUDE.md §5。
//!
//! 解析策略：
//! - `classify_output` 接收从 reader 拿到的文本 + 上下文命令类型，选择解析路径。
//! - 各子解析器（parse_locals, parse_where, parse_threads 等）逐行匹配生成结构化数据。
//! - 无法识别的文本兜底为 `CommandResult::Raw`。

use std::sync::LazyLock;

use regex::Regex;

use crate::protocol::*;

// ─── 正则（细粒度解析用，prompt/event 级已在 reader 中）─────────────────────────

/// `where` 输出中的栈帧行：`  [1] com.example.Main.method (Main.java:42)`
static RE_FRAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^\s*\[(?P<idx>\d+)\]\s+(?P<class>\S+)\.(?P<method>\S+)\s+\((?P<loc>[^)]+)\)",
    )
    .unwrap()
});

/// `locals` 输出中的变量行：`name = value`
/// 实际格式示例：`args = instance of java.lang.String[0] (id=430)`
/// 注意：jdb 不输出类型括号——它只有 `name = value`。
static RE_LOCAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?P<name>\S+)\s+=\s+(?P<value>.+)$").unwrap()
});

/// `print/eval` 输出：` <expr> = value` 或 `<expr> = <type> value`
static RE_PRINT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?P<expr>.+?)\s+=\s+(?P<value>.+)$").unwrap()
});

/// `threads` 输出中线程行的头部：`  (类名)0xID rest…`
/// 实际格式：`  (java.lang.Thread)0x1   main   running`——id 紧跟 `)`，无空格。
/// 括号内是线程对象的**类名**（非 group）；group 来自独立的 `Group xxx:` 行。
static RE_THREAD_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*\((?P<class>[^)]+)\)(?P<id>0x[0-9a-fA-F]+)\s+(?P<rest>.+?)\s*$").unwrap()
});

/// 从线程行 rest 尾部分离出状态（状态可能含空格，如 `cond. waiting`、`running (at breakpoint)`）。
static RE_THREAD_STATE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\s+(?P<state>(?:cond\. waiting|running|sleeping|waiting|zombie|unknown|not started|monitor).*)$",
    )
    .unwrap()
});

/// `Group <name>:` 分组头行。
static RE_THREAD_GROUP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*Group\s+(?P<group>\S+):").unwrap()
});

/// `list` 命令输出中的源代码行：`42    int x = 1;` 或 `42 =>  int x = 1;`
/// `=>` 标记当前执行行。捕获 marker 以便定位 around_line。
static RE_SOURCE_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?P<num>\d+)\s+(?P<marker>=>)?\s*(?P<text>.*?)\s*$").unwrap()
});

// ─── 命令上下文提示 ──────────────────────────────────────────────────────────────

/// 告诉 parser 当前 block 是对哪条命令的响应，以选择解析路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandHint {
    Where,
    WhereAll,
    Locals,
    Print,
    Dump,
    Eval,
    Threads,
    ListSource,
    Breakpoints,
    BreakpointSet,
    Run,
    Cont,
    Step,
    Next,
    StepOut,
    Other,
}

// ─── 公开 API ────────────────────────────────────────────────────────────────────

/// 将原始输出文本 + 命令 hint 解析为 `CommandResult`。
///
/// 返回 `(result, note)`——`note` 是可能的附加提示（如"请用 -g 编译"）。
pub fn classify_output(output: &str, hint: CommandHint) -> (CommandResult, Option<String>) {
    let note = if output.contains("Local variable information not available") {
        Some("Compile with `javac -g` to include local variable debug info.".into())
    } else {
        None
    };

    let result = match hint {
        CommandHint::Where => parse_where(output),
        CommandHint::WhereAll => parse_where_all(output),
        CommandHint::Locals => parse_locals(output),
        CommandHint::Print | CommandHint::Eval => parse_print(output),
        CommandHint::Dump => parse_dump(output),
        CommandHint::Threads => parse_threads(output),
        CommandHint::ListSource => parse_source(output),
        CommandHint::Breakpoints => parse_breakpoint_list(output),
        CommandHint::BreakpointSet => parse_breakpoint_set(output),
        _ => CommandResult::Raw { text: output.to_string() },
    };
    (result, note)
}

// ─── 子解析器 ────────────────────────────────────────────────────────────────────

/// 解析单条 `where` 栈帧行；非帧行返回 None。
fn parse_frame_line(line: &str) -> Option<StackFrame> {
    let c = RE_FRAME.captures(line)?;
    let index = c["idx"].parse().unwrap_or(0);
    let method = c["method"].to_string();
    // loc 格式: "File.java:42" 或 "native method" 或 "Unknown Source"
    let (file, line_num, is_native) = parse_location_parens(&c["loc"]);
    Some(StackFrame {
        index,
        location: Location {
            class: c["class"].to_string(),
            method,
            file,
            line: line_num,
        },
        is_native,
    })
}

/// 解析单线程 `where` 输出为 `StackTrace`。
pub fn parse_where(output: &str) -> CommandResult {
    let frames: Vec<StackFrame> = output.lines().filter_map(parse_frame_line).collect();
    if frames.is_empty() {
        CommandResult::Raw { text: output.to_string() }
    } else {
        CommandResult::StackTrace { frames }
    }
}

/// 解析 `where all` 多线程输出为 `ThreadStackTrace`。
///
/// 输出形如：每个线程一个 header 行（`main:`、`Reference Handler:`），其后为缩进的帧行；
/// 无帧的线程只有 header。线程名可能含空格，故用"以冒号结尾的非帧行"作为分组边界。
pub fn parse_where_all(output: &str) -> CommandResult {
    let mut threads: Vec<ThreadStack> = Vec::new();
    for line in output.lines() {
        if let Some(frame) = parse_frame_line(line) {
            match threads.last_mut() {
                Some(t) => t.frames.push(frame),
                // 帧出现在任何 header 之前——归入一个匿名线程兜底。
                None => threads.push(ThreadStack { thread: String::new(), frames: vec![frame] }),
            }
        } else if let Some(name) = line.trim().strip_suffix(':')
            && !name.is_empty()
        {
            threads.push(ThreadStack { thread: name.to_string(), frames: Vec::new() });
        }
    }
    if threads.is_empty() {
        CommandResult::Raw { text: output.to_string() }
    } else {
        CommandResult::ThreadStackTrace { threads }
    }
}

/// 解析 `locals` 输出为 `Locals`。
pub fn parse_locals(output: &str) -> CommandResult {
    let mut vars = Vec::new();
    for line in output.lines() {
        if let Some(c) = RE_LOCAL.captures(line) {
            vars.push(VarBinding {
                name: c["name"].to_string(),
                ty: c.name("ty").map(|m| m.as_str().to_string()),
                value: c["value"].to_string(),
            });
        }
    }
    if vars.is_empty() {
        CommandResult::Raw { text: output.to_string() }
    } else {
        CommandResult::Locals { vars }
    }
}

/// 解析 `print` / `eval` 输出为 `Value`。
pub fn parse_print(output: &str) -> CommandResult {
    // 第一行有意义内容通常是 ` expr = value`。
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(c) = RE_PRINT.captures(line) {
            return CommandResult::Value {
                expr: c["expr"].to_string(),
                value: c["value"].to_string(),
                ty: None,
            };
        }
    }
    CommandResult::Raw { text: output.to_string() }
}

/// 解析 `dump` 输出为 `ObjectDump`（字段列表）。
pub fn parse_dump(output: &str) -> CommandResult {
    // dump 输出第一行通常是 `expr = { ... }` 或多行字段。
    // 尝试提取表达式名和字段。
    let lines: Vec<&str> = output.lines().collect();
    if lines.is_empty() {
        return CommandResult::Raw { text: output.to_string() };
    }

    // 第一行: `expr = {` 或 `expr = value`
    let first = lines[0].trim();
    let expr = if let Some(pos) = first.find('=') {
        first[..pos].trim().to_string()
    } else {
        return CommandResult::Raw { text: output.to_string() };
    };

    let mut fields = Vec::new();
    for &line in &lines[1..] {
        let line = line.trim().trim_end_matches(',');
        if line == "}" || line.is_empty() {
            continue;
        }
        // 字段格式: `fieldName: value` 或 `fieldName (type): value`
        if let Some(pos) = line.find(':') {
            let name_part = line[..pos].trim();
            let value = line[pos + 1..].trim().to_string();
            // name_part 可能带 (type)
            let (name, ty) = if let Some(paren) = name_part.find('(') {
                let n = name_part[..paren].trim().to_string();
                let t = name_part[paren + 1..].trim_end_matches(')').to_string();
                (n, Some(t))
            } else {
                (name_part.to_string(), None)
            };
            fields.push(VarBinding { name, ty, value });
        }
    }

    if fields.is_empty() {
        // 可能是单行 dump，回退 Value 解析
        parse_print(output)
    } else {
        CommandResult::ObjectDump { expr, fields }
    }
}

/// 解析 `threads` 输出为 `Threads`。
///
/// 跟踪 `Group xxx:` 行确定当前分组；线程行的 class+id 用 `RE_THREAD_LINE` 提取，
/// state 用 `RE_THREAD_STATE` 从 rest 尾部分离（state 可能含空格），剩余为 name。
pub fn parse_threads(output: &str) -> CommandResult {
    let mut threads = Vec::new();
    let mut current_group: Option<String> = None;

    for line in output.lines() {
        if let Some(g) = RE_THREAD_GROUP.captures(line) {
            current_group = Some(g["group"].to_string());
            continue;
        }
        if let Some(c) = RE_THREAD_LINE.captures(line) {
            let id = c["id"].to_string();
            let rest = &c["rest"];
            // 从 rest 尾部分离 state；分不出则整个 rest 当 name、state 留空。
            let (name, state) = match RE_THREAD_STATE.find(rest) {
                Some(m) => (
                    rest[..m.start()].trim().to_string(),
                    rest[m.start()..].trim().to_string(),
                ),
                None => (rest.trim().to_string(), String::new()),
            };
            threads.push(ThreadInfo {
                id,
                name,
                group: current_group.clone(),
                state,
            });
        }
    }

    if threads.is_empty() {
        CommandResult::Raw { text: output.to_string() }
    } else {
        CommandResult::Threads { threads }
    }
}

/// 解析 `list` 命令输出为 `Source`。
pub fn parse_source(output: &str) -> CommandResult {
    let mut lines = Vec::new();
    let mut marker_line: Option<u32> = None;
    for text_line in output.lines() {
        if let Some(c) = RE_SOURCE_LINE.captures(text_line) {
            let num = c["num"].parse().unwrap_or(0);
            if c.name("marker").is_some() {
                marker_line = Some(num);
            }
            lines.push(SourceLine {
                number: num,
                text: c["text"].to_string(),
            });
        }
    }
    if lines.is_empty() {
        return CommandResult::Raw { text: output.to_string() };
    }
    // around_line: 优先用 `=>` 标记行，否则取中间行。
    let around_line = marker_line.unwrap_or_else(|| lines[lines.len() / 2].number);
    CommandResult::Source { around_line, lines }
}

/// 解析 `clear`(无参) / `stop`(无参) 输出为断点列表。
pub fn parse_breakpoint_list(output: &str) -> CommandResult {
    let breakpoints: Vec<String> = output
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    CommandResult::BreakpointList { breakpoints }
}

/// 解析 `stop at`/`stop in`/`catch` 的设置确认。
pub fn parse_breakpoint_set(output: &str) -> CommandResult {
    let text = output.trim();
    // 成功时 jdb 会输出如 "Set breakpoint com.example.Main:42" 或 "Deferring breakpoint ..."
    let deferred = text.contains("Deferring") || text.contains("deferred");
    let bp_kind = if text.contains("catch") || text.contains("Exception") {
        BreakpointKind::Catch
    } else if text.contains(':') {
        BreakpointKind::Line
    } else {
        BreakpointKind::Method
    };
    CommandResult::BreakpointSet {
        spec: text.to_string(),
        bp_kind,
        deferred,
    }
}

// ─── 工具函数 ────────────────────────────────────────────────────────────────────

/// 解析栈帧括号内容 `(File.java:42)` → (file, line, is_native)。
fn parse_location_parens(loc: &str) -> (Option<String>, u32, bool) {
    if loc.contains("native method") || loc.contains("Native Method") {
        return (None, 0, true);
    }
    if loc.contains("Unknown Source") {
        return (None, 0, false);
    }
    // 格式: "File.java:42" 或 "bci=N"
    if let Some(colon) = loc.rfind(':') {
        let file = &loc[..colon];
        let line = loc[colon + 1..].parse().unwrap_or(0);
        (Some(file.to_string()), line, false)
    } else {
        (Some(loc.to_string()), 0, false)
    }
}

#[cfg(test)]
mod tests {
    //! Parser TDD 测试——用真实 jdb transcript fixture 验证解析器正确性。
    use super::*;
    use crate::protocol::{BreakpointKind, CommandResult};

    // ─── fixture 文本（从 tests/fixtures/jdb/ 真实捕获，locale-forced）────────────

    const LOCALS_SPARSE: &str = "\
Method arguments:
args = instance of java.lang.String[0] (id=430)
Local variables:";

    const LOCALS_FULL: &str = "\
Method arguments:
args = instance of java.lang.String[0] (id=430)
Local variables:
count = 3
label = \"hello\"
sum = 3";

    const WHERE_SINGLE: &str = "  [1] Main.main (Main.java:9)";

    const WHERE_ALL: &str = "\
Reference Handler:
  [1] java.lang.Object.wait (native method)
  [2] java.lang.ref.Reference.tryHandlePending (Reference.java:191)
Signal Dispatcher:
main:
  [1] Main.foo (Main.java:15)
  [2] Main.main (Main.java:9)";

    const PRINT_INT: &str = " count = 3";

    const PRINT_STR: &str = " label = \"hello\"";

    const THREADS: &str = "\
Group system:
  (java.lang.ref.Reference$ReferenceHandler)0x181 Reference Handler cond. waiting
  (java.lang.ref.Finalizer$FinalizerThread)0x180  Finalizer         cond. waiting
  (java.lang.Thread)0x17f                         Signal Dispatcher running
  (java.lang.Thread)0x17e                         Attach Listener   running
Group main:
  (java.lang.Thread)0x1                           main              running (at breakpoint)";

    const LIST_SOURCE: &str = "\
5            int sum = 0;
6            for (int i = 0; i < count; i++) {
7                sum += i;
8            }
9 =>         System.out.println(label + \" sum=\" + sum);
10        }
11    }";

    const BP_SET_METHOD: &str = "\
Deferring breakpoint Main.main.
It will be set after the class is loaded.";

    const BP_SET_LINE: &str = "\
Deferring breakpoint Main:9.
It will be set after the class is loaded.";

    // ─── Tests ──────────────────────────────────────────────────────────────────

    #[test]
    fn locals_full_parses_all_bindings() {
        let result = parse_locals(LOCALS_FULL);
        let CommandResult::Locals { vars } = result else {
            panic!("expected Locals, got {result:?}");
        };
        // args(方法参数) + count + label + sum
        assert_eq!(vars.len(), 4, "vars: {vars:?}");
        assert_eq!(vars[0].name, "args");
        assert_eq!(vars[1].name, "count");
        assert_eq!(vars[1].value, "3");
        assert_eq!(vars[2].name, "label");
        assert_eq!(vars[2].value, "\"hello\"");
        assert_eq!(vars[3].name, "sum");
        assert_eq!(vars[3].value, "3");
    }

    #[test]
    fn locals_sparse_only_args() {
        let result = parse_locals(LOCALS_SPARSE);
        let CommandResult::Locals { vars } = result else {
            panic!("expected Locals, got {result:?}");
        };
        assert_eq!(vars.len(), 1);
        assert_eq!(vars[0].name, "args");
    }

    #[test]
    fn where_single_frame() {
        let result = parse_where(WHERE_SINGLE);
        let CommandResult::StackTrace { frames } = result else {
            panic!("expected StackTrace, got {result:?}");
        };
        assert_eq!(frames.len(), 1);
        let f = &frames[0];
        assert_eq!(f.index, 1);
        assert_eq!(f.location.class, "Main");
        assert_eq!(f.location.method, "main");
        assert_eq!(f.location.file.as_deref(), Some("Main.java"));
        assert_eq!(f.location.line, 9);
        assert!(!f.is_native);
    }

    #[test]
    fn where_all_groups_by_thread() {
        let result = parse_where_all(WHERE_ALL);
        let CommandResult::ThreadStackTrace { threads } = result else {
            panic!("expected ThreadStackTrace, got {result:?}");
        };
        assert_eq!(threads.len(), 3, "threads: {threads:?}");

        // 含 native 帧的线程
        assert_eq!(threads[0].thread, "Reference Handler");
        assert_eq!(threads[0].frames.len(), 2);
        assert!(threads[0].frames[0].is_native);
        assert_eq!(threads[0].frames[1].location.line, 191);

        // 无帧线程仍保留
        assert_eq!(threads[1].thread, "Signal Dispatcher");
        assert!(threads[1].frames.is_empty());

        // 主线程（线程名为 "main"，注意不要与帧行混淆）
        assert_eq!(threads[2].thread, "main");
        assert_eq!(threads[2].frames.len(), 2);
        assert_eq!(threads[2].frames[0].location.method, "foo");
        assert_eq!(threads[2].frames[0].location.line, 15);
    }

    #[test]
    fn print_int_value() {
        let result = parse_print(PRINT_INT);
        let CommandResult::Value { expr, value, .. } = result else {
            panic!("expected Value, got {result:?}");
        };
        assert_eq!(expr, "count");
        assert_eq!(value, "3");
    }

    #[test]
    fn print_string_value() {
        let result = parse_print(PRINT_STR);
        let CommandResult::Value { expr, value, .. } = result else {
            panic!("expected Value, got {result:?}");
        };
        assert_eq!(expr, "label");
        assert_eq!(value, "\"hello\"");
    }

    #[test]
    fn threads_with_groups() {
        let result = parse_threads(THREADS);
        let CommandResult::Threads { threads } = result else {
            panic!("expected Threads, got {result:?}");
        };
        assert_eq!(threads.len(), 5, "threads: {threads:?}");

        let by_name = |n: &str| threads.iter().find(|t| t.name == n).unwrap();

        let rh = by_name("Reference Handler");
        assert_eq!(rh.id, "0x181");
        assert_eq!(rh.group.as_deref(), Some("system"));
        assert_eq!(rh.state, "cond. waiting");

        let sd = by_name("Signal Dispatcher");
        assert_eq!(sd.id, "0x17f");
        assert_eq!(sd.group.as_deref(), Some("system"));
        assert_eq!(sd.state, "running");

        let main = by_name("main");
        assert_eq!(main.id, "0x1");
        assert_eq!(main.group.as_deref(), Some("main"));
        assert_eq!(main.state, "running (at breakpoint)");
    }

    #[test]
    fn source_with_current_line_marker() {
        let result = parse_source(LIST_SOURCE);
        let CommandResult::Source { around_line, lines } = result else {
            panic!("expected Source, got {result:?}");
        };
        // `=>` 标记的当前执行行是 9
        assert_eq!(around_line, 9);
        assert_eq!(lines.len(), 7);
        assert_eq!(lines[0].number, 5);
        assert_eq!(lines[0].text, "int sum = 0;");
        let line9 = lines.iter().find(|l| l.number == 9).unwrap();
        assert_eq!(line9.text, "System.out.println(label + \" sum=\" + sum);");
    }

    #[test]
    fn breakpoint_set_method_deferred() {
        let result = parse_breakpoint_set(BP_SET_METHOD);
        let CommandResult::BreakpointSet { bp_kind, deferred, .. } = result else {
            panic!("expected BreakpointSet, got {result:?}");
        };
        assert_eq!(bp_kind, BreakpointKind::Method);
        assert!(deferred);
    }

    #[test]
    fn breakpoint_set_line_deferred() {
        let result = parse_breakpoint_set(BP_SET_LINE);
        let CommandResult::BreakpointSet { bp_kind, deferred, .. } = result else {
            panic!("expected BreakpointSet, got {result:?}");
        };
        assert_eq!(bp_kind, BreakpointKind::Line);
        assert!(deferred);
    }
}
