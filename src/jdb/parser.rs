//! Classify and parse raw text blocks returned by the reader into `CommandResult`.
//!
//! This is the **correctness core**. Unit tests cover it with real jdb transcript fixtures.
//! Regex contracts come from CLAUDE.md §5.
//!
//! Parsing strategy:
//! - `classify_output` receives reader text plus the contextual command type and selects a parse path.
//! - Sub-parsers (`parse_locals`, `parse_where`, `parse_threads`, etc.) match line by line into structured data.
//! - Unrecognized text falls back to `CommandResult::Raw`.

use std::sync::LazyLock;

use regex::Regex;

use crate::protocol::*;

// ─── Regexes (fine-grained parsing; prompt/event parsing lives in reader) ──────

/// Stack-frame line in `where` output: `  [1] com.example.Main.method (Main.java:42)`
static RE_FRAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*\[(?P<idx>\d+)\]\s+(?P<class>\S+)\.(?P<method>\S+)\s+\((?P<loc>[^)]+)\)")
        .unwrap()
});

/// Variable line in `locals` output: `name = value`
/// Real example: `args = instance of java.lang.String[0] (id=430)`
/// Note: jdb does not emit type parentheses; it only emits `name = value`.
static RE_LOCAL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?P<name>\S+)\s+=\s+(?P<value>.+)$").unwrap());

/// `print/eval` output: ` <expr> = value` or `<expr> = <type> value`
static RE_PRINT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(?P<expr>.+?)\s+=\s+(?P<value>.+)$").unwrap());

/// Prefix of a thread line in `threads` output: `  (className)ID rest...`
/// Real format: `  (java.lang.Thread)0x1   main   running`; id immediately follows `)` with no space.
/// The parentheses contain the thread object's **class name**, not the group; groups come from separate
/// `Group xxx:` lines. IDs are usually `0x`-prefixed hex, but some JDK jdb builds (for example external
/// Tomcat attach) print plain decimal (`18315`). Capture both verbatim and pass them back to `thread <id>`;
/// the hex branch comes first so `0x...` is not truncated by the decimal branch.
static RE_THREAD_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*\((?P<class>[^)]+)\)(?P<id>0x[0-9a-fA-F]+|\d+)\s+(?P<rest>.+?)\s*$").unwrap()
});

/// Split state from the tail of a thread-line rest; state may contain spaces such as `cond. waiting`.
static RE_THREAD_STATE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\s+(?P<state>(?:cond\. waiting|running|sleeping|waiting|zombie|unknown|not started|monitor).*)$",
    )
    .unwrap()
});

/// `Group <name>:` group header line.
static RE_THREAD_GROUP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*Group\s+(?P<group>\S+):").unwrap());

/// Source line in `list` output: `42    int x = 1;` or `42 =>  int x = 1;`
/// In en_US locale, line numbers may contain thousands separators: `3,956    int x = 1;`
/// `=>` marks the current execution line. Capture the marker to identify around_line.
static RE_SOURCE_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?P<num>[\d,]+)\s+(?P<marker>=>)?\s*(?P<text>.*?)\s*$").unwrap()
});

// ─── Command Context Hints ─────────────────────────────────────────────────────

/// Tells the parser which command produced the current block so it can choose a parse path.
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
    Classes,
    Methods,
    WatchSet,
    Run,
    Cont,
    Step,
    Next,
    StepOut,
    Other,
}

// ─── Public API ────────────────────────────────────────────────────────────────

/// Parse raw output text plus a command hint into `CommandResult`.
///
/// Returns `(result, note)`, where `note` is an optional extra hint such as "compile with -g".
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
        CommandHint::Classes => parse_classes(output),
        CommandHint::Methods => parse_methods(output),
        CommandHint::WatchSet => parse_watch_set(output),
        _ => CommandResult::Raw {
            text: output.to_string(),
        },
    };
    (result, note)
}

// ─── Sub-parsers ───────────────────────────────────────────────────────────────

