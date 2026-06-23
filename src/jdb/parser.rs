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

/// 解析 `where` / `where all` 输出为 `StackTrace`。
pub fn parse_where(output: &str) -> CommandResult {
    let mut frames = Vec::new();
    for line in output.lines() {
        if let Some(c) = RE_FRAME.captures(line) {
            let index = c["idx"].parse().unwrap_or(0);
            let full_class = &c["class"];
            let method = c["method"].to_string();
            let loc = &c["loc"];

            // loc 格式: "File.java:42" 或 "native method" 或 "Unknown Source"
            let (file, line_num, is_native) = parse_location_parens(loc);

            frames.push(StackFrame {
                index,
                location: Location {
                    class: full_class.to_string(),
                    method,
                    file,
                    line: line_num,
                },
                is_native,
            });
        }
    }
    if frames.is_empty() {
        CommandResult::Raw { text: output.to_string() }
    } else {
        CommandResult::StackTrace { frames }
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
#[path = "parser_tests.rs"]
mod parser_tests;
