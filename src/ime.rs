use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::env;
use std::fs::Metadata;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinHandle;
use tokio::time::timeout;

const PROTOCOL_VERSION: u8 = 1;
const MAX_FRAME_BYTES: usize = 256 * 1024;
const ACK_TIMEOUT: Duration = Duration::from_secs(2);
const SOCKET_DIRECTORY: &str = "daapstt";
const SOCKET_NAME: &str = "ime.sock";

#[derive(Debug, Deserialize)]
struct Response {
    id: Option<u64>,
    ok: Option<bool>,
    session: Option<u64>,
    error: Option<String>,
    event: Option<String>,
    reason: Option<String>,
}

#[derive(Debug)]
enum Incoming {
    Response(Response),
    ProtocolError(String),
}

#[derive(Default)]
struct Cancellation {
    happened: AtomicBool,
    notify: Notify,
}

impl Cancellation {
    fn signal(&self) {
        if !self.happened.swap(true, Ordering::AcqRel) {
            // There can only be one waiter because the public method borrows
            // the session mutably. notify_one also retains a permit across the
            // small race between checking the atomic flag and awaiting.
            self.notify.notify_one();
        }
    }

    fn clear(&self) {
        self.happened.store(false, Ordering::Release);
    }

    async fn wait(&self) {
        loop {
            let notified = self.notify.notified();
            if self.happened.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }
}

/// One connection-bound IBus dictation session.
///
/// Requests are serialized by the mutable API. A timeout is ambiguous (the
/// bridge may already have applied the operation), so the connection is torn
/// down and no operation is ever retried.
pub(crate) struct ImeSession {
    writer: Option<WriteHalf<UnixStream>>,
    incoming: mpsc::Receiver<Incoming>,
    reader: Option<JoinHandle<()>>,
    cancellation: Arc<Cancellation>,
    session: Option<u64>,
    next_id: u64,
    ack_timeout: Duration,
}

impl ImeSession {
    pub(crate) async fn begin() -> Result<Self> {
        let path = socket_path()?;
        validate_socket_path(&path)?;
        Self::begin_at(&path, ACK_TIMEOUT).await
    }

    async fn begin_at(path: &Path, ack_timeout: Duration) -> Result<Self> {
        let stream = timeout(ack_timeout, UnixStream::connect(path))
            .await
            .map_err(|_| anyhow!("timed out connecting to IME bridge"))?
            .with_context(|| format!("cannot connect to IME bridge at {}", path.display()))?;

        let peer = stream
            .peer_cred()
            .context("cannot inspect IME bridge peer credentials")?;
        let effective_uid = unsafe { libc::geteuid() };
        if peer.uid() != effective_uid {
            bail!("IME bridge is owned by a different user");
        }

        let (reader_half, writer) = tokio::io::split(stream);
        let (incoming_tx, incoming) = mpsc::channel(8);
        let cancellation = Arc::new(Cancellation::default());
        let reader_cancellation = cancellation.clone();
        let reader = tokio::spawn(async move {
            reader_loop(reader_half, incoming_tx, reader_cancellation).await;
        });

        let mut client = Self {
            writer: Some(writer),
            incoming,
            reader: Some(reader),
            cancellation,
            session: None,
            next_id: 1,
            ack_timeout,
        };

        let response = client.request("begin", None).await?;
        let session = response
            .session
            .filter(|session| *session != 0)
            .ok_or_else(|| anyhow!("IME bridge returned no session token"));
        match session {
            Ok(session) => {
                client.session = Some(session);
                Ok(client)
            }
            Err(error) => {
                client.close();
                Err(error)
            }
        }
    }

    pub(crate) async fn partial(&mut self, text: &str) -> Result<()> {
        self.session_request("partial", Some(text)).await
    }

    /// Commits the sole final transcript and terminates this session.
    ///
    /// Once the matching commit ACK is received, delivery succeeded. The
    /// socket is then closed immediately; a focus reset caused by that commit
    /// cannot retroactively turn the acknowledged delivery into a failure.
    pub(crate) async fn commit(&mut self, text: &str) -> Result<()> {
        self.session_request("commit", Some(text)).await?;
        self.close_normally();
        Ok(())
    }

