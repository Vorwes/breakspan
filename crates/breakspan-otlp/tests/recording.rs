//! Recording/reloading behavior through the real loopback OTLP/gRPC boundary.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers panic on setup failure; clippy's test exemption covers only #[test] functions"
)]
use breakspan_core::{SpanId, TraceId, render_tree};
use breakspan_otlp::{DEFAULT_MAX_RECORDING_BYTES, Recorder, RecordingOptions, load};
use breakspan_store::{MemoryStore, StoreLimits};
use opentelemetry_proto::tonic::{
    collector::trace::v1::{
        ExportTraceServiceRequest, ExportTraceServiceResponse,
        trace_service_client::TraceServiceClient,
    },
    common::v1::{AnyValue, KeyValue, any_value::Value},
    trace::v1::{ResourceSpans, ScopeSpans, Span, Status},
};
use prost::{Message, bytes::BufMut};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use tokio::{net::TcpListener, sync::oneshot, task::JoinHandle, time::timeout};
use tonic::{
    codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder},
    transport::Channel,
};

const HEADER: &[u8; 8] = b"BRKSPAN\x01";
const MAX_FRAME_BYTES: usize = 1024 * 1024;
const PRIVATE_CONTENT: &str = "PRIVATE_TELEMETRY_CONTENT";
const EXPECTED_TREE: &str = "Trace 01010101010101010101010101010101 (snapshot, 3 spans)\n\
synthetic agent  4.21s\n\
├── synthetic model replacement  6ms\n\
└── synthetic tool  6ms [error]\n";

