//! Tiny interactive terminal helpers for `jdbg setup`.
//!
//! A keyboard-driven multi-select prompt (arrow keys to move, space to toggle,
//! enter to confirm). Built on `std` raw FFI only — no `crossterm`/`termion`.
//! Both platforms are unified on VT escape sequences: on Unix raw `termios`
//! already delivers them; on Windows we enable `ENABLE_VIRTUAL_TERMINAL_INPUT`
//! so arrow keys arrive as the same `ESC [ A/B` byte sequences.
//!
//! If raw mode cannot be entered (not a real console, FFI failure) the caller
//! is expected to fall back to a plain line-based prompt.

use std::io::{self, Read, Write};

/// A single keypress relevant to the selector.
enum Key {
    Up,
    Down,
    Space,
    Enter,
    Cancel,
    Other,
}

/// Render a multi-select list and return the chosen state per item.
///
/// `title` is printed above the list. `labels` are the rows; `selected` is the
/// initial checked state (same length as `labels`). Returns `Ok(Some(states))`
/// when the user confirms with enter, `Ok(None)` when they cancel (Esc / `q` /
/// Ctrl-C), and `Err` when raw mode is unavailable so the caller can fall back.
pub fn multi_select(
    title: &str,
    labels: &[String],
    selected: &[bool],
) -> io::Result<Option<Vec<bool>>> {
    debug_assert_eq!(labels.len(), selected.len());
    if labels.is_empty() {
        return Ok(Some(Vec::new()));
    }

    let _raw = RawMode::enter()?;
    let mut state: Vec<bool> = selected.to_vec();
    let mut cursor = 0usize;
    let mut stdout = io::stdout();

    writeln!(
        stdout,
        "{title}\r\n  (\u{2191}/\u{2193} move \u{00b7} space toggle \u{00b7} enter confirm \u{00b7} esc cancel)\r"
    )?;
    render(&mut stdout, labels, &state, cursor, true)?;

    let mut input = io::stdin();
    let result = loop {
        match read_key(&mut input)? {
            Key::Up => cursor = (cursor + labels.len() - 1) % labels.len(),
            Key::Down => cursor = (cursor + 1) % labels.len(),
            Key::Space => state[cursor] = !state[cursor],
            Key::Enter => break Some(state.clone()),
            Key::Cancel => break None,
            Key::Other => continue,
        }
        render(&mut stdout, labels, &state, cursor, false)?;
    };

    // Leave the cursor on a fresh line below the list.
    writeln!(stdout, "\r")?;
    stdout.flush()?;
    Ok(result)
}

/// Draw (or redraw) the list. After the first draw we move the cursor back up
/// over the previously drawn rows and overwrite them in place.
fn render(
    out: &mut io::Stdout,
    labels: &[String],
    state: &[bool],
    cursor: usize,
    first: bool,
) -> io::Result<()> {
    if !first {
        // Move up over the rows drawn last time.
        write!(out, "\x1b[{}A", labels.len())?;
    }
    for (idx, label) in labels.iter().enumerate() {
        let pointer = if idx == cursor { ">" } else { " " };
        let mark = if state[idx] { "\u{2714}" } else { " " };
        // \r + clear-to-end so leftover characters from a longer prior line vanish.
        write!(out, "\r\x1b[K {pointer} [{mark}] {label}\r\n")?;
    }
    out.flush()
}

/// Read one logical keypress, decoding the few VT escape sequences we care about.
fn read_key(input: &mut io::Stdin) -> io::Result<Key> {
    let mut b = [0u8; 1];
    if input.read(&mut b)? == 0 {
        return Ok(Key::Cancel); // EOF
    }
    match b[0] {
        b' ' => Ok(Key::Space),
        b'\r' | b'\n' => Ok(Key::Enter),
        b'q' | 0x03 => Ok(Key::Cancel), // q or Ctrl-C
        0x1b => {
            // Either a bare Esc (cancel) or a CSI sequence: ESC [ A/B/C/D.
            if input.read(&mut b)? == 0 || b[0] != b'[' {
                return Ok(Key::Cancel);
            }
            if input.read(&mut b)? == 0 {
                return Ok(Key::Cancel);
            }
            match b[0] {
                b'A' => Ok(Key::Up),
                b'B' => Ok(Key::Down),
                _ => Ok(Key::Other),
            }
        }
        _ => Ok(Key::Other),
    }
}