    pub(crate) async fn clear(&mut self) -> Result<()> {
        self.session_request("clear", None).await
    }

    pub(crate) async fn finish(&mut self) -> Result<()> {
        self.terminal_request("end").await
    }

    pub(crate) async fn cancel(&mut self) -> Result<()> {
        self.terminal_request("cancel").await
    }

    /// Waits until the bridge invalidates this session or disconnects.
    ///
    /// A normally finished session is not a cancellation, so calling this
    /// after `commit`, `finish`, or `cancel` remains pending.
    pub(crate) async fn cancelled(&mut self) {
        self.cancellation.wait().await;
    }

    async fn terminal_request(&mut self, operation: &str) -> Result<()> {
        if self.session.is_none() {
            return Ok(());
        }
        let result = self.session_request(operation, None).await;
        if result.is_ok() {
            self.close_normally();
        } else {
            self.close();
        }
        result
    }

    async fn session_request(&mut self, operation: &str, text: Option<&str>) -> Result<()> {
        let session = self
            .session
            .ok_or_else(|| anyhow!("IME session is not active"))?;
        self.request(operation, Some((session, text))).await?;
        Ok(())
    }

    async fn request(
        &mut self,
        operation: &str,
        session_and_text: Option<(u64, Option<&str>)>,
    ) -> Result<Response> {
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| anyhow!("IME request id exhausted"))?;

        let mut request = json!({
            "v": PROTOCOL_VERSION,
            "id": id,
            "op": operation,
        });
        if let Some((session, text)) = session_and_text {
            request["session"] = Value::from(session);
            if let Some(text) = text {
                request["text"] = Value::from(text);
            }
        }
        let mut frame = serde_json::to_vec(&request).context("cannot encode IME request")?;
        if frame.len() > MAX_FRAME_BYTES {
            self.cancellation.signal();
            self.close();
            bail!("IME request exceeds {MAX_FRAME_BYTES} bytes");
        }
        frame.push(b'\n');

        // The deadline covers both backpressure while writing and the ACK.
        // Either can leave application state ambiguous, so every failure tears
        // down the connection and is never retried.
        let ack_timeout = self.ack_timeout;
        let operation = async {
            let writer = self
                .writer
                .as_mut()
                .ok_or_else(|| anyhow!("IME session is closed"))?;
            writer
                .write_all(&frame)
                .await
                .context("cannot write to IME bridge")?;
            self.wait_for_response(id).await
        };
        match timeout(ack_timeout, operation).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(error)) => {
                self.cancellation.signal();
                self.close();
                Err(error)
            }
            Err(_) => {
                self.cancellation.signal();
                self.close();
                bail!("timed out completing IME request")
            }
        }
    }

    async fn wait_for_response(&mut self, expected_id: u64) -> Result<Response> {
        loop {
            match self.incoming.recv().await {
                Some(Incoming::Response(response)) if response.event.as_deref() == Some("cancelled") => {
                    self.cancellation.signal();
                    let reason = response.reason.as_deref().unwrap_or("unknown");
                    bail!("IME session was cancelled: {reason}");
                }
                Some(Incoming::Response(response)) => {
                    if response.id != Some(expected_id) {
                        bail!("unexpected IME response id");
                    }
                    match response.ok {
                        Some(true) => return Ok(response),
                        Some(false) => {
                            bail!(
                                "IME bridge rejected request: {}",
                                response.error.as_deref().unwrap_or("invalid_request")
                            )
                        }
                        None => bail!("IME response has no acknowledgement status"),
                    }
                }
                Some(Incoming::ProtocolError(error)) => bail!("invalid IME response: {error}"),
                None => {
                    self.cancellation.signal();
                    bail!("IME bridge disconnected")
                }
            }
        }
    }

    fn close_normally(&mut self) {
        self.close();
        // The reader can observe the server closing immediately after its ACK.
        // Once the terminal ACK has won, that EOF is normal rather than an
        // invalidation and must not make a later `cancelled()` complete.
        self.cancellation.clear();
    }

    fn close(&mut self) {
        self.session = None;
        self.writer.take();
        if let Some(reader) = self.reader.take() {
            reader.abort();
        }
    }
}

