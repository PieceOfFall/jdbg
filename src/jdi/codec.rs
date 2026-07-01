//! Length-prefixed frame codec for the JDI sidecar protocol.

/// Default maximum sidecar JSON payload size (8 MiB).
pub const DEFAULT_MAX_FRAME_SIZE: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
    #[error("sidecar frame is too large: {size} bytes (max {max})")]
    FrameTooLarge { size: usize, max: usize },
    #[error("sidecar protocol stream ended in the middle of a frame")]
    UnexpectedEof,
}

/// Encode one payload as `[4-byte big-endian length][payload]`.
pub fn encode_frame(payload: &[u8], max_frame_size: usize) -> Result<Vec<u8>, FrameError> {
    if payload.len() > max_frame_size {
        return Err(FrameError::FrameTooLarge {
            size: payload.len(),
            max: max_frame_size,
        });
    }
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(payload);
    Ok(frame)
}

/// Incremental decoder for length-prefixed sidecar frames.
#[derive(Debug)]
pub struct FrameDecoder {
    max_frame_size: usize,
    buffer: Vec<u8>,
}

impl FrameDecoder {
    pub fn new(max_frame_size: usize) -> Self {
        Self {
            max_frame_size,
            buffer: Vec::new(),
        }
    }

    /// Add bytes and return all complete payloads decoded from the buffer.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Vec<u8>>, FrameError> {
        self.buffer.extend_from_slice(bytes);
        let mut frames = Vec::new();

        loop {
            if self.buffer.len() < 4 {
                break;
            }
            let len = u32::from_be_bytes([
                self.buffer[0],
                self.buffer[1],
                self.buffer[2],
                self.buffer[3],
            ]) as usize;
            if len > self.max_frame_size {
                return Err(FrameError::FrameTooLarge {
                    size: len,
                    max: self.max_frame_size,
                });
            }
            if self.buffer.len() < 4 + len {
                break;
            }
            frames.push(self.buffer[4..4 + len].to_vec());
            self.buffer.drain(..4 + len);
        }

        Ok(frames)
    }

    /// Signal EOF. A clean EOF has no partial bytes buffered.
    pub fn finish(&self) -> Result<(), FrameError> {
        if self.buffer.is_empty() {
            Ok(())
        } else {
            Err(FrameError::UnexpectedEof)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX: usize = 16;

    #[test]
    fn decodes_normal_frame() {
        let frame = encode_frame(b"hello", MAX).unwrap();
        let mut decoder = FrameDecoder::new(MAX);

        let frames = decoder.push(&frame).unwrap();

        assert_eq!(frames, vec![b"hello".to_vec()]);
    }

    #[test]
    fn decodes_consecutive_frames() {
        let mut bytes = encode_frame(b"one", MAX).unwrap();
        bytes.extend(encode_frame(b"two", MAX).unwrap());
        let mut decoder = FrameDecoder::new(MAX);

        let frames = decoder.push(&bytes).unwrap();

        assert_eq!(frames, vec![b"one".to_vec(), b"two".to_vec()]);
    }

    #[test]
    fn decodes_split_frame() {
        let frame = encode_frame(b"hello", MAX).unwrap();
        let mut decoder = FrameDecoder::new(MAX);

        assert!(decoder.push(&frame[..3]).unwrap().is_empty());
        let frames = decoder.push(&frame[3..]).unwrap();

        assert_eq!(frames, vec![b"hello".to_vec()]);
    }

    #[test]
    fn decodes_payloads_containing_newlines() {
        let frame = encode_frame(b"{\n  \"ok\": true\n}", MAX).unwrap();
        let mut decoder = FrameDecoder::new(MAX);

        let frames = decoder.push(&frame).unwrap();

        assert_eq!(frames, vec![b"{\n  \"ok\": true\n}".to_vec()]);
    }

    #[test]
    fn decodes_empty_payload() {
        let frame = encode_frame(b"", MAX).unwrap();
        let mut decoder = FrameDecoder::new(MAX);

        let frames = decoder.push(&frame).unwrap();

        assert_eq!(frames, vec![Vec::<u8>::new()]);
    }

    #[test]
    fn finish_rejects_eof_during_header_or_body() {
        let mut header_only = FrameDecoder::new(MAX);
        header_only.push(&[0, 0]).unwrap();
        assert_eq!(header_only.finish().unwrap_err(), FrameError::UnexpectedEof);

        let mut body_partial = FrameDecoder::new(MAX);
        body_partial.push(&[0, 0, 0, 5, b'h']).unwrap();
        assert_eq!(
            body_partial.finish().unwrap_err(),
            FrameError::UnexpectedEof
        );
    }

    #[test]
    fn rejects_oversized_frames() {
        assert_eq!(
            encode_frame(&[0; MAX + 1], MAX).unwrap_err(),
            FrameError::FrameTooLarge {
                size: MAX + 1,
                max: MAX,
            }
        );

        let mut decoder = FrameDecoder::new(MAX);
        let bytes = (MAX as u32 + 1).to_be_bytes();
        assert_eq!(
            decoder.push(&bytes).unwrap_err(),
            FrameError::FrameTooLarge {
                size: MAX + 1,
                max: MAX,
            }
        );
    }
}