// ----------------------------------------------------------------------------
// Raw mode: RAII guard that restores the terminal on drop.
// ----------------------------------------------------------------------------

#[cfg(unix)]
mod imp {
    use std::io;
    use std::os::raw::{c_int, c_void};

    // termios layout differs between Linux and macOS; define each precisely.
    #[cfg(target_os = "linux")]
    mod sys {
        use std::os::raw::c_uint;
        pub type Tcflag = c_uint;
        pub const NCCS: usize = 32;
        pub const ICANON: Tcflag = 0x0000_0002;
        pub const ECHO: Tcflag = 0x0000_0008;
        pub const ISIG: Tcflag = 0x0000_0001;
        pub const VMIN: usize = 6;
        pub const VTIME: usize = 5;
        #[repr(C)]
        #[derive(Clone)]
        pub struct Termios {
            pub c_iflag: Tcflag,
            pub c_oflag: Tcflag,
            pub c_cflag: Tcflag,
            pub c_lflag: Tcflag,
            pub c_line: u8,
            pub c_cc: [u8; NCCS],
            pub c_ispeed: Tcflag,
            pub c_ospeed: Tcflag,
        }
        impl Termios {
            pub fn zeroed() -> Self {
                Self {
                    c_iflag: 0,
                    c_oflag: 0,
                    c_cflag: 0,
                    c_lflag: 0,
                    c_line: 0,
                    c_cc: [0; NCCS],
                    c_ispeed: 0,
                    c_ospeed: 0,
                }
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    mod sys {
        use std::os::raw::c_ulong;
        pub type Tcflag = c_ulong;
        pub const NCCS: usize = 20;
        pub const ICANON: Tcflag = 0x0000_0100;
        pub const ECHO: Tcflag = 0x0000_0008;
        pub const ISIG: Tcflag = 0x0000_0080;
        pub const VMIN: usize = 16;
        pub const VTIME: usize = 17;
        #[repr(C)]
        #[derive(Clone)]
        pub struct Termios {
            pub c_iflag: Tcflag,
            pub c_oflag: Tcflag,
            pub c_cflag: Tcflag,
            pub c_lflag: Tcflag,
            pub c_cc: [u8; NCCS],
            pub c_ispeed: Tcflag,
            pub c_ospeed: Tcflag,
        }
        impl Termios {
            pub fn zeroed() -> Self {
                Self {
                    c_iflag: 0,
                    c_oflag: 0,
                    c_cflag: 0,
                    c_lflag: 0,
                    c_cc: [0; NCCS],
                    c_ispeed: 0,
                    c_ospeed: 0,
                }
            }
        }
    }

    const STDIN_FILENO: c_int = 0;
    const TCSANOW: c_int = 0;

    unsafe extern "C" {
        fn tcgetattr(fd: c_int, termios: *mut c_void) -> c_int;
        fn tcsetattr(fd: c_int, optional_actions: c_int, termios: *const c_void) -> c_int;
    }

    pub struct RawMode {
        original: sys::Termios,
    }

    impl RawMode {
        pub fn enter() -> io::Result<Self> {
            let mut original = sys::Termios::zeroed();
            let rc = unsafe { tcgetattr(STDIN_FILENO, &mut original as *mut _ as *mut c_void) };
            if rc != 0 {
                return Err(io::Error::last_os_error());
            }
            let mut raw = original.clone();
            // cbreak + no-echo: read byte-at-a-time, no line editing, no echo,
            // and clear ISIG so Ctrl-C reaches us as 0x03 (lets Drop restore).
            raw.c_lflag &= !(sys::ICANON | sys::ECHO | sys::ISIG);
            raw.c_cc[sys::VMIN] = 1;
            raw.c_cc[sys::VTIME] = 0;
            let rc =
                unsafe { tcsetattr(STDIN_FILENO, TCSANOW, &raw as *const _ as *const c_void) };
            if rc != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(RawMode { original })
        }
    }

    impl Drop for RawMode {
        fn drop(&mut self) {
            unsafe {
                tcsetattr(
                    STDIN_FILENO,
                    TCSANOW,
                    &self.original as *const _ as *const c_void,
                );
            }
        }
    }
}

#[cfg(windows)]
mod imp {
    use std::io;
    use std::os::raw::c_void;

