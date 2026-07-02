//! Platform-local sidecar connection setup.

use std::io;

use crate::jdi::lifecycle::SidecarEndpoint;
use crate::jdi::transport::SidecarStream;

pub struct PendingSidecarConnection {
    inner: imp::PendingSidecarConnection,
}

#[cfg(windows)]
unsafe impl Send for PendingSidecarConnection {}

impl PendingSidecarConnection {
    pub fn new(label: &str) -> io::Result<Self> {
        Ok(Self {
            inner: imp::PendingSidecarConnection::new(label)?,
        })
    }

    pub fn endpoint(&self) -> SidecarEndpoint {
        self.inner.endpoint()
    }

    pub fn accept(self) -> io::Result<SidecarStream> {
        self.inner.accept()
    }
}

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::io;
    use std::os::windows::io::{FromRawHandle, RawHandle};
    use std::ptr;

    use crate::jdi::lifecycle::SidecarEndpoint;
    use crate::jdi::transport::SidecarStream;

    const PIPE_ACCESS_INBOUND: u32 = 0x0000_0001;
    const PIPE_ACCESS_OUTBOUND: u32 = 0x0000_0002;
    const PIPE_TYPE_BYTE: u32 = 0x0000_0000;
    const PIPE_READMODE_BYTE: u32 = 0x0000_0000;
    const PIPE_WAIT: u32 = 0x0000_0000;
    const ERROR_PIPE_CONNECTED: u32 = 535;
    const INVALID_HANDLE_VALUE: RawHandle = -1isize as RawHandle;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateNamedPipeW(
            lpName: *const u16,
            dwOpenMode: u32,
            dwPipeMode: u32,
            nMaxInstances: u32,
            nOutBufferSize: u32,
            nInBufferSize: u32,
            nDefaultTimeOut: u32,
            lpSecurityAttributes: *mut c_void,
        ) -> RawHandle;
        fn ConnectNamedPipe(hNamedPipe: RawHandle, lpOverlapped: *mut c_void) -> i32;
        fn CloseHandle(hObject: RawHandle) -> i32;
        fn GetLastError() -> u32;
    }

    pub struct PendingSidecarConnection {
        endpoint: SidecarEndpoint,
        to_sidecar: Option<RawHandle>,
        from_sidecar: Option<RawHandle>,
    }

    impl PendingSidecarConnection {
        pub fn new(label: &str) -> io::Result<Self> {
            let pipe_name = format!(r"\\.\pipe\jdbg-jdi-{}-{label}", std::process::id());
            let to_sidecar = create_pipe(&format!("{pipe_name}-to-sidecar"), PIPE_ACCESS_OUTBOUND)?;
            let from_sidecar =
                create_pipe(&format!("{pipe_name}-from-sidecar"), PIPE_ACCESS_INBOUND)?;
            Ok(Self {
                endpoint: SidecarEndpoint::new("named-pipe", pipe_name),
                to_sidecar: Some(to_sidecar),
                from_sidecar: Some(from_sidecar),
            })
        }

        pub fn endpoint(&self) -> SidecarEndpoint {
            self.endpoint.clone()
        }

        pub fn accept(mut self) -> io::Result<SidecarStream> {
            let to_sidecar = self.to_sidecar.take().expect("to-sidecar pipe present");
            let from_sidecar = self.from_sidecar.take().expect("from-sidecar pipe present");
            connect_pipe(to_sidecar)?;
            connect_pipe(from_sidecar)?;
            let writer = unsafe { std::fs::File::from_raw_handle(to_sidecar) };
            let reader = unsafe { std::fs::File::from_raw_handle(from_sidecar) };
            Ok(SidecarStream::file_pair(reader, writer))
        }
    }

    fn create_pipe(pipe_name: &str, open_mode: u32) -> io::Result<RawHandle> {
        let mut wide: Vec<u16> = pipe_name.encode_utf16().collect();
        wide.push(0);
        let handle = unsafe {
            CreateNamedPipeW(
                wide.as_ptr(),
                open_mode,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                1,
                8192,
                8192,
                0,
                ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Ok(handle)
    }

    fn connect_pipe(handle: RawHandle) -> io::Result<()> {
        let connected = unsafe { ConnectNamedPipe(handle, ptr::null_mut()) };
        if connected == 0 {
            let error = unsafe { GetLastError() };
            if error != ERROR_PIPE_CONNECTED {
                unsafe {
                    CloseHandle(handle);
                }
                return Err(io::Error::from_raw_os_error(error as i32));
            }
        }
        Ok(())
    }

    impl Drop for PendingSidecarConnection {
        fn drop(&mut self) {
            if let Some(handle) = self.to_sidecar.take() {
                unsafe {
                    CloseHandle(handle);
                }
            }
            if let Some(handle) = self.from_sidecar.take() {
                unsafe {
                    CloseHandle(handle);
                }
            }
        }
    }
}

#[cfg(unix)]
mod imp {
    use std::io;
    use std::os::fd::{AsRawFd, RawFd};
    use std::os::unix::net::UnixStream;

    use crate::jdi::lifecycle::SidecarEndpoint;
    use crate::jdi::transport::SidecarStream;

    const F_GETFD: i32 = 1;
    const F_SETFD: i32 = 2;
    const FD_CLOEXEC: i32 = 1;

    unsafe extern "C" {
        fn fcntl(fd: i32, cmd: i32, ...) -> i32;
    }

    pub struct PendingSidecarConnection {
        endpoint: SidecarEndpoint,
        parent: UnixStream,
        _child: UnixStream,
    }

    impl PendingSidecarConnection {
        pub fn new(label: &str) -> io::Result<Self> {
            let (parent, child) = UnixStream::pair()?;
            clear_cloexec(child.as_raw_fd())?;
            Ok(Self {
                endpoint: SidecarEndpoint::new("unix-domain-socket", child.as_raw_fd().to_string()),
                parent,
                _child: child,
            })
        }

        pub fn endpoint(&self) -> SidecarEndpoint {
            self.endpoint.clone()
        }

        pub fn accept(self) -> io::Result<SidecarStream> {
            Ok(SidecarStream::unix(self.parent))
        }
    }

    fn clear_cloexec(fd: RawFd) -> io::Result<()> {
        let flags = unsafe { fcntl(fd, F_GETFD) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { fcntl(fd, F_SETFD, flags & !FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepared_endpoint_uses_platform_local_transport() {
        let pending = PendingSidecarConnection::new("jdbg-jdi-test").unwrap();
        let endpoint = pending.endpoint();

        assert_ne!(endpoint.transport, "tcp");
        #[cfg(windows)]
        {
            assert_eq!(endpoint.transport, "named-pipe");
            assert!(endpoint.endpoint.starts_with(r"\\.\pipe\jdbg-jdi-"));
        }
        #[cfg(unix)]
        {
            assert_eq!(endpoint.transport, "unix-domain-socket");
            assert!(endpoint.endpoint.parse::<i32>().is_ok());
        }
    }
}
