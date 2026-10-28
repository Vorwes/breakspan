//! Versioned protobuf export recordings. All filesystem work stays on blocking
//! workers; no transport types escape this crate.
use crate::{MAX_MESSAGE_BYTES, ingest_export};
use breakspan_store::MemoryStore;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use prost::Message;
use std::{
    fmt,
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    path::Path,
    sync::{Arc, Mutex},
};
use tokio::sync::watch;

const MAGIC: &[u8; 7] = b"BRKSPAN";
const HEADER: &[u8; 8] = b"BRKSPAN\x01";

/// Total on-disk bytes, including the header and frame lengths.
pub const DEFAULT_MAX_RECORDING_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
pub struct RecordingOptions {
    /// Explicitly truncate an existing file instead of refusing to overwrite it.
    pub force: bool,
    pub max_bytes: u64,
}
impl Default for RecordingOptions {
    fn default() -> Self {
        Self {
            force: false,
            max_bytes: DEFAULT_MAX_RECORDING_BYTES,
        }
    }
}

/// Diagnostics contain no payload, protobuf decoder detail or OS-supplied text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordingError(&'static str);
impl fmt::Display for RecordingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for RecordingError {}

/// Clones share the file, ordering lock and sticky failure notification.
#[derive(Clone)]
pub struct Recorder {
    state: Arc<Mutex<RecordingState>>,
    failure: watch::Sender<Option<RecordingError>>,
}
struct RecordingState {
    file: Option<File>,
    bytes: u64,
    max_bytes: u64,
    buffer: Vec<u8>,
}
impl Recorder {
    /// Create a new recording on a blocking worker. Existing files are untouched
    /// unless force is set. The cap must accommodate the eight-byte header.
    pub async fn create(
        path: impl AsRef<Path>,
        options: RecordingOptions,
    ) -> Result<Self, RecordingError> {
        let path = path.as_ref().to_owned();
        tokio::task::spawn_blocking(move || {
            if options.max_bytes < HEADER.len() as u64 {
                return Err(RecordingError(
                    "recording size cap must be at least 8 bytes",
                ));
            }
            let mut open = OpenOptions::new();
            open.write(true);
            if options.force {
                open.create(true).truncate(true);
            } else {
                open.create_new(true);
            }
            // Private captures should not acquire group/world readability through
            // a permissive umask. Windows uses the containing directory's ACL.
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                open.mode(0o600);
            }
            let mut file = open.open(path).map_err(|error| {
                if error.kind() == io::ErrorKind::AlreadyExists {
                    RecordingError("recording already exists; use --force to overwrite")
                } else {
                    RecordingError("could not create recording file")
                }
            })?;
            file.write_all(HEADER)
                .map_err(|_| RecordingError("could not write recording header"))?;
            let (failure, _) = watch::channel(None);
            Ok(Self {
                state: Arc::new(Mutex::new(RecordingState {
                    file: Some(file),
                    bytes: HEADER.len() as u64,
                    max_bytes: options.max_bytes,
                    buffer: Vec::new(),
                })),
                failure,
            })
        })
        .await
        .map_err(|_| RecordingError("recording creation task failed"))?
    }

    /// Wait for the first recording failure, including one that already occurred.
    /// Recording stays disabled after failure; live ingestion is unaffected.
    pub async fn failed(&self) -> RecordingError {
        let mut failure = self.failure.subscribe();
        loop {
            if let Some(error) = *failure.borrow_and_update() {
                return error;
            }
            if failure.changed().await.is_err() {
                // This handle owns a sender, so closure is impossible while this
                // future is alive. Remain pending rather than spinning if closed.
                return std::future::pending().await;
            }
        }
    }

    // Called only by the receiver's blocking worker. Hold the ordering lock
    // through ingestion: releasing it after writing would let concurrent exports
    // apply replacements or exhaust capacity in a different order than replay.
    pub(crate) fn ingest(&self, request: ExportTraceServiceRequest, store: &MemoryStore) -> u64 {
        match self.state.lock() {
            Ok(mut state) => {
                if state.file.is_some()
                    && let Err(error) = state.append(&request)
                {
                    state.file = None;
                    self.fail(error);
                }
                let rejected = ingest_export(request, store);
                drop(state);
                rejected
            }
            Err(_) => {
                self.fail(RecordingError("recording worker failed; recording stopped"));
                ingest_export(request, store)
            }
        }
    }

    fn fail(&self, error: RecordingError) {
        self.failure.send_if_modified(|failure| {
            if failure.is_some() {
                false
            } else {
                *failure = Some(error);
                true
            }
        });
    }
}
impl RecordingState {
    fn append(&mut self, request: &ExportTraceServiceRequest) -> Result<(), RecordingError> {
        let len = request.encoded_len();
        if len > MAX_MESSAGE_BYTES {
            return Err(RecordingError("recording frame exceeds the 1 MiB limit"));
        }
        let next = self.bytes + 4 + len as u64;
        if next > self.max_bytes {
            return Err(RecordingError(
                "recording size cap reached; recording stopped",
            ));
        }
        self.buffer.clear();
        request
            .encode(&mut self.buffer)
            .map_err(|_| RecordingError("could not encode recording frame; recording stopped"))?;
        if let Some(file) = &mut self.file {
            file.write_all(&(len as u32).to_le_bytes())
                .and_then(|()| file.write_all(&self.buffer))
                .map_err(|_| {
                    RecordingError("could not write recording frame; recording stopped")
                })?;
        }
        self.bytes = next;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadError {
    message: &'static str,
    frame: Option<u64>,
}
impl LoadError {
    fn header(message: &'static str) -> Self {
        Self {
            message,
            frame: None,
        }
    }
    fn frame(message: &'static str, frame: u64) -> Self {
        Self {
            message,
            frame: Some(frame),
        }
    }
}
impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(frame) = self.frame {
            write!(f, "recording frame {frame}: {}", self.message)
        } else {
            write!(f, "recording: {}", self.message)
        }
    }
}
impl std::error::Error for LoadError {}

