//! Parser TDD 测试——用真实 jdb transcript fixture 验证解析器正确性。

#[cfg(test)]
mod tests {
    use crate::jdb::parser::*;
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
