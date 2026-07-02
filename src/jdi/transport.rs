//! Blocking TCP transport for the JDI sidecar.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
#[cfg(test)]
use std::net::TcpStream;
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::Value;

use crate::jdi::codec::{DEFAULT_MAX_FRAME_SIZE, FrameError, encode_frame};
use crate::jdi::protocol::{SidecarErrorPayload, SidecarMessage};

type PendingMap = Arc<Mutex<HashMap<String, Sender<SidecarResponse>>>>;

pub enum SidecarStream {
    #[cfg(test)]
    Tcp(TcpStream),
    #[cfg(unix)]
    Unix(UnixStream),
    #[cfg(windows)]
    FilePair {
        reader: std::fs::File,
        writer: std::fs::File,
    },
}

impl SidecarStream {
    #[cfg(test)]
    fn tcp_for_tests(stream: TcpStream) -> Self {
        Self::Tcp(stream)
    }

    #[cfg(unix)]
    pub(crate) fn unix(stream: UnixStream) -> Self {
        Self::Unix(stream)
    }

    #[cfg(windows)]
    pub(crate) fn file_pair(reader: std::fs::File, writer: std::fs::File) -> Self {
        Self::FilePair { reader, writer }
    }

    pub(crate) fn try_clone(&self) -> std::io::Result<Self> {
        match self {
            #[cfg(test)]
            Self::Tcp(stream) => Ok(Self::Tcp(stream.try_clone()?)),
            #[cfg(unix)]
            Self::Unix(stream) => Ok(Self::Unix(stream.try_clone()?)),
            #[cfg(windows)]
            Self::FilePair { reader, writer } => Ok(Self::FilePair {
                reader: reader.try_clone()?,
                writer: writer.try_clone()?,
            }),
        }
    }
}

impl Read for SidecarStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            #[cfg(test)]
            Self::Tcp(stream) => stream.read(buf),
            #[cfg(unix)]
            Self::Unix(stream) => stream.read(buf),
            #[cfg(windows)]
            Self::FilePair { reader, .. } => reader.read(buf),
        }
    }
}