/// Parse a single `where` stack-frame line; non-frame lines return None.
fn parse_frame_line(line: &str) -> Option<StackFrame> {
    let c = RE_FRAME.captures(line)?;
    let index = c["idx"].parse().unwrap_or(0);
    let method = c["method"].to_string();
    // loc format: "File.java:42", "native method", or "Unknown Source".
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

/// Parse single-thread `where` output into `StackTrace`.
pub fn parse_where(output: &str) -> CommandResult {
    let frames: Vec<StackFrame> = output.lines().filter_map(parse_frame_line).collect();
    if frames.is_empty() {
        CommandResult::Raw {
            text: output.to_string(),
        }
    } else {
        CommandResult::StackTrace { frames }
    }
}

/// Parse multi-thread `where all` output into `ThreadStackTrace`.
///
/// Output has one header line per thread (`main:`, `Reference Handler:`), followed by indented frame lines.
/// Threads without frames have only the header. Thread names may contain spaces, so a non-frame line ending
/// in a colon is used as the group boundary.
pub fn parse_where_all(output: &str) -> CommandResult {
    let mut threads: Vec<ThreadStack> = Vec::new();
    for line in output.lines() {
        if let Some(frame) = parse_frame_line(line) {
            match threads.last_mut() {
                Some(t) => t.frames.push(frame),
                // A frame appeared before any header; put it in an anonymous fallback thread.
                None => threads.push(ThreadStack {
                    thread: String::new(),
                    frames: vec![frame],
                }),
            }
        } else if let Some(name) = line.trim().strip_suffix(':')
            && !name.is_empty()
        {
            threads.push(ThreadStack {
                thread: name.to_string(),
                frames: Vec::new(),
            });
        }
    }
    if threads.is_empty() {
        CommandResult::Raw {
            text: output.to_string(),
        }
    } else {
        CommandResult::ThreadStackTrace { threads }
    }
}

/// Parse `locals` output into `Locals`.
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
        CommandResult::Raw {
            text: output.to_string(),
        }
    } else {
        CommandResult::Locals { vars }
    }
}

/// Parse `print` / `eval` output into `Value`.
pub fn parse_print(output: &str) -> CommandResult {
    // The first meaningful line is usually ` expr = value`.
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
    CommandResult::Raw {
        text: output.to_string(),
    }
}

/// Parse `dump` output into `ObjectDump` field list.
pub fn parse_dump(output: &str) -> CommandResult {
    // The first dump line is usually `expr = { ... }` or a multi-line field listing.
    // Try to extract the expression name and fields.
    let lines: Vec<&str> = output.lines().collect();
    if lines.is_empty() {
        return CommandResult::Raw {
            text: output.to_string(),
        };
    }

    // First line: `expr = {` or `expr = value`.
    let first = lines[0].trim();
    let expr = if let Some(pos) = first.find('=') {
        first[..pos].trim().to_string()
    } else {
        return CommandResult::Raw {
            text: output.to_string(),
        };
    };

    let mut fields = Vec::new();
    for &line in &lines[1..] {
        let line = line.trim().trim_end_matches(',');
        if line == "}" || line.is_empty() {
            continue;
        }
        // Field format: `fieldName: value` or `fieldName (type): value`.
        if let Some(pos) = line.find(':') {
            let name_part = line[..pos].trim();
            let value = line[pos + 1..].trim().to_string();
            // name_part may include (type).
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
        // May be a single-line dump; fall back to Value parsing.
        parse_print(output)
    } else {
        CommandResult::ObjectDump { expr, fields }
    }
}

/// Parse `threads` output into `Threads`.
///
/// Track `Group xxx:` lines to determine the current group. Extract class+id from thread lines with
/// `RE_THREAD_LINE`, split state from the tail of rest with `RE_THREAD_STATE` (state may contain spaces),
/// and treat the remainder as the name.
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
            // Split state from the tail of rest; if it cannot be split, use all rest as name and leave state empty.
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
        CommandResult::Raw {
            text: output.to_string(),
        }
    } else {
        CommandResult::Threads { threads }
    }
}

/// Parse `list` command output into `Source`.
pub fn parse_source(output: &str) -> CommandResult {
    let mut lines = Vec::new();
    let mut marker_line: Option<u32> = None;
    for text_line in output.lines() {
        if let Some(c) = RE_SOURCE_LINE.captures(text_line) {
            let num = c["num"].replace(',', "").parse().unwrap_or(0);
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
        return CommandResult::Raw {
            text: output.to_string(),
        };
    }
    // around_line: prefer the `=>` marked line, otherwise use the middle line.
    let around_line = marker_line.unwrap_or_else(|| lines[lines.len() / 2].number);
    CommandResult::Source { around_line, lines }
}

/// Parse `clear` with no args / `stop` with no args into a breakpoint list.
pub fn parse_breakpoint_list(output: &str) -> CommandResult {
    let breakpoints: Vec<String> = output
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    CommandResult::BreakpointList { breakpoints }
}

/// Parse setup confirmation from `stop at`/`stop in`/`catch`.
pub fn parse_breakpoint_set(output: &str) -> CommandResult {
    let text = output.trim();
    // On success, jdb emits lines like "Set breakpoint com.example.Main:42" or "Deferring breakpoint ...".
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

// ─── classes / methods / watch parsers ────────────────────────────────────────

/// Parse `classes [pattern]` output, one fully qualified class name per line.
pub fn parse_classes(output: &str) -> CommandResult {
    let classes: Vec<String> = output
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with("**"))
        .map(|l| l.to_string())
        .collect();
    CommandResult::Classes { classes }
}

/// Parse `methods <class>` output, one method signature per line.
pub fn parse_methods(output: &str) -> CommandResult {
    let methods: Vec<String> = output
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with("**"))
        .map(|l| l.to_string())
        .collect();
    CommandResult::Methods {
        class: String::new(),
        methods,
    }
}