#[derive(Debug, Default)]
pub struct LoadReport {
    pub frames: u64,
    pub rejected_spans: u64,
    /// Loading stops at the first corrupt/over-limit frame, retaining its valid
    /// prefix. Header errors and errors before any valid frame return Err instead.
    pub warning: Option<LoadError>,
}

/// Replay raw exports through the same validation, normalization and ingestion
/// path as gRPC. The caller supplies a fresh store and chooses its memory limits.
/// Filesystem access, decoding and normalization run on one blocking worker.
pub async fn load(
    path: impl AsRef<Path>,
    store: MemoryStore,
    max_bytes: u64,
) -> Result<LoadReport, LoadError> {
    let path = path.as_ref().to_owned();
    tokio::task::spawn_blocking(move || {
        if max_bytes < HEADER.len() as u64 {
            return Err(LoadError::header("size cap must be at least 8 bytes"));
        }
        let mut file = File::open(path).map_err(|_| LoadError::header("could not open file"))?;
        let mut header = [0; HEADER.len()];
        file.read_exact(&mut header).map_err(|error| {
            LoadError::header(if error.kind() == io::ErrorKind::UnexpectedEof {
                "truncated header"
            } else {
                "could not read header"
            })
        })?;
        if &header[..MAGIC.len()] != MAGIC {
            return Err(LoadError::header("bad magic"));
        }
        if header[MAGIC.len()] != HEADER[MAGIC.len()] {
            return Err(LoadError::header("unknown format version"));
        }
        let mut report = LoadReport::default();
        let mut bytes = HEADER.len() as u64;
        let mut payload = Vec::new();
        loop {
            match read_frame(
                &mut file,
                &mut payload,
                &mut bytes,
                max_bytes,
                report.frames + 1,
            ) {
                Ok(false) => return Ok(report),
                Ok(true) => {}
                Err(error) => return partial_or_error(report, error),
            }
            let request = match ExportTraceServiceRequest::decode(payload.as_slice()) {
                Ok(request) => request,
                Err(_) => {
                    let error = LoadError::frame("undecodable protobuf", report.frames + 1);
                    return partial_or_error(report, error);
                }
            };
            report.rejected_spans += ingest_export(request, &store);
            report.frames += 1;
        }
    })
    .await
    .map_err(|_| LoadError::header("loading task failed"))?
}

fn partial_or_error(mut report: LoadReport, error: LoadError) -> Result<LoadReport, LoadError> {
    if report.frames == 0 {
        Err(error)
    } else {
        report.warning = Some(error);
        Ok(report)
    }
}

