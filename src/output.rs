//! 输出渲染：把 `CommandResponse` 渲染为人类可读文本（默认）或 JSON（`--json`）。

use crate::protocol::*;

/// 渲染完整响应。
pub fn render(resp: &CommandResponse, json: bool) -> String {
    if json {
        return serde_json::to_string_pretty(resp).unwrap_or_else(|_| format!("{resp:?}"));
    }
    let mut out = render_result(&resp.result);
    if let Some(note) = &resp.note {
        out.push_str(&format!("\n[note] {note}"));
    }
    out
}

/// 渲染 CommandResult 为人类可读文本。
fn render_result(result: &CommandResult) -> String {
    match result {
        CommandResult::SessionCreated { session, mode, target, state } => {
            format!("Session {session} created ({mode:?} {target}), state: {state:?}")
        }
        CommandResult::SessionList { sessions } => {
            if sessions.is_empty() {
                return "No active sessions.".into();
            }
            let mut lines = vec![format!("{:<10} {:<8} {:<20} {:<10} {}", "ID", "MODE", "TARGET", "STATE", "PID")];
            for s in sessions {
                lines.push(format!(
                    "{:<10} {:<8} {:<20} {:<10} {}",
                    s.id,
                    format!("{:?}", s.mode).to_lowercase(),
                    s.target,
                    format!("{:?}", s.state).to_lowercase(),
                    s.jdb_pid.map(|p| p.to_string()).unwrap_or_default(),
                ));
            }
            lines.join("\n")
        }
        CommandResult::Status { session, state, last_event, jdb_alive } => {
            let evt = last_event.as_ref()
                .map(|e| format!("{e:?}"))
                .unwrap_or_else(|| "none".into());
            format!("Session {session}: state={state:?} jdb_alive={jdb_alive} last_event={evt}")
        }
        CommandResult::BreakpointSet { spec, bp_kind, deferred } => {
            let d = if *deferred { " (deferred)" } else { "" };
            format!("Breakpoint set ({bp_kind:?}){d}: {spec}")
        }
        CommandResult::BreakpointList { breakpoints } => {
            if breakpoints.is_empty() {
                "No breakpoints set.".into()
            } else {
                breakpoints.join("\n")
            }
        }
        CommandResult::Stopped { event, location, thread, source_context, .. } => {
            let kind = match event {
                Event::Breakpoint { .. } => "Breakpoint hit",
                Event::Step { .. } => "Step completed",
                _ => "Stopped",
            };
            let mut out = format!(
                "{kind}: {}.{}() line={} thread={thread}",
                location.class, location.method, location.line
            );
            if let Some(lines) = source_context {
                for l in lines {
                    let marker = if l.number == location.line { "=>" } else { "  " };
                    out.push_str(&format!("\n{marker} {:>4}  {}", l.number, l.text));
                }
            }
            out
        }
        CommandResult::ExceptionCaught { exception, caught, location, thread } => {
            let mode = if *caught { "caught" } else { "uncaught" };
            format!(
                "Exception ({mode}): {exception} at {}.{}() line={} thread={thread}",
                location.class, location.method, location.line
            )
        }
        CommandResult::VmExited { exit_code, .. } => {
            match exit_code {
                Some(code) => format!("The application exited with code {code}"),
                None => "The application exited".into(),
            }
        }
        CommandResult::Timeout { partial_output, state } => {
            format!("TIMEOUT (state={state:?}). Partial output:\n{partial_output}")
        }
        CommandResult::StackTrace { frames } => {
            frames.iter().map(|f| {
                let loc = &f.location;
                let file = loc.file.as_deref().unwrap_or("?");
                if f.is_native {
                    format!("  [{}] {}.{} (native)", f.index, loc.class, loc.method)
                } else {
                    format!("  [{}] {}.{} ({file}:{})", f.index, loc.class, loc.method, loc.line)
                }
            }).collect::<Vec<_>>().join("\n")
        }
        CommandResult::ThreadStackTrace { threads } => {
            let mut lines = Vec::new();
            for ts in threads {
                lines.push(format!("{}:", ts.thread));
                if ts.frames.is_empty() {
                    lines.push("  (no frames)".into());
                }
                for f in &ts.frames {
                    let loc = &f.location;
                    let file = loc.file.as_deref().unwrap_or("?");
                    if f.is_native {
                        lines.push(format!("  [{}] {}.{} (native)", f.index, loc.class, loc.method));
                    } else {
                        lines.push(format!("  [{}] {}.{} ({file}:{})", f.index, loc.class, loc.method, loc.line));
                    }
                }
            }
            lines.join("\n")
        }
        CommandResult::Locals { vars } => {
            if vars.is_empty() {
                return "No local variables.".into();
            }
            vars.iter().map(|v| {
                match &v.ty {
                    Some(t) => format!("  {} ({t}) = {}", v.name, v.value),
                    None => format!("  {} = {}", v.name, v.value),
                }
            }).collect::<Vec<_>>().join("\n")
        }
        CommandResult::Value { expr, value, ty } => {
            match ty {
                Some(t) => format!("{expr} ({t}) = {value}"),
                None => format!("{expr} = {value}"),
            }
        }
        CommandResult::ObjectDump { expr, fields } => {
            let mut out = format!("{expr} = {{\n");
            for f in fields {
                out.push_str(&format!("  {}: {},\n", f.name, f.value));
            }
            out.push('}');
            out
        }
        CommandResult::Threads { threads } => {
            let mut lines = Vec::new();
            let mut last_group: Option<&str> = None;
            for t in threads {
                let group = t.group.as_deref().unwrap_or("?");
                if last_group != Some(group) {
                    lines.push(format!("Group {group}:"));
                    last_group = Some(group);
                }
                lines.push(format!("  {} {:<24} {}", t.id, t.name, t.state));
            }
            lines.join("\n")
        }
        CommandResult::Source { lines, .. } => {
            lines.iter().map(|l| {
                format!("{:>4}  {}", l.number, l.text)
            }).collect::<Vec<_>>().join("\n")
        }
        CommandResult::Inspection { expr, size, elements, truncated } => {
            let sz = size.map(|s| format!(" (size={s})")).unwrap_or_default();
            let mut out = format!("{expr}{sz}:");
            if elements.is_empty() {
                out.push_str(" (empty or inaccessible)");
            } else {
                for el in elements {
                    out.push_str(&format!("\n  {} = {}", el.name, el.value));
                }
            }
            if *truncated == Some(true) {
                out.push_str("\n  ... (truncated)");
            }
            out
        }
        CommandResult::Raw { text } => text.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stopped_with_source_context_renders_markers() {
        let resp = CommandResponse {
            result: CommandResult::Stopped {
                event: Event::Breakpoint {
                    location: Location { class: "Main".into(), method: "main".into(), file: None, line: 10 },
                    thread: "main".into(),
                },
                location: Location { class: "Main".into(), method: "main".into(), file: None, line: 10 },
                thread: "main".into(),
                frame: None,
                source_context: Some(vec![
                    SourceLine { number: 9, text: "int x = 1;".into() },
                    SourceLine { number: 10, text: "int y = 2;".into() },
                    SourceLine { number: 11, text: "return x + y;".into() },
                ]),
            },
            stderr: None,
            note: None,
        };
        let out = render(&resp, false);
        assert!(out.contains("Breakpoint hit: Main.main() line=10 thread=main"));
        assert!(out.contains("=>   10  int y = 2;"));
        assert!(out.contains("     9  int x = 1;"));
        assert!(out.contains("    11  return x + y;"));
    }

    #[test]
    fn stopped_without_source_context_is_one_line() {
        let resp = CommandResponse {
            result: CommandResult::Stopped {
                event: Event::Step {
                    location: Location { class: "Foo".into(), method: "bar".into(), file: None, line: 5 },
                    thread: "t1".into(),
                },
                location: Location { class: "Foo".into(), method: "bar".into(), file: None, line: 5 },
                thread: "t1".into(),
                frame: None,
                source_context: None,
            },
            stderr: None,
            note: None,
        };
        let out = render(&resp, false);
        assert_eq!(out, "Step completed: Foo.bar() line=5 thread=t1");
    }

    #[test]
    fn inspection_renders_elements_and_truncation() {
        let resp = CommandResponse {
            result: CommandResult::Inspection {
                expr: "myList".into(),
                size: Some(3),
                elements: vec![
                    VarBinding { name: "[0]".into(), ty: None, value: "\"hello\"".into() },
                    VarBinding { name: "[1]".into(), ty: None, value: "\"world\"".into() },
                ],
                truncated: Some(true),
            },
            stderr: None,
            note: None,
        };
        let out = render(&resp, false);
        assert!(out.contains("myList (size=3):"));
        assert!(out.contains("[0] = \"hello\""));
        assert!(out.contains("[1] = \"world\""));
        assert!(out.contains("... (truncated)"));
    }

    #[test]
    fn inspection_empty_shows_placeholder() {
        let resp = CommandResponse {
            result: CommandResult::Inspection {
                expr: "emptyList".into(),
                size: Some(0),
                elements: vec![],
                truncated: Some(false),
            },
            stderr: None,
            note: None,
        };
        let out = render(&resp, false);
        assert!(out.contains("emptyList (size=0): (empty or inaccessible)"));
    }
}