/// Parse `watch` setup confirmation.
pub fn parse_watch_set(output: &str) -> CommandResult {
    let text = output.trim();
    let deferred = text.contains("Deferring") || text.contains("deferred");
    let mode = if text.contains("access") && text.contains("modification") {
        "all".to_string()
    } else if text.contains("access") {
        "access".to_string()
    } else {
        "modification".to_string()
    };
    CommandResult::WatchSet {
        spec: text.to_string(),
        mode,
        deferred,
    }
}

// ─── Utility Functions ────────────────────────────────────────────────────────

/// Parse stack-frame parentheses `(File.java:42)` into (file, line, is_native).
/// Note: in en_US locale, jdb may print thousands separators for line numbers >=1000, e.g. `File.java:3,956`.
fn parse_location_parens(loc: &str) -> (Option<String>, u32, bool) {
    if loc.contains("native method") || loc.contains("Native Method") {
        return (None, 0, true);
    }
    if loc.contains("Unknown Source") {
        return (None, 0, false);
    }
    // Format: "File.java:42", "File.java:3,956", or "bci=N".
    if let Some(colon) = loc.rfind(':') {
        let file = &loc[..colon];
        let line = loc[colon + 1..].replace(',', "").parse().unwrap_or(0);
        (Some(file.to_string()), line, false)
    } else {
        (Some(loc.to_string()), 0, false)
    }
}

#[cfg(test)]
mod tests {
    //! Parser TDD tests: verify parser correctness with real jdb transcript fixtures.
    use super::*;
    use crate::protocol::{BreakpointKind, CommandResult};

    // ─── Fixture Text (real captures from tests/fixtures/jdb/, locale-forced) ─

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

    // Some JDK jdb builds print thread ids as decimal without a 0x prefix, e.g. OpenJDK/Azul in external Tomcat attach.
    const THREADS_DECIMAL: &str = "\
Group system:
  (java.lang.Thread)18247                          Signal Dispatcher running
Group main:
  (org.apache.tomcat.util.threads.TaskThread)18315 http-nio-9702-exec-1 running (at breakpoint)
  (org.apache.tomcat.util.threads.TaskThread)18316 http-nio-9702-exec-2 cond. waiting";

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
        // args (method parameter) + count + label + sum.
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

        // Thread with a native frame.
        assert_eq!(threads[0].thread, "Reference Handler");
        assert_eq!(threads[0].frames.len(), 2);
        assert!(threads[0].frames[0].is_native);
        assert_eq!(threads[0].frames[1].location.line, 191);

        // Threads without frames are still kept.
        assert_eq!(threads[1].thread, "Signal Dispatcher");
        assert!(threads[1].frames.is_empty());

        // Main thread; the thread name is "main", and must not be confused with frame lines.
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
    fn threads_with_decimal_ids() {
        let result = parse_threads(THREADS_DECIMAL);
        let CommandResult::Threads { threads } = result else {
            panic!("expected Threads, got {result:?}");
        };
        assert_eq!(threads.len(), 3, "threads: {threads:?}");

        let by_name = |n: &str| threads.iter().find(|t| t.name == n).unwrap();

        let exec1 = by_name("http-nio-9702-exec-1");
        assert_eq!(exec1.id, "18315");
        assert_eq!(exec1.group.as_deref(), Some("main"));
        assert_eq!(exec1.state, "running (at breakpoint)");

        let exec2 = by_name("http-nio-9702-exec-2");
        assert_eq!(exec2.id, "18316");
        assert_eq!(exec2.state, "cond. waiting");
    }

    #[test]
    fn source_with_current_line_marker() {
        let result = parse_source(LIST_SOURCE);
        let CommandResult::Source { around_line, lines } = result else {
            panic!("expected Source, got {result:?}");
        };
        // The `=>` marker indicates that the current execution line is 9.
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
        let CommandResult::BreakpointSet {
            bp_kind, deferred, ..
        } = result
        else {
            panic!("expected BreakpointSet, got {result:?}");
        };
        assert_eq!(bp_kind, BreakpointKind::Method);
        assert!(deferred);
    }