// Check length and the cumulative budget before resizing or decoding. An initial
// read distinguishes clean EOF from a partially written length prefix.
fn read_frame(
    file: &mut File,
    payload: &mut Vec<u8>,
    bytes: &mut u64,
    max_bytes: u64,
    frame: u64,
) -> Result<bool, LoadError> {
    let mut length = [0; 4];
    let read = loop {
        match file.read(&mut length) {
            Ok(0) => return Ok(false),
            Ok(read) => break read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(LoadError::frame("could not read frame length", frame)),
        }
    };
    if *bytes + 4 > max_bytes {
        return Err(LoadError::frame("total size cap exceeded", frame));
    }
    file.read_exact(&mut length[read..]).map_err(|error| {
        LoadError::frame(
            if error.kind() == io::ErrorKind::UnexpectedEof {
                "truncated frame length"
            } else {
                "could not read frame length"
            },
            frame,
        )
    })?;
    let len = u32::from_le_bytes(length) as usize;
    if len > MAX_MESSAGE_BYTES {
        return Err(LoadError::frame("frame exceeds the 1 MiB limit", frame));
    }
    let next = *bytes + 4 + len as u64;
    if next > max_bytes {
        return Err(LoadError::frame("total size cap exceeded", frame));
    }
    payload.resize(len, 0);
    file.read_exact(payload).map_err(|error| {
        LoadError::frame(
            if error.kind() == io::ErrorKind::UnexpectedEof {
                "truncated frame payload"
            } else {
                "could not read frame payload"
            },
            frame,
        )
    })?;
    *bytes = next;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry_proto::tonic::{
        collector::trace::v1::trace_service_client::TraceServiceClient,
        trace::v1::{ResourceSpans, ScopeSpans, Span},
    };
    use std::{
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        time::Duration,
    };
    use tokio::{net::TcpListener, sync::oneshot, time::timeout};

    struct TestFile(PathBuf);
    impl Drop for TestFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[tokio::test]
    async fn write_failure_stops_recording_not_live_ingestion() {
        // A read-only descriptor deterministically fails writes on both Unix and
        // Windows, unlike /dev/full, chmod or disk-space-dependent fault tests.
        let (file, recorder) = tokio::task::spawn_blocking(|| {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "breakspan-write-fault-{}-{}.bspan",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .unwrap();
            output.write_all(HEADER).unwrap();
            drop(output);
            let file = File::open(&path).unwrap();
            let (failure, _) = watch::channel(None);
            let recorder = Recorder {
                state: Arc::new(Mutex::new(RecordingState {
                    file: Some(file),
                    bytes: HEADER.len() as u64,
                    max_bytes: DEFAULT_MAX_RECORDING_BYTES,
                    buffer: Vec::new(),
                })),
                failure,
            };
            (TestFile(path), recorder)
        })
        .await
        .unwrap();
        let store = MemoryStore::default();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(crate::serve(
            listener,
            store.clone(),
            Some(recorder.clone()),
            async {
                let _ = stopped.await;
            },
        ));
        let mut client = timeout(
            Duration::from_secs(5),
            TraceServiceClient::connect(format!("http://{address}")),
        )
        .await
        .unwrap()
        .unwrap();
        for id in [1, 2] {
            let request = ExportTraceServiceRequest {
                resource_spans: vec![ResourceSpans {
                    scope_spans: vec![ScopeSpans {
                        spans: vec![Span {
                            trace_id: vec![1; 16],
                            span_id: vec![id; 8],
                            name: format!("synthetic-{id}"),
                            start_time_unix_nano: 1,
                            end_time_unix_nano: 2,
                            ..Default::default()
                        }],
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
            };
            assert!(
                timeout(Duration::from_secs(5), client.export(request))
                    .await
                    .unwrap()
                    .unwrap()
                    .into_inner()
                    .partial_success
                    .is_none()
            );
        }
        let failure = timeout(Duration::from_secs(5), recorder.failed())
            .await
            .unwrap();
        assert_eq!(recorder.failed().await, failure);
        let traces = store.snapshots().await;
        let names: Vec<_> = traces[0]
            .spans()
            .values()
            .map(|span| span.name.as_str())
            .collect();
        assert_eq!(names, ["synthetic-1", "synthetic-2"]);
        drop(client);
        stop.send(()).unwrap();
        timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        drop(recorder);
        tokio::task::spawn_blocking(move || {
            assert_eq!(std::fs::read(&file.0).unwrap(), HEADER);
        })
        .await
        .unwrap();
    }
}