impl Drop for ImeSession {
    fn drop(&mut self) {
        self.close();
    }
}

fn socket_path() -> Result<PathBuf> {
    let runtime = env::var_os("XDG_RUNTIME_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("XDG_RUNTIME_DIR is not set"))?;
    if !runtime.is_absolute() {
        bail!("XDG_RUNTIME_DIR is not an absolute path");
    }
    validate_private_directory(&runtime, "XDG runtime directory")?;
    let bridge_directory = runtime.join(SOCKET_DIRECTORY);
    validate_private_directory(&bridge_directory, "IME runtime directory")?;
    Ok(bridge_directory.join(SOCKET_NAME))
}

fn validate_private_directory(path: &Path, description: &str) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("cannot inspect {description} at {}", path.display()))?;
    validate_owner(&metadata, description)?;
    if !metadata.file_type().is_dir() {
        bail!("{description} is not a directory");
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        bail!("{description} is accessible by other users");
    }
    Ok(())
}

fn validate_socket_path(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("cannot inspect IME socket at {}", path.display()))?;
    validate_owner(&metadata, "IME socket")?;
    if !metadata.file_type().is_socket() {
        bail!("IME socket path is not a Unix socket");
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        bail!("IME socket is accessible by other users");
    }
    Ok(())
}

fn validate_owner(metadata: &Metadata, description: &str) -> Result<()> {
    let effective_uid = unsafe { libc::geteuid() };
    if metadata.uid() != effective_uid {
        bail!("{description} is owned by a different user");
    }
    Ok(())
}

async fn reader_loop(
    reader: ReadHalf<UnixStream>,
    incoming: mpsc::Sender<Incoming>,
    cancellation: Arc<Cancellation>,
) {
    let mut reader = BufReader::new(reader);
    loop {
        let frame = match read_frame(&mut reader).await {
            Ok(Some(frame)) => frame,
            Ok(None) => {
                cancellation.signal();
                return;
            }
            Err(error) => {
                let _ = incoming
                    .send(Incoming::ProtocolError(error.to_string()))
                    .await;
                cancellation.signal();
                return;
            }
        };
        let response = match serde_json::from_slice::<Response>(&frame) {
            Ok(response) => response,
            Err(error) => {
                let _ = incoming
                    .send(Incoming::ProtocolError(error.to_string()))
                    .await;
                cancellation.signal();
                return;
            }
        };
        let cancelled = response.event.as_deref() == Some("cancelled");
        if incoming.send(Incoming::Response(response)).await.is_err() {
            return;
        }
        if cancelled {
            cancellation.signal();
        }
    }
}

