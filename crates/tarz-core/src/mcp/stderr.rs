//! The stderr pipe of a stdio MCP server.

use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;

/// First bytes of a server's stderr retained as a failure reason.
const STDERR_CAPTURE_BYTES: usize = 8 * 1024;

/// The first bytes of a server's stderr, and when the pipe ends.
///
/// A reader thread keeps draining the pipe so a chatty server cannot block
/// on a full pipe. The pipe ends when every process holding its write end
/// has exited, which is the only exit signal the adapter leaves the host.
pub(crate) struct StderrCapture {
    prefix: Mutex<Vec<u8>>,
    ended: watch::Receiver<bool>,
}

impl StderrCapture {
    pub(crate) fn start(mut reader: std::io::PipeReader) -> Arc<Self> {
        let (ended_sender, ended) = watch::channel(false);
        let capture = Arc::new(Self {
            prefix: Mutex::new(Vec::new()),
            ended,
        });
        let sink = capture.clone();
        std::thread::spawn(move || {
            let mut buffer = [0u8; 4096];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => {
                        let mut prefix = sink.prefix.lock().expect("stderr prefix lock");
                        if prefix.len() < STDERR_CAPTURE_BYTES {
                            let keep = (STDERR_CAPTURE_BYTES - prefix.len()).min(count);
                            prefix.extend_from_slice(&buffer[..keep]);
                        }
                    }
                }
            }
            let _ = ended_sender.send(true);
        });
        capture
    }

    /// Wait up to `wait` for the pipe to end; whether it did.
    pub(crate) async fn ended(&self, wait: Duration) -> bool {
        let mut ended = self.ended.clone();
        tokio::time::timeout(wait, ended.wait_for(|ended| *ended))
            .await
            .is_ok_and(|result| result.is_ok())
    }

    /// The retained text once the pipe ends or `wait` passes; `None` when empty.
    pub(crate) async fn text(&self, wait: Duration) -> Option<String> {
        self.ended(wait).await;
        let bytes = self.prefix.lock().expect("stderr prefix lock").clone();
        let text = String::from_utf8_lossy(&bytes).trim().to_owned();
        (!text.is_empty()).then_some(text)
    }
}