impl Write for SidecarStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            #[cfg(test)]
            Self::Tcp(stream) => stream.write(buf),
            #[cfg(unix)]
            Self::Unix(stream) => stream.write(buf),
            #[cfg(windows)]
            Self::FilePair { writer, .. } => writer.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            #[cfg(test)]
            Self::Tcp(stream) => stream.flush(),
            #[cfg(unix)]
            Self::Unix(stream) => stream.flush(),
            #[cfg(windows)]
            Self::FilePair { writer, .. } => writer.flush(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SidecarEvent {
    pub session: String,
    pub seq: u64,
    pub event: String,
    pub payload: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SidecarResponse {
    pub id: String,
    pub result: Option<Value>,
    pub error: Option<SidecarErrorPayload>,
}

#[derive(Debug, thiserror::Error)]
pub enum SidecarTransportError {
    #[error("sidecar IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("sidecar frame error: {0}")]
    Frame(#[from] FrameError),
    #[error("sidecar JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("sidecar request {id} timed out after {timeout:?}")]
    Timeout { id: String, timeout: Duration },
    #[error("sidecar returned error {code}: {message}")]
    Remote { code: String, message: String },
    #[error("sidecar reader is disconnected")]
    Disconnected,
}

/// A connected sidecar transport.
pub struct SidecarTransport {
    writer: Mutex<SidecarStream>,
    pending: PendingMap,
    events: Arc<Mutex<VecDeque<SidecarEvent>>>,
    ids: AtomicU64,
    _reader: JoinHandle<()>,
}

impl SidecarTransport {
    pub fn start(stream: SidecarStream) -> std::io::Result<Self> {
        let reader_stream = stream.try_clone()?;
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let events = Arc::new(Mutex::new(VecDeque::new()));
        let reader = spawn_reader(reader_stream, Arc::clone(&pending), Arc::clone(&events));
        Ok(Self {
            writer: Mutex::new(stream),
            pending,
            events,
            ids: AtomicU64::new(1),
            _reader: reader,
        })
    }

    pub fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, SidecarTransportError> {
        let id = self.next_id();
        let rx = self.register_pending(id.clone());
        let msg = SidecarMessage::Request {
            id: id.clone(),
            method: method.into(),
            params,
        };
        self.write_message(&msg)?;

        match rx.recv_timeout(timeout) {
            Ok(resp) => response_result(resp),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.pending
                    .lock()
                    .expect("pending mutex poisoned")
                    .remove(&id);
                Err(SidecarTransportError::Timeout { id, timeout })
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(SidecarTransportError::Disconnected),
        }
    }

    pub fn ping(&self, timeout: Duration) -> Result<Value, SidecarTransportError> {
        self.request("ping", serde_json::json!({}), timeout)
    }

    pub fn shutdown(&self, timeout: Duration) -> Result<Value, SidecarTransportError> {
        self.request("shutdown", serde_json::json!({}), timeout)
    }

    pub fn drain_events(&self) -> Vec<SidecarEvent> {
        let mut events = self.events.lock().expect("events mutex poisoned");
        events.drain(..).collect()
    }

    fn next_id(&self) -> String {
        format!("r{}", self.ids.fetch_add(1, Ordering::Relaxed))
    }

    fn register_pending(&self, id: String) -> Receiver<SidecarResponse> {
        let (tx, rx) = mpsc::channel();
        self.pending
            .lock()
            .expect("pending mutex poisoned")
            .insert(id, tx);
        rx
    }

    fn write_message(&self, msg: &SidecarMessage) -> Result<(), SidecarTransportError> {
        let mut writer = self.writer.lock().expect("writer mutex poisoned");
        write_framed_message(&mut *writer, msg)
    }
}

fn spawn_reader(
    mut stream: SidecarStream,
    pending: PendingMap,
    events: Arc<Mutex<VecDeque<SidecarEvent>>>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        while let Ok(msg) = read_framed_message(&mut stream) {
            dispatch_incoming(msg, &pending, &events);
        }
    })
}

pub(crate) fn read_framed_message(
    stream: &mut impl Read,
) -> Result<SidecarMessage, SidecarTransportError> {
    let mut header = [0; 4];
    stream.read_exact(&mut header)?;
    let len = u32::from_be_bytes(header) as usize;
    if len > DEFAULT_MAX_FRAME_SIZE {
        return Err(FrameError::FrameTooLarge {
            size: len,
            max: DEFAULT_MAX_FRAME_SIZE,
        }
        .into());
    }
    let mut body = vec![0; len];
    stream.read_exact(&mut body)?;
    Ok(serde_json::from_slice(&body)?)
}

pub(crate) fn write_framed_message(
    stream: &mut impl Write,
    msg: &SidecarMessage,
) -> Result<(), SidecarTransportError> {
    let body = serde_json::to_vec(msg)?;
    let frame = encode_frame(&body, DEFAULT_MAX_FRAME_SIZE)?;
    stream.write_all(&frame)?;
    stream.flush()?;
    Ok(())
}

fn dispatch_incoming(
    msg: SidecarMessage,
    pending: &PendingMap,
    events: &Arc<Mutex<VecDeque<SidecarEvent>>>,
) {
    match msg {
        SidecarMessage::Response { id, result, error } => {
            if let Some(tx) = pending.lock().expect("pending mutex poisoned").remove(&id) {
                let _ = tx.send(SidecarResponse { id, result, error });
            }
        }
        SidecarMessage::Event {
            session,
            seq,
            event,
            payload,
        } => {
            events
                .lock()
                .expect("events mutex poisoned")
                .push_back(SidecarEvent {
                    session,
                    seq,
                    event,
                    payload,
                });
        }
        SidecarMessage::Heartbeat { .. } | SidecarMessage::Request { .. } => {}
    }
}

fn response_result(resp: SidecarResponse) -> Result<Value, SidecarTransportError> {
    if let Some(err) = resp.error {
        return Err(SidecarTransportError::Remote {
            code: err.code,
            message: err.message,
        });
    }
    Ok(resp.result.unwrap_or(Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jdi::protocol::SidecarMessage;
    use serde_json::json;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;
    use std::time::Duration;

    #[test]
    fn request_matches_response_and_queues_interleaved_event() {
        let (client_stream, server_thread) = connected_fake_server(|mut stream| {
            let request = read_test_message(&mut stream);
            let SidecarMessage::Request { id, .. } = request else {
                panic!("expected request");
            };
            write_test_message(
                &mut stream,
                &SidecarMessage::Event {
                    session: "s1".into(),
                    seq: 1,
                    event: "vmDisconnected".into(),
                    payload: json!({}),
                },
            );
            write_test_message(
                &mut stream,
                &SidecarMessage::Response {
                    id,
                    result: Some(json!({"ok": true})),
                    error: None,
                },
            );
        });
        let client = SidecarTransport::start(SidecarStream::tcp_for_tests(client_stream)).unwrap();

        let result = client
            .request("ping", json!({}), Duration::from_secs(1))
            .unwrap();

        assert_eq!(result, json!({"ok": true}));
        assert_eq!(client.drain_events().len(), 1);
        server_thread.join().unwrap();
    }

    #[test]
    fn ignores_response_for_unknown_id_and_waits_for_matching_id() {
        let (client_stream, server_thread) = connected_fake_server(|mut stream| {
            let request = read_test_message(&mut stream);
            let SidecarMessage::Request { id, .. } = request else {
                panic!("expected request");
            };
            write_test_message(
                &mut stream,
                &SidecarMessage::Response {
                    id: "other".into(),
                    result: Some(json!({"wrong": true})),
                    error: None,
                },
            );
            write_test_message(
                &mut stream,
                &SidecarMessage::Response {
                    id,
                    result: Some(json!({"right": true})),
                    error: None,
                },
            );
        });
        let client = SidecarTransport::start(SidecarStream::tcp_for_tests(client_stream)).unwrap();

        let result = client
            .request("ping", json!({}), Duration::from_secs(1))
            .unwrap();

        assert_eq!(result, json!({"right": true}));
        server_thread.join().unwrap();
    }

    #[test]
    fn request_times_out_without_killing_transport() {
        let (client_stream, server_thread) = connected_fake_server(|mut stream| {
            let first = read_test_message(&mut stream);
            let SidecarMessage::Request { id: first_id, .. } = first else {
                panic!("expected request");
            };
            let second = read_test_message(&mut stream);
            let SidecarMessage::Request { id: second_id, .. } = second else {
                panic!("expected request");
            };
            assert_ne!(first_id, second_id);
            write_test_message(
                &mut stream,
                &SidecarMessage::Response {
                    id: second_id,
                    result: Some(json!({"ok": true})),
                    error: None,
                },
            );
        });
        let client = SidecarTransport::start(SidecarStream::tcp_for_tests(client_stream)).unwrap();

        assert!(matches!(
            client.request("slow", json!({}), Duration::from_millis(20)),
            Err(SidecarTransportError::Timeout { .. })
        ));
        let result = client
            .request("ping", json!({}), Duration::from_secs(1))
            .unwrap();

        assert_eq!(result, json!({"ok": true}));
        server_thread.join().unwrap();
    }

    fn connected_fake_server(
        f: impl FnOnce(TcpStream) + Send + 'static,
    ) -> (TcpStream, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).unwrap();
        let handle = thread::spawn(move || {
            let (server, _) = listener.accept().unwrap();
            f(server);
        });
        (client, handle)
    }

    fn read_test_message(stream: &mut TcpStream) -> SidecarMessage {
        let mut header = [0; 4];
        stream.read_exact(&mut header).unwrap();
        let len = u32::from_be_bytes(header) as usize;
        let mut body = vec![0; len];
        stream.read_exact(&mut body).unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    fn write_test_message(stream: &mut TcpStream, msg: &SidecarMessage) {
        let body = serde_json::to_vec(msg).unwrap();
        stream
            .write_all(
                &crate::jdi::codec::encode_frame(&body, crate::jdi::codec::DEFAULT_MAX_FRAME_SIZE)
                    .unwrap(),
            )
            .unwrap();
        stream.flush().unwrap();
    }
}