async fn read_frame(reader: &mut BufReader<ReadHalf<UnixStream>>) -> Result<Option<Vec<u8>>> {
    let mut frame = Vec::new();
    loop {
        let available = reader.fill_buf().await.context("cannot read IME response")?;
        if available.is_empty() {
            if frame.is_empty() {
                return Ok(None);
            }
            bail!("IME response ended before newline");
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(available.len(), |position| position);
        if frame.len() + take > MAX_FRAME_BYTES {
            bail!("IME response exceeds {MAX_FRAME_BYTES} bytes");
        }
        frame.extend_from_slice(&available[..take]);
        reader.consume(take + usize::from(newline.is_some()));
        if newline.is_some() {
            return Ok(Some(frame));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{UnixListener, UnixStream};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TestSocket {
        root: PathBuf,
        path: PathBuf,
        listener: UnixListener,
    }

    impl TestSocket {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = env::temp_dir().join(format!(
                "daapstt-ime-{}-{nonce}-{}",
                std::process::id(),
                TEST_COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
            let path = root.join("ime.sock");
            let listener = UnixListener::bind(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            Self { root, path, listener }
        }
    }

    impl Drop for TestSocket {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    async fn receive(reader: &mut BufReader<UnixStream>) -> Value {
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        serde_json::from_str(&line).unwrap()
    }

    async fn acknowledge_begin(stream: UnixStream) -> BufReader<UnixStream> {
        let mut stream = BufReader::new(stream);
        let begin = receive(&mut stream).await;
        assert_eq!(begin["v"], 1);
        assert_eq!(begin["id"], 1);
        assert_eq!(begin["op"], "begin");
        stream
            .get_mut()
            .write_all(b"{\"id\":1,\"ok\":true,\"session\":41}\n")
            .await
            .unwrap();
        stream
    }

    #[tokio::test]
    async fn lost_ack_closes_without_retrying() {
        let socket = TestSocket::new();
        let path = socket.path.clone();
        let server = tokio::spawn(async move {
            let (stream, _) = socket.listener.accept().await.unwrap();
            let mut stream = acknowledge_begin(stream).await;
            let request = receive(&mut stream).await;
            assert_eq!(request["id"], 2);
            assert_eq!(request["op"], "partial");
            assert_eq!(request["text"], "bonjour");

            let mut extra = String::new();
            let bytes = timeout(Duration::from_secs(1), stream.read_line(&mut extra))
                .await
                .expect("client did not close after its ACK timeout")
                .unwrap();
            assert_eq!(bytes, 0, "client retried an ambiguous request: {extra}");
        });

        let mut client = ImeSession::begin_at(&path, Duration::from_millis(50))
            .await
            .unwrap();
        let error = client.partial("bonjour").await.unwrap_err();
        assert!(error.to_string().contains("timed out"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn focus_cancellation_wakes_waiter() {
        let socket = TestSocket::new();
        let path = socket.path.clone();
        let server = tokio::spawn(async move {
            let (stream, _) = socket.listener.accept().await.unwrap();
            let mut stream = acknowledge_begin(stream).await;
            stream
                .get_mut()
                .write_all(
                    b"{\"event\":\"cancelled\",\"session\":41,\"reason\":\"focus_changed\"}\n",
                )
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
        });

        let mut client = ImeSession::begin_at(&path, Duration::from_secs(1))
            .await
            .unwrap();
        timeout(Duration::from_secs(1), client.cancelled())
            .await
            .expect("focus cancellation was not delivered");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn acknowledged_commit_succeeds_before_following_cancellation() {
        let socket = TestSocket::new();
        let path = socket.path.clone();
        let server = tokio::spawn(async move {
            let (stream, _) = socket.listener.accept().await.unwrap();
            let mut stream = acknowledge_begin(stream).await;
            let commit = receive(&mut stream).await;
            assert_eq!(commit["op"], "commit");
            assert_eq!(commit["text"], "bonjour français 👋 café");
            stream
                .get_mut()
                .write_all(
                    b"{\"id\":2,\"ok\":true}\n{\"event\":\"cancelled\",\"session\":41,\"reason\":\"focus_changed\"}\n",
                )
                .await
                .unwrap();
        });

        let mut client = ImeSession::begin_at(&path, Duration::from_secs(1))
            .await
            .unwrap();
        client.commit("bonjour français 👋 café").await.unwrap();
        server.await.unwrap();
        assert!(
            timeout(Duration::from_millis(20), client.cancelled())
                .await
                .is_err(),
            "post-ACK cancellation retroactively invalidated a successful commit"
        );
    }

    #[tokio::test]
    #[ignore = "requires the real IBus bridge and changes the focused application's text"]
    async fn ime_manual_smoke() {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let mut session = ImeSession::begin().await.unwrap();
        for partial in ["bonj", "bonjour fran", "bonjour français 👋", "bonjour français 👋 café"] {
            session.partial(partial).await.unwrap();
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        session.commit("bonjour français 👋 café").await.unwrap();
    }
}
