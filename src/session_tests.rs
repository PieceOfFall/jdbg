//! session 层映射逻辑的单元测试——聚焦 `event_to_result`（纯函数）。

#[cfg(test)]
mod tests {
    use super::super::{CommandKind, event_to_result};
    use crate::jdb::reader::DetectedEvent;
    use crate::protocol::*;

    #[test]
    fn breakpoint_event_maps_to_stopped() {
        let ev = DetectedEvent::Breakpoint {
            thread: "main".into(),
            class: "Main".into(),
            method: "main".into(),
            line: 9,
        };
        let (result, event) = event_to_result(ev);

        let CommandResult::Stopped { location, thread, event: inner_event, frame } = result else {
            panic!("expected Stopped, got {result:?}");
        };
        assert_eq!(thread, "main");
        assert_eq!(location.class, "Main");
        assert_eq!(location.method, "main");
        assert_eq!(location.line, 9);
        assert!(frame.is_none());
        assert!(matches!(inner_event, Event::Breakpoint { .. }));
        assert!(matches!(event, Event::Breakpoint { .. }));
    }

    #[test]
    fn step_event_maps_to_stopped() {
        let ev = DetectedEvent::Step {
            thread: "main".into(),
            class: "Main".into(),
            method: "main".into(),
            line: 10,
        };
        let (result, event) = event_to_result(ev);

        let CommandResult::Stopped { location, event: inner_event, .. } = result else {
            panic!("expected Stopped, got {result:?}");
        };
        assert_eq!(location.line, 10);
        assert!(matches!(inner_event, Event::Step { .. }));
        assert!(matches!(event, Event::Step { .. }));
    }

    #[test]
    fn exception_event_maps_to_exception_caught() {
        let ev = DetectedEvent::Exception {
            thread: "main".into(),
            exception: "java.lang.NullPointerException".into(),
            caught: false,
        };
        let (result, event) = event_to_result(ev);

        let CommandResult::ExceptionCaught { exception, caught, thread, .. } = result else {
            panic!("expected ExceptionCaught, got {result:?}");
        };
        assert_eq!(exception, "java.lang.NullPointerException");
        assert!(!caught);
        assert_eq!(thread, "main");
        assert!(matches!(event, Event::Exception { caught: false, .. }));
    }

    #[test]
    fn command_kind_defaults() {
        use crate::jdb::parser::CommandHint;

        let n = CommandKind::normal(CommandHint::Locals);
        assert_eq!(n.timeout.as_secs(), 15);

        let b = CommandKind::blocking(CommandHint::Run);
        assert_eq!(b.timeout.as_secs(), 30);

        let custom = n.with_timeout(std::time::Duration::from_secs(5));
        assert_eq!(custom.timeout.as_secs(), 5);
    }
}