struct TempDir(PathBuf);
impl TempDir {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let path = std::env::temp_dir().join(format!(
                "breakspan-recording-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("cannot create test directory: {error}"),
            }
        }
    }
    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct TestReceiver {
    client: TraceServiceClient<Channel>,
    channel: Channel,
    address: std::net::SocketAddr,
    store: MemoryStore,
    recorder: Recorder,
    stop: oneshot::Sender<()>,
    task: JoinHandle<Result<(), breakspan_otlp::ReceiverError>>,
}
impl TestReceiver {
    async fn start(path: &Path, options: RecordingOptions, store: MemoryStore) -> Self {
        let recorder = Recorder::create(path, options).await.unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(breakspan_otlp::serve(
            listener,
            store.clone(),
            Some(recorder.clone()),
            async {
                let _ = stopped.await;
            },
        ));
        let channel = timeout(
            Duration::from_secs(5),
            Channel::from_shared(format!("http://{address}"))
                .unwrap()
                .connect(),
        )
        .await
        .unwrap()
        .unwrap();
        Self {
            client: TraceServiceClient::new(channel.clone()),
            channel,
            address,
            store,
            recorder,
            stop,
            task,
        }
    }
    async fn export(&mut self, export: ExportTraceServiceRequest) -> ExportTraceServiceResponse {
        timeout(Duration::from_secs(5), self.client.export(export))
            .await
            .unwrap()
            .unwrap()
            .into_inner()
    }
    async fn shutdown(self) {
        drop(self.client);
        drop(self.channel);
        self.stop.send(()).unwrap();
        timeout(Duration::from_secs(5), self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        drop(self.recorder);
    }
}

fn span(id: u8, parent: Option<u8>, operation: &str, name: &str) -> Span {
    Span {
        trace_id: vec![1; 16],
        span_id: vec![id; 8],
        parent_span_id: parent.map(|id| vec![id; 8]).unwrap_or_default(),
        name: name.into(),
        start_time_unix_nano: u64::from(id) * 10_000_000,
        end_time_unix_nano: u64::from(id) * 10_000_000 + 6_000_000,
        attributes: vec![KeyValue {
            key: "gen_ai.operation.name".into(),
            value: Some(AnyValue {
                value: Some(Value::StringValue(operation.into())),
            }),
            ..Default::default()
        }],
        ..Default::default()
    }
}
fn request(spans: Vec<Span>) -> ExportTraceServiceRequest {
    ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            scope_spans: vec![ScopeSpans {
                spans,
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
}
fn trace_id() -> TraceId {
    TraceId::new([1; 16]).unwrap()
}
fn length_prefix(len: usize) -> [u8; 4] {
    u32::try_from(len).unwrap().to_le_bytes()
}
fn frame(export: &ExportTraceServiceRequest) -> Vec<u8> {
    let body = export.encode_to_vec();
    let mut frame = length_prefix(body.len()).to_vec();
    frame.extend_from_slice(&body);
    frame
}
fn recording(exports: &[ExportTraceServiceRequest]) -> Vec<u8> {
    let mut bytes = HEADER.to_vec();
    for export in exports {
        bytes.extend_from_slice(&frame(export));
    }
    bytes
}
fn assert_safe(error: impl std::fmt::Display) -> String {
    let message = error.to_string();
    assert!(
        !message.contains(PRIVATE_CONTENT),
        "error exposed telemetry: {message}"
    );
    message
}

async fn record_synthetic(path: &Path) -> String {
    let mut receiver =
        TestReceiver::start(path, RecordingOptions::default(), MemoryStore::default()).await;
    let model = span(2, Some(1), "chat", "superseded model");
    let mut tool = span(3, Some(1), "execute_tool", "synthetic tool");
    tool.status = Some(Status {
        code: 2,
        message: "synthetic failure".into(),
    });
    let first = request(vec![tool, model]);
    assert!(
        receiver
            .export(first.clone())
            .await
            .partial_success
            .is_none()
    );
    assert!(
        render_tree(&receiver.store.trace(trace_id()).await.unwrap()).contains("missing parent")
    );
    // A successful export means its whole frame is already written, without shutdown/flush.
    assert_eq!(fs::read(path).unwrap(), recording(&[first]));

    let mut parent = span(1, None, "invoke_agent", "synthetic agent");
    parent.end_time_unix_nano = 4_220_000_000;
    let second = request(vec![parent]);
    assert!(receiver.export(second).await.partial_success.is_none());
    let replacement = request(vec![span(
        2,
        Some(1),
        "chat",
        "synthetic model replacement",
    )]);
    assert!(receiver.export(replacement).await.partial_success.is_none());
    let live = render_tree(&receiver.store.trace(trace_id()).await.unwrap());
    assert_eq!(live, EXPECTED_TREE);
    receiver.shutdown().await;
    live
}

#[tokio::test]
async fn live_recording_reload_repairs_late_parent_and_preserves_last_write_wins() {
    let temp = TempDir::new();
    let path = temp.file("synthetic.bspan");
    let live = record_synthetic(&path).await;
    let store = MemoryStore::default();
    let report = load(&path, store.clone(), DEFAULT_MAX_RECORDING_BYTES)
        .await
        .unwrap();
    assert_eq!(report.frames, 3);
    assert_eq!(report.rejected_spans, 0);
    assert!(report.warning.is_none());
    assert_eq!(render_tree(&store.trace(trace_id()).await.unwrap()), live);
}

#[tokio::test]
async fn concurrent_connections_replay_the_same_replacements_and_capacity_winner() {
    let temp = TempDir::new();
    let path = temp.file("concurrent.bspan");
    let limits = StoreLimits {
        max_spans: 2,
        ..StoreLimits::default()
    };
    let mut receiver =
        TestReceiver::start(&path, RecordingOptions::default(), MemoryStore::new(limits)).await;
    // connect() creates a second HTTP/2 connection, not another handle to the
    // first connection's queue/concurrency limit.
    let mut second = timeout(
        Duration::from_secs(5),
        TraceServiceClient::connect(format!("http://{}", receiver.address)),
    )
    .await
    .unwrap()
    .unwrap();
    let mut rejected = 0;
    for round in 0..16 {
        let left = request(vec![
            span(1, None, "chat", &format!("left replacement {round}")),
            span(2, Some(1), "execute_tool", "left capacity contender"),
        ]);
        let right = request(vec![
            span(1, None, "chat", &format!("right replacement {round}")),
            span(3, Some(1), "execute_tool", "right capacity contender"),
        ]);
        let (left, right) = timeout(Duration::from_secs(5), async {
            tokio::join!(receiver.client.export(left), second.export(right))
        })
        .await
        .unwrap();
        for response in [left.unwrap().into_inner(), right.unwrap().into_inner()] {
            rejected += response
                .partial_success
                .map_or(0, |partial| partial.rejected_spans as u64);
        }
        let replayed = MemoryStore::new(limits);
        let report = load(&path, replayed.clone(), DEFAULT_MAX_RECORDING_BYTES)
            .await
            .unwrap();
        assert_eq!(report.frames, (round + 1) * 2);
        assert_eq!(report.rejected_spans, rejected);
        assert!(report.warning.is_none());
        assert_eq!(replayed.snapshots().await, receiver.store.snapshots().await);
        assert_eq!(
            receiver
                .store
                .trace(trace_id())
                .await
                .unwrap()
                .spans()
                .len(),
            2
        );
    }
    drop(second);
    receiver.shutdown().await;
}

#[tokio::test]
async fn checked_in_synthetic_fixture_matches_known_rendered_tree() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/synthetic.bspan");
    let store = MemoryStore::default();
    let report = load(&path, store.clone(), DEFAULT_MAX_RECORDING_BYTES)
        .await
        .unwrap();
    assert_eq!(report.frames, 3);
    assert_eq!(report.rejected_spans, 0);
    assert!(report.warning.is_none());
    assert_eq!(
        render_tree(&store.trace(trace_id()).await.unwrap()),
        EXPECTED_TREE
    );
}

#[tokio::test]
async fn loaded_recordings_produce_step_names() {
    let named = |id, operation, key: &str, value: &str| {
        let mut span = span(id, Some(1), operation, "step");
        span.attributes.push(KeyValue {
            key: key.into(),
            value: Some(AnyValue {
                value: Some(Value::StringValue(value.into())),
            }),
            ..Default::default()
        });
        span
    };
    let mut agent = named(1, "invoke_agent", "gen_ai.agent.name", "planner");
    agent.parent_span_id.clear();
    let export = request(vec![
        agent,
        named(2, "chat", "gen_ai.request.model", "synthetic-model"),
        named(3, "execute_tool", "gen_ai.tool.name", "calculator"),
        // A tool name on a model span is ignored.
        named(4, "chat", "gen_ai.tool.name", "calculator"),
    ]);
    let temp = TempDir::new();
    let path = temp.file("named.bspan");
    fs::write(&path, recording(&[export])).unwrap();
    let store = MemoryStore::default();
    let report = load(&path, store.clone(), DEFAULT_MAX_RECORDING_BYTES)
        .await
        .unwrap();
    assert_eq!(report.frames, 1);
    assert_eq!(report.rejected_spans, 0);
    assert!(report.warning.is_none());
    let trace = store.trace(trace_id()).await.unwrap();
    let names: Vec<_> = (1..=4)
        .map(|id| {
            trace.spans()[&SpanId::new([id; 8]).unwrap()]
                .step_name
                .as_deref()
        })
        .collect();
    assert_eq!(
        names,
        [
            Some("planner"),
            Some("synthetic-model"),
            Some("calculator"),
            None
        ]
    );
}

#[tokio::test]
async fn semantically_rejected_spans_are_recorded_and_revalidated() {
    let temp = TempDir::new();
    let path = temp.file("rejected.bspan");
    let mut receiver =
        TestReceiver::start(&path, RecordingOptions::default(), MemoryStore::default()).await;
    let mut invalid_id = span(1, None, "chat", PRIVATE_CONTENT);
    invalid_id.trace_id = vec![0; 16];
    let mut invalid_timing = span(2, None, "chat", PRIVATE_CONTENT);
    invalid_timing.end_time_unix_nano = 0;
    let export = request(vec![
        invalid_id,
        invalid_timing,
        span(3, None, "chat", "valid sibling"),
    ]);
    let partial = receiver
        .export(export.clone())
        .await
        .partial_success
        .unwrap();
    assert_eq!(partial.rejected_spans, 2);
    assert_safe(partial.error_message);
    let live = render_tree(&receiver.store.trace(trace_id()).await.unwrap());
    receiver.shutdown().await;
    assert_eq!(fs::read(&path).unwrap(), recording(&[export]));
    let loaded = MemoryStore::default();
    let report = load(&path, loaded.clone(), DEFAULT_MAX_RECORDING_BYTES)
        .await
        .unwrap();
    assert_eq!(report.frames, 1);
    assert_eq!(report.rejected_spans, 2);
    assert!(report.warning.is_none());
    assert_eq!(render_tree(&loaded.trace(trace_id()).await.unwrap()), live);
}

#[tokio::test]
async fn load_reapplies_current_store_capacity_not_live_rejections() {
    let temp = TempDir::new();
    let path = temp.file("capacity.bspan");
    let limits = StoreLimits {
        max_spans: 1,
        ..StoreLimits::default()
    };
    let mut receiver =
        TestReceiver::start(&path, RecordingOptions::default(), MemoryStore::new(limits)).await;
    let response = receiver
        .export(request(vec![
            span(1, None, "chat", "first"),
            span(2, None, "chat", "second"),
        ]))
        .await;
    assert_eq!(response.partial_success.unwrap().rejected_spans, 1);
    receiver.shutdown().await;
    let roomy = MemoryStore::default();
    let report = load(&path, roomy.clone(), DEFAULT_MAX_RECORDING_BYTES)
        .await
        .unwrap();
    assert_eq!(report.rejected_spans, 0);
    assert_eq!(roomy.trace(trace_id()).await.unwrap().spans().len(), 2);
    let small = MemoryStore::new(limits);
    let report = load(&path, small.clone(), DEFAULT_MAX_RECORDING_BYTES)
        .await
        .unwrap();
    assert_eq!(report.rejected_spans, 1);
    assert_eq!(small.trace(trace_id()).await.unwrap().spans().len(), 1);
}

#[tokio::test]
async fn bad_magic_version_and_every_truncated_header_are_errors() {
    let temp = TempDir::new();
    let path = temp.file(&format!("{PRIVATE_CONTENT}.bspan"));
    let mut bad_magic = HEADER.to_vec();
    bad_magic[0] = b'X';
    let mut bad_version = HEADER.to_vec();
    bad_version[7] = 2;
    let mut invalid_headers = vec![bad_magic, bad_version];
    invalid_headers.extend((0..HEADER.len()).map(|length| HEADER[..length].to_vec()));
    for header in invalid_headers {
        fs::write(&path, header).unwrap();
        let store = MemoryStore::default();
        let error = load(&path, store.clone(), DEFAULT_MAX_RECORDING_BYTES)
            .await
            .err()
            .unwrap();
        assert_safe(error);
        assert!(store.snapshots().await.is_empty());
    }
}

#[tokio::test]
async fn malformed_frames_error_without_prefix_and_warn_after_complete_prefix() {
    let temp = TempDir::new();
    let path = temp.file(&format!("{PRIVATE_CONTENT}.bspan"));
    let good = request(vec![span(1, None, "chat", PRIVATE_CONTENT)]);
    let good_bytes = recording(&[good]);
    let mut truncated_body = length_prefix(PRIVATE_CONTENT.len() + 1).to_vec();
    truncated_body.extend_from_slice(PRIVATE_CONTENT.as_bytes());
    let mut invalid_prost = length_prefix(PRIVATE_CONTENT.len() + 1).to_vec();
    invalid_prost.push(0x0f); // Field 1 with illegal protobuf wire type 7.
    invalid_prost.extend_from_slice(PRIVATE_CONTENT.as_bytes());
    let mut oversize = length_prefix(MAX_FRAME_BYTES + 1).to_vec();
    oversize.extend_from_slice(PRIVATE_CONTENT.as_bytes());
    let mut corruptions = vec![truncated_body, invalid_prost, oversize];
    corruptions.extend((1..4).map(|length| vec![0xff; length]));
    for corruption in corruptions {
        let mut without_prefix = HEADER.to_vec();
        without_prefix.extend_from_slice(&corruption);
        fs::write(&path, without_prefix).unwrap();
        let empty_store = MemoryStore::default();
        let error = load(&path, empty_store.clone(), DEFAULT_MAX_RECORDING_BYTES)
            .await
            .err()
            .unwrap();
        assert_safe(error);
        assert!(empty_store.snapshots().await.is_empty());

        let mut with_prefix = good_bytes.clone();
        with_prefix.extend_from_slice(&corruption);
        fs::write(&path, with_prefix).unwrap();
        let store = MemoryStore::default();
        let report = load(&path, store.clone(), DEFAULT_MAX_RECORDING_BYTES)
            .await
            .unwrap();
        assert_eq!(report.frames, 1);
        assert_eq!(report.rejected_spans, 0);
        assert_safe(report.warning.unwrap());
        let trace = store.trace(trace_id()).await.unwrap();
        assert_eq!(trace.spans().len(), 1);
        assert_eq!(
            trace.spans()[&SpanId::new([1; 8]).unwrap()].name,
            PRIVATE_CONTENT
        );
    }
}

#[tokio::test]
async fn decoded_empty_frames_count_as_prefix_even_when_no_spans_are_accepted() {
    let temp = TempDir::new();
    let path = temp.file("empty.bspan");
    let mut bytes = recording(&[ExportTraceServiceRequest::default()]);
    bytes.push(0xff); // Truncated next length.
    fs::write(&path, bytes).unwrap();
    let store = MemoryStore::default();
    let report = load(&path, store.clone(), DEFAULT_MAX_RECORDING_BYTES)
        .await
        .unwrap();
    assert_eq!(report.frames, 1);
    assert_eq!(report.rejected_spans, 0);
    assert!(report.warning.is_some());
    assert!(store.snapshots().await.is_empty());

    let mut invalid = span(1, None, "chat", PRIVATE_CONTENT);
    invalid.trace_id.clear();
    let mut bytes = recording(&[request(vec![invalid])]);
    bytes.push(0xff);
    fs::write(&path, bytes).unwrap();
    let report = load(&path, store.clone(), DEFAULT_MAX_RECORDING_BYTES)
        .await
        .unwrap();
    assert_eq!(report.frames, 1);
    assert_eq!(report.rejected_spans, 1);
    assert_safe(report.warning.unwrap());
    assert!(store.snapshots().await.is_empty());
}

#[tokio::test]
async fn maximum_one_mebibyte_frame_length_is_inclusive() {
    let temp = TempDir::new();
    let path = temp.file("maximum-frame.bspan");
    // Unknown length-delimited protobuf field 2 with a 1,048,572-byte value:
    // one tag byte + three length bytes + value = exactly 1 MiB.
    let mut payload = vec![0; MAX_FRAME_BYTES];
    payload[..4].copy_from_slice(&[0x12, 0xfc, 0xff, 0x3f]);
    let mut bytes = HEADER.to_vec();
    bytes.extend_from_slice(&length_prefix(MAX_FRAME_BYTES));
    bytes.extend_from_slice(&payload);
    fs::write(&path, &bytes).unwrap();
    let store = MemoryStore::default();
    let report = load(&path, store.clone(), bytes.len() as u64)
        .await
        .unwrap();
    assert_eq!(report.frames, 1);
    assert_eq!(report.rejected_spans, 0);
    assert!(report.warning.is_none());
    assert!(store.snapshots().await.is_empty());
    assert!(
        load(&path, MemoryStore::default(), bytes.len() as u64 - 1)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn load_caps_include_header_and_lengths_with_incremental_prefix_recovery() {
    let temp = TempDir::new();
    let path = temp.file("boundaries.bspan");
    fs::write(&path, HEADER).unwrap();
    assert!(load(&path, MemoryStore::default(), 7).await.is_err());
    let report = load(&path, MemoryStore::default(), 8).await.unwrap();
    assert_eq!(report.frames, 0);
    assert!(report.warning.is_none());

    let empty = ExportTraceServiceRequest::default();
    fs::write(&path, recording(std::slice::from_ref(&empty))).unwrap();
    assert!(load(&path, MemoryStore::default(), 11).await.is_err());
    let report = load(&path, MemoryStore::default(), 12).await.unwrap();
    assert_eq!(report.frames, 1);
    assert!(report.warning.is_none());

    fs::write(&path, recording(&[empty.clone(), empty])).unwrap();
    for cap in [12, 15] {
        let report = load(&path, MemoryStore::default(), cap).await.unwrap();
        assert_eq!(report.frames, 1);
        assert_safe(report.warning.unwrap());
    }
    let report = load(&path, MemoryStore::default(), 16).await.unwrap();
    assert_eq!(report.frames, 2);
    assert!(report.warning.is_none());

    let first = request(vec![span(1, None, "chat", "retained prefix")]);
    let second = request(vec![span(2, None, "chat", "outside cap")]);
    let first_bytes = recording(std::slice::from_ref(&first));
    let first_size = first_bytes.len() as u64;
    fs::write(&path, first_bytes).unwrap();
    assert!(
        load(&path, MemoryStore::default(), first_size - 1)
            .await
            .is_err()
    );
    assert!(
        load(&path, MemoryStore::default(), first_size)
            .await
            .unwrap()
            .warning
            .is_none()
    );
    fs::write(&path, recording(&[first, second])).unwrap();
    let store = MemoryStore::default();
    let report = load(&path, store.clone(), first_size).await.unwrap();
    assert_eq!(report.frames, 1);
    assert_safe(report.warning.unwrap());
    let trace = store.trace(trace_id()).await.unwrap();
    assert_eq!(trace.spans().len(), 1);
    assert_eq!(
        trace.spans()[&SpanId::new([1; 8]).unwrap()].name,
        "retained prefix"
    );
}

#[tokio::test]
async fn recording_cap_failure_is_sticky_and_does_not_stop_live_ingestion() {
    let temp = TempDir::new();
    let path = temp.file("limited.bspan");
    let first = request(vec![span(1, None, "chat", "recorded")]);
    let expected = recording(std::slice::from_ref(&first));
    let mut receiver = TestReceiver::start(
        &path,
        RecordingOptions {
            force: false,
            max_bytes: expected.len() as u64,
        },
        MemoryStore::default(),
    )
    .await;
    assert!(receiver.export(first).await.partial_success.is_none());
    assert_eq!(fs::read(&path).unwrap(), expected);
    assert!(
        receiver
            .export(request(vec![span(2, None, "chat", PRIVATE_CONTENT)]))
            .await
            .partial_success
            .is_none()
    );
    let failure = timeout(Duration::from_secs(5), receiver.recorder.failed())
        .await
        .unwrap();
    let first_failure = assert_safe(failure);
    assert!(
        receiver
            .export(request(vec![span(3, None, "chat", "still live")]))
            .await
            .partial_success
            .is_none()
    );
    let failure = timeout(Duration::from_secs(5), receiver.recorder.failed())
        .await
        .unwrap();
    assert_eq!(assert_safe(failure), first_failure);
    assert_eq!(
        receiver
            .store
            .trace(trace_id())
            .await
            .unwrap()
            .spans()
            .len(),
        3
    );
    assert_eq!(fs::read(&path).unwrap(), expected);
    receiver.shutdown().await;
    let store = MemoryStore::default();
    let report = load(&path, store.clone(), DEFAULT_MAX_RECORDING_BYTES)
        .await
        .unwrap();
    assert_eq!(report.frames, 1);
    assert!(report.warning.is_none());
    assert_eq!(store.trace(trace_id()).await.unwrap().spans().len(), 1);
}

#[tokio::test]
async fn header_only_recording_cap_allows_live_exports_but_records_no_frames() {
    let temp = TempDir::new();
    let path = temp.file("header-only.bspan");
    let mut receiver = TestReceiver::start(
        &path,
        RecordingOptions {
            force: false,
            max_bytes: 8,
        },
        MemoryStore::default(),
    )
    .await;
    assert!(
        receiver
            .export(ExportTraceServiceRequest::default())
            .await
            .partial_success
            .is_none()
    );
    assert_safe(
        timeout(Duration::from_secs(5), receiver.recorder.failed())
            .await
            .unwrap(),
    );
    assert!(
        receiver
            .export(request(vec![span(1, None, "chat", "still accepted")]))
            .await
            .partial_success
            .is_none()
    );
    assert_eq!(
        receiver
            .store
            .trace(trace_id())
            .await
            .unwrap()
            .spans()
            .len(),
        1
    );
    receiver.shutdown().await;
    assert_eq!(fs::read(&path).unwrap(), HEADER);
}

#[tokio::test]
async fn recorder_refuses_overwrite_and_force_replaces_only_when_explicit() {
    let temp = TempDir::new();
    let path = temp.file(&format!("{PRIVATE_CONTENT}.bspan"));
    let original = recording(&[request(vec![span(1, None, "chat", "preserved")])]);
    fs::write(&path, &original).unwrap();
    let error = Recorder::create(&path, RecordingOptions::default())
        .await
        .err()
        .unwrap();
    assert_safe(error);
    assert_eq!(fs::read(&path).unwrap(), original);
    // Reject an invalid cap before replacing an existing destination.
    let error = Recorder::create(
        &path,
        RecordingOptions {
            force: true,
            max_bytes: 7,
        },
    )
    .await
    .err()
    .unwrap();
    assert_safe(error);
    assert_eq!(fs::read(&path).unwrap(), original);
    let recorder = Recorder::create(
        &path,
        RecordingOptions {
            force: true,
            ..RecordingOptions::default()
        },
    )
    .await
    .unwrap();
    drop(recorder);
    assert_eq!(fs::read(&path).unwrap(), HEADER);
    let report = load(&path, MemoryStore::default(), 8).await.unwrap();
    assert_eq!(report.frames, 0);
    assert!(report.warning.is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn force_replaces_permissive_files_and_symlinks_with_a_private_file() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let temp = TempDir::new();
    let force = RecordingOptions {
        force: true,
        ..RecordingOptions::default()
    };
    let path = temp.file("permissive.bspan");
    fs::write(&path, PRIVATE_CONTENT).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    drop(Recorder::create(&path, force).await.unwrap());
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(fs::read(&path).unwrap(), HEADER);

    let target = temp.file("shared-target");
    fs::write(&target, PRIVATE_CONTENT).unwrap();
    let link = temp.file("link.bspan");
    symlink(&target, &link).unwrap();
    drop(Recorder::create(&link, force).await.unwrap());
    assert!(fs::symlink_metadata(&link).unwrap().file_type().is_file());
    assert_eq!(fs::read(&link).unwrap(), HEADER);
    assert_eq!(fs::read_to_string(&target).unwrap(), PRIVATE_CONTENT);
}

// A raw client lets malformed protobuf reach tonic's decoder rather than making
// an impossible invalid message with the generated, typed client.
struct RawExportCodec;
struct RawExportEncoder;
struct ExportResponseDecoder;
impl Codec for RawExportCodec {
    type Encode = Vec<u8>;
    type Decode = ExportTraceServiceResponse;
    type Encoder = RawExportEncoder;
    type Decoder = ExportResponseDecoder;
    fn encoder(&mut self) -> Self::Encoder {
        RawExportEncoder
    }
    fn decoder(&mut self) -> Self::Decoder {
        ExportResponseDecoder
    }
}
impl Encoder for RawExportEncoder {
    type Item = Vec<u8>;
    type Error = tonic::Status;
    fn encode(&mut self, item: Self::Item, dst: &mut EncodeBuf<'_>) -> Result<(), Self::Error> {
        dst.put_slice(&item);
        Ok(())
    }
}
impl Decoder for ExportResponseDecoder {
    type Item = ExportTraceServiceResponse;
    type Error = tonic::Status;
    fn decode(&mut self, src: &mut DecodeBuf<'_>) -> Result<Option<Self::Item>, Self::Error> {
        ExportTraceServiceResponse::decode(src)
            .map(Some)
            .map_err(|_| tonic::Status::internal("invalid export response"))
    }
}

#[tokio::test]
async fn malformed_and_oversize_grpc_requests_are_excluded_from_recording() {
    let temp = TempDir::new();
    let path = temp.file("transport-rejected.bspan");
    let mut receiver =
        TestReceiver::start(&path, RecordingOptions::default(), MemoryStore::default()).await;
    let mut raw = tonic::client::Grpc::new(receiver.channel.clone());
    timeout(Duration::from_secs(5), raw.ready())
        .await
        .unwrap()
        .unwrap();
    let malformed = timeout(
        Duration::from_secs(5),
        raw.unary(
            tonic::Request::new(vec![0x0f]),
            tonic::codegen::http::uri::PathAndQuery::from_static(
                "/opentelemetry.proto.collector.trace.v1.TraceService/Export",
            ),
            RawExportCodec,
        ),
    )
    .await
    .unwrap();
    assert!(malformed.is_err());
    assert_eq!(fs::read(&path).unwrap(), HEADER);
    let huge = request(vec![span(1, None, "chat", &"x".repeat(MAX_FRAME_BYTES))]);
    assert!(
        timeout(Duration::from_secs(5), receiver.client.export(huge))
            .await
            .unwrap()
            .is_err()
    );
    assert_eq!(fs::read(&path).unwrap(), HEADER);
    let healthy = request(vec![span(2, None, "chat", "healthy")]);
    assert!(
        receiver
            .export(healthy.clone())
            .await
            .partial_success
            .is_none()
    );
    drop(raw);
    receiver.shutdown().await;
    assert_eq!(fs::read(&path).unwrap(), recording(&[healthy]));
    let report = load(&path, MemoryStore::default(), DEFAULT_MAX_RECORDING_BYTES)
        .await
        .unwrap();
    assert_eq!(report.frames, 1);
    assert_eq!(report.rejected_spans, 0);
    assert!(report.warning.is_none());
}

#[tokio::test]
#[ignore = "explicitly regenerates the checked-in synthetic trace through real OTLP/gRPC"]
async fn regenerate_synthetic_fixture() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/synthetic.bspan");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    // The test helper uses create_new; replace only this explicitly synthetic fixture.
    match fs::remove_file(&path) {
        Ok(()) => (),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => panic!("cannot replace synthetic fixture: {error}"),
    }
    let live = record_synthetic(&path).await;
    let store = MemoryStore::default();
    let report = load(&path, store.clone(), DEFAULT_MAX_RECORDING_BYTES)
        .await
        .unwrap();
    assert_eq!(report.frames, 3);
    assert_eq!(report.rejected_spans, 0);
    assert!(report.warning.is_none());
    assert_eq!(render_tree(&store.trace(trace_id()).await.unwrap()), live);
    eprintln!("BEGIN EXPECTED CLI STDOUT\n{live}END EXPECTED CLI STDOUT");
}

/// A scenario name with its passing and failing export.
type FailurePair = (
    &'static str,
    ExportTraceServiceRequest,
    ExportTraceServiceRequest,
);

/// Synthetic passing/failing run pairs for common agent failure shapes, read by
/// the CLI `diff` tests and `docs/failure-pairs.md`. Only operation and
/// step-name attributes are set; there are no prompts, arguments or results.
fn failure_pairs() -> [FailurePair; 5] {
    const TOOL: &str = "calculator";
    let pass = || run(vec![agent(), model(2, 1), tool(3, 1, TOOL), model(4, 1)]);
    let graph_run = |tool_status: fn(Span) -> Span| {
        run(vec![
            agent(),
            graph(2, 1, "agent graph"),
            model(3, 2),
            graph(4, 2, "tool node"),
            tool_status(tool(5, 4, TOOL)),
            model(6, 2),
        ])
    };
    [
        (
            "tool-error-recovery",
            pass(),
            run(vec![
                agent(),
                model(2, 1),
                failed(tool(3, 1, TOOL)),
                model(4, 1),
                tool(5, 1, TOOL),
                model(6, 1),
            ]),
        ),
        (
            "tool-loop",
            pass(),
            run(vec![
                agent(),
                model(2, 1),
                tool(3, 1, TOOL),
                model(4, 1),
                tool(5, 1, TOOL),
                model(6, 1),
                tool(7, 1, TOOL),
                model(8, 1),
            ]),
        ),
        ("early-stop", pass(), run(vec![agent(), model(2, 1)])),
        (
            "wrong-tool",
            pass(),
            run(vec![
                agent(),
                model(2, 1),
                tool(3, 1, "web_search"),
                model(4, 1),
            ]),
        ),
        (
            "transparent-error",
            graph_run(|span| span),
            graph_run(failed),
        ),
    ]
}

fn failure_fixture(scenario: &str, side: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../breakspan-cli/tests/fixtures")
        .join(format!("{scenario}-{side}.bspan"))
}

fn string_attribute(key: &str, value: &str) -> KeyValue {
    KeyValue {
        key: key.into(),
        value: Some(AnyValue {
            value: Some(Value::StringValue(value.into())),
        }),
        ..Default::default()
    }
}

/// A classified step named by its convention attribute; the span name follows
/// the GenAI `{operation} {name}` pattern.
fn step(id: u8, parent: Option<u8>, operation: &str, key: &str, name: &str) -> Span {
    let mut span = span(id, parent, operation, &format!("{operation} {name}"));
    span.attributes.push(string_attribute(key, name));
    span
}
fn agent() -> Span {
    step(
        1,
        None,
        "invoke_agent",
        "gen_ai.agent.name",
        "synthetic-agent",
    )
}
fn model(id: u8, parent: u8) -> Span {
    step(
        id,
        Some(parent),
        "chat",
        "gen_ai.request.model",
        "synthetic-model",
    )
}
fn tool(id: u8, parent: u8, name: &str) -> Span {
    step(id, Some(parent), "execute_tool", "gen_ai.tool.name", name)
}
/// A graph/chain node without `gen_ai.operation.name`, classified as `Other`.
fn graph(id: u8, parent: u8, name: &str) -> Span {
    let mut span = span(id, Some(parent), "", name);
    span.attributes.clear();
    span
}
fn failed(mut span: Span) -> Span {
    span.status = Some(Status {
        code: 2,
        message: "synthetic failure".into(),
    });
    span
}
/// One export whose parents end after their descendants. Parents must precede
/// their children, which have larger IDs and therefore start later.
fn run(mut spans: Vec<Span>) -> ExportTraceServiceRequest {
    for child in (1..spans.len()).rev() {
        let parent = spans[..child]
            .iter()
            .position(|span| span.span_id == spans[child].parent_span_id)
            .unwrap();
        spans[parent].end_time_unix_nano = spans[parent]
            .end_time_unix_nano
            .max(spans[child].end_time_unix_nano + 1_000_000);
    }
    request(spans)
}

#[test]
fn committed_failure_fixtures_match_their_generator() {
    for (scenario, pass, fail) in failure_pairs() {
        for (side, export) in [("pass", pass), ("fail", fail)] {
            let path = failure_fixture(scenario, side);
            assert_eq!(
                fs::read(&path).unwrap(),
                recording(&[export]),
                "{} is stale; regenerate it",
                path.display()
            );
        }
    }
}

#[tokio::test]
#[ignore = "explicitly regenerates the checked-in failure fixture pairs through real OTLP/gRPC"]
async fn regenerate_failure_fixtures() {
    for (scenario, pass, fail) in failure_pairs() {
        for (side, export) in [("pass", pass), ("fail", fail)] {
            let path = failure_fixture(scenario, side);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            // The recorder uses create_new; replace only these synthetic fixtures.
            match fs::remove_file(&path) {
                Ok(()) => (),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                Err(error) => panic!("cannot replace synthetic fixture: {error}"),
            }
            let mut receiver =
                TestReceiver::start(&path, RecordingOptions::default(), MemoryStore::default())
                    .await;
            assert!(
                receiver
                    .export(export.clone())
                    .await
                    .partial_success
                    .is_none()
            );
            receiver.shutdown().await;
            assert_eq!(fs::read(&path).unwrap(), recording(&[export]));
        }
    }
}