    #[test]
    fn breakpoint_set_line_deferred() {
        let result = parse_breakpoint_set(BP_SET_LINE);
        let CommandResult::BreakpointSet {
            bp_kind, deferred, ..
        } = result
        else {
            panic!("expected BreakpointSet, got {result:?}");
        };
        assert_eq!(bp_kind, BreakpointKind::Line);
        assert!(deferred);
    }

    // ─── classes / methods / watch tests ─────────────────────────────────────

    const CLASSES_OUTPUT: &str = "\
** classes list **
java.lang.Object
java.lang.String
com.example.Service
com.example.Service$$EnhancerBySpringCGLIB$$abc123
java.util.ArrayList";

    const CLASSES_FILTERED: &str = "\
com.example.Service
com.example.Service$$EnhancerBySpringCGLIB$$abc123";

    const METHODS_OUTPUT: &str = "\
** methods list **
java.lang.String <init>(byte[], int, int)
java.lang.String charAt(int)
java.lang.String length()
java.lang.String toString()";

    const WATCH_SET_MODIFICATION: &str = "Set watch modification of com.example.Service.name";
    const WATCH_SET_ACCESS: &str = "Set watch access of com.example.Service.name";
    const WATCH_SET_ALL: &str = "Set watch all access and modification of com.example.Service.name";
    const WATCH_SET_DEFERRED: &str = "\
Deferring watch modification of com.example.Service.name.
It will be set after the class is loaded.";

    #[test]
    fn classes_parses_with_header() {
        let result = parse_classes(CLASSES_OUTPUT);
        let CommandResult::Classes { classes } = result else {
            panic!("expected Classes, got {result:?}");
        };
        assert_eq!(classes.len(), 5);
        assert_eq!(classes[0], "java.lang.Object");
        assert_eq!(
            classes[3],
            "com.example.Service$$EnhancerBySpringCGLIB$$abc123"
        );
    }

    #[test]
    fn classes_parses_filtered() {
        let result = parse_classes(CLASSES_FILTERED);
        let CommandResult::Classes { classes } = result else {
            panic!("expected Classes, got {result:?}");
        };
        assert_eq!(classes.len(), 2);
        assert!(classes[1].contains("CGLIB"));
    }

    #[test]
    fn classes_empty_returns_empty_vec() {
        let result = parse_classes("");
        let CommandResult::Classes { classes } = result else {
            panic!("expected Classes, got {result:?}");
        };
        assert!(classes.is_empty());
    }

    #[test]
    fn methods_parses_with_header() {
        let result = parse_methods(METHODS_OUTPUT);
        let CommandResult::Methods { methods, .. } = result else {
            panic!("expected Methods, got {result:?}");
        };
        assert_eq!(methods.len(), 4);
        assert!(methods[0].contains("<init>"));
        assert!(methods[2].contains("length()"));
    }

    #[test]
    fn methods_empty_returns_empty_vec() {
        let result = parse_methods("");
        let CommandResult::Methods { methods, .. } = result else {
            panic!("expected Methods, got {result:?}");
        };
        assert!(methods.is_empty());
    }

    #[test]
    fn watch_set_modification() {
        let result = parse_watch_set(WATCH_SET_MODIFICATION);
        let CommandResult::WatchSet { mode, deferred, .. } = result else {
            panic!("expected WatchSet, got {result:?}");
        };
        assert_eq!(mode, "modification");
        assert!(!deferred);
    }

    #[test]
    fn watch_set_access() {
        let result = parse_watch_set(WATCH_SET_ACCESS);
        let CommandResult::WatchSet { mode, deferred, .. } = result else {
            panic!("expected WatchSet, got {result:?}");
        };
        assert_eq!(mode, "access");
        assert!(!deferred);
    }

    #[test]
    fn watch_set_all() {
        let result = parse_watch_set(WATCH_SET_ALL);
        let CommandResult::WatchSet { mode, deferred, .. } = result else {
            panic!("expected WatchSet, got {result:?}");
        };
        assert_eq!(mode, "all");
        assert!(!deferred);
    }

    #[test]
    fn watch_set_deferred() {
        let result = parse_watch_set(WATCH_SET_DEFERRED);
        let CommandResult::WatchSet { mode, deferred, .. } = result else {
            panic!("expected WatchSet, got {result:?}");
        };
        assert_eq!(mode, "modification");
        assert!(deferred);
    }
}