    type Handle = *mut c_void;
    const STD_INPUT_HANDLE: u32 = 0xFFFF_FFF6; // (DWORD)-10
    const STD_OUTPUT_HANDLE: u32 = 0xFFFF_FFF5; // (DWORD)-11
    const INVALID_HANDLE_VALUE: isize = -1;

    const ENABLE_PROCESSED_INPUT: u32 = 0x0001;
    const ENABLE_LINE_INPUT: u32 = 0x0002;
    const ENABLE_ECHO_INPUT: u32 = 0x0004;
    const ENABLE_VIRTUAL_TERMINAL_INPUT: u32 = 0x0200;
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;

    unsafe extern "system" {
        fn GetStdHandle(n: u32) -> Handle;
        fn GetConsoleMode(h: Handle, mode: *mut u32) -> i32;
        fn SetConsoleMode(h: Handle, mode: u32) -> i32;
    }

    fn std_handle(which: u32) -> io::Result<Handle> {
        let h = unsafe { GetStdHandle(which) };
        if h.is_null() || h as isize == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Ok(h)
    }

    fn get_mode(h: Handle) -> io::Result<u32> {
        let mut mode = 0u32;
        if unsafe { GetConsoleMode(h, &mut mode) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(mode)
    }

    fn set_mode(h: Handle, mode: u32) -> io::Result<()> {
        if unsafe { SetConsoleMode(h, mode) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub struct RawMode {
        stdin: Handle,
        stdout: Handle,
        in_orig: u32,
        out_orig: u32,
    }

    impl RawMode {
        pub fn enter() -> io::Result<Self> {
            let stdin = std_handle(STD_INPUT_HANDLE)?;
            let stdout = std_handle(STD_OUTPUT_HANDLE)?;
            let in_orig = get_mode(stdin)?;
            let out_orig = get_mode(stdout)?;
            // Raw input: no line buffering, no echo, no Ctrl-C handling (we read
            // it as a byte), VT input so arrows arrive as ESC sequences.
            let in_raw = (in_orig
                & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT))
                | ENABLE_VIRTUAL_TERMINAL_INPUT;
            set_mode(stdin, in_raw)?;
            // VT output so our ANSI redraw escapes are honored.
            if let Err(e) = set_mode(stdout, out_orig | ENABLE_VIRTUAL_TERMINAL_PROCESSING) {
                let _ = set_mode(stdin, in_orig);
                return Err(e);
            }
            Ok(RawMode {
                stdin,
                stdout,
                in_orig,
                out_orig,
            })
        }
    }

    impl Drop for RawMode {
        fn drop(&mut self) {
            let _ = set_mode(self.stdin, self.in_orig);
            let _ = set_mode(self.stdout, self.out_orig);
        }
    }
}

#[cfg(not(any(unix, windows)))]
mod imp {
    use std::io;
    pub struct RawMode;
    impl RawMode {
        pub fn enter() -> io::Result<Self> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "interactive selection not supported on this platform",
            ))
        }
    }
}

use imp::RawMode;
