//! Real OTLP/gRPC wire tests: no external collector, Python, fixed port or sleeps.
use breakspan_core::{AttributeValue, OperationKind, SpanId, SpanStatus, TraceId, render_tree};
use breakspan_store::{MemoryStore, StoreLimits};
use opentelemetry_proto::tonic::{
    collector::trace::v1::{
        ExportTraceServiceRequest, ExportTraceServiceResponse,
        trace_service_client::TraceServiceClient,
    },
    common::v1::{
        AnyValue, ArrayValue, InstrumentationScope, KeyValue, KeyValueList, any_value::Value,
    },
    resource::v1::Resource,
    trace::v1::{ResourceSpans, ScopeSpans, Span, Status},
};
use std::time::Duration;
use tokio::{net::TcpListener, sync::oneshot, task::JoinHandle, time::timeout};
use tonic::transport::Channel;

struct TestReceiver {
    client: TraceServiceClient<Channel>,
    store: MemoryStore,
    stop: oneshot::Sender<()>,
    task: JoinHandle<Result<(), breakspan_otlp::ReceiverError>>,
}
impl TestReceiver {
    async fn start(store: MemoryStore) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(breakspan_otlp::serve(listener, store.clone(), async {
            let _ = stopped.await;
        }));
        let client = timeout(
            Duration::from_secs(5),
            TraceServiceClient::connect(format!("http://{address}")),
        )
        .await
        .unwrap()
        .unwrap();
        Self {
            client,
            store,
            stop,
            task,
        }
    }
    async fn export(&mut self, spans: Vec<Span>) -> ExportTraceServiceResponse {
        timeout(Duration::from_secs(5), self.client.export(request(spans)))
            .await
            .unwrap()
            .unwrap()
            .into_inner()
    }
    async fn shutdown(self) {
        drop(self.client);
        self.stop.send(()).unwrap();
        timeout(Duration::from_secs(5), self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}
fn kv(key: &str, value: Value) -> KeyValue {
    KeyValue {
        key: key.into(),
        value: Some(AnyValue { value: Some(value) }),
        ..Default::default()
    }
}
fn string(key: &str, value: &str) -> KeyValue {
    kv(key, Value::StringValue(value.into()))
}
fn span(id: u8, parent: Option<u8>, operation: Option<&str>, name: &str) -> Span {
    Span {
        trace_id: vec![1; 16],
        span_id: vec![id; 8],
        parent_span_id: parent.map(|p| vec![p; 8]).unwrap_or_default(),
        name: name.into(),
        start_time_unix_nano: u64::from(id) * 10_000_000,
        end_time_unix_nano: u64::from(id) * 10_000_000 + 6_000_000,
        attributes: operation
            .map(|op| vec![string("gen_ai.operation.name", op)])
            .unwrap_or_default(),
        ..Default::default()
    }
}
fn request(spans: Vec<Span>) -> ExportTraceServiceRequest {
    ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            resource: Some(Resource {
                attributes: vec![string("service.name", "test-agent")],
                ..Default::default()
            }),
            schema_url: "resource-schema".into(),
            scope_spans: vec![ScopeSpans {
                scope: Some(InstrumentationScope {
                    name: "test-instrumentation".into(),
                    version: "1.0".into(),
                    attributes: vec![kv("scope.flag", Value::BoolValue(true))],
                    ..Default::default()
                }),
                schema_url: "scope-schema".into(),
                spans,
            }],
        }],
    }
}
fn trace_id() -> TraceId {
    TraceId::new([1; 16]).unwrap()
}
fn span_id(id: u8) -> SpanId {
    SpanId::new([id; 8]).unwrap()
}

#[tokio::test]
async fn genai_export_normalizes_and_late_parent_repairs_tree() {
    let mut receiver = TestReceiver::start(MemoryStore::default()).await;
    let model = span(2, Some(1), Some("chat"), "chat");
    let mut tool = span(3, Some(1), Some("execute_tool"), "execute_tool read_file");
    tool.attributes.extend([
        string("gen_ai.tool.name", "read_file"),
        kv("vendor.tokens", Value::IntValue(42)),
        kv("vendor.bytes", Value::BytesValue(vec![0, 255])),
        kv("vendor.ratio", Value::DoubleValue(0.5)),
        kv(
            "vendor.list",
            Value::ArrayValue(ArrayValue {
                values: vec![AnyValue {
                    value: Some(Value::BoolValue(true)),
                }],
            }),
        ),
        kv(
            "vendor.map",
            Value::KvlistValue(KeyValueList {
                values: vec![string("nested", "kept")],
            }),
        ),
        KeyValue {
            key: "vendor.empty".into(),
            ..Default::default()
        },
    ]);
    tool.status = Some(Status {
        code: 2,
        message: "file not found".into(),
    });
    assert!(
        receiver
            .export(vec![tool, model])
            .await
            .partial_success
            .is_none()
    );
    let orphan = receiver.store.trace(trace_id()).await.unwrap();
    assert!(render_tree(&orphan).contains("missing parent"));
    let mut root = span(1, None, Some("invoke_agent"), "invoke_agent");
    root.end_time_unix_nano = 4_220_000_000;
    root.status = Some(Status {
        code: 1,
        message: String::new(),
    });
    assert!(receiver.export(vec![root]).await.partial_success.is_none());
    let trace = receiver.store.trace(trace_id()).await.unwrap();
    assert_eq!(trace.spans().len(), 3);
    let root = &trace.spans()[&span_id(1)];
    assert_eq!(root.operation, OperationKind::Agent);
    assert_eq!(root.parent_id, None);
    assert_eq!(root.status, SpanStatus::Ok);
    assert_eq!(root.timing.start_unix_nanos(), 10_000_000);
    assert_eq!(root.timing.end_unix_nanos(), 4_220_000_000);
    assert_eq!(trace.spans()[&span_id(2)].operation, OperationKind::Model);
    assert_eq!(trace.spans()[&span_id(2)].status, SpanStatus::Unset);
    let tool = &trace.spans()[&span_id(3)];
    assert_eq!(tool.operation, OperationKind::Tool);
    assert_eq!(tool.parent_id, Some(root.id));
    assert_eq!(tool.status, SpanStatus::Error("file not found".into()));
    assert_eq!(tool.attributes["vendor.tokens"], AttributeValue::Int(42));
    assert_eq!(
        tool.attributes["vendor.bytes"],
        AttributeValue::Bytes(vec![0, 255])
    );
    assert_eq!(tool.attributes["vendor.ratio"], AttributeValue::Double(0.5));
    assert_eq!(
        tool.attributes["vendor.list"],
        AttributeValue::Array(vec![AttributeValue::Bool(true)])
    );
    assert_eq!(
        tool.attributes["vendor.map"],
        AttributeValue::Map([("nested".into(), AttributeValue::String("kept".into()))].into())
    );
    assert_eq!(tool.attributes["vendor.empty"], AttributeValue::Empty);
    assert_eq!(
        tool.resource_attributes["service.name"],
        AttributeValue::String("test-agent".into())
    );
    assert_eq!(tool.resource_schema_url, "resource-schema");
    assert_eq!(tool.scope.name, "test-instrumentation");
    assert_eq!(tool.scope.version, "1.0");
    assert_eq!(tool.scope.schema_url, "scope-schema");
    assert_eq!(
        tool.scope.attributes["scope.flag"],
        AttributeValue::Bool(true)
    );
    // No prompts, model identity, tool arguments/results or agent identity needed.
    let tree = render_tree(&trace);
    assert!(
        tree.contains(
            "invoke_agent  4.21s\n├── chat  6ms\n└── execute_tool read_file  6ms [error]"
        )
    );
    assert!(!tree.contains("missing parent"));
    receiver.shutdown().await;
}

#[tokio::test]
async fn unknown_operations_statuses_and_missing_metadata_are_nonfatal() {
    let mut receiver = TestReceiver::start(MemoryStore::default()).await;
    let mut unknown = span(1, None, Some("future_operation"), "chat is just a name");
    unknown.status = Some(Status {
        code: 99,
        message: "future status".into(),
    });
    let mut wrong_type = span(2, None, None, "invoke_agent");
    wrong_type
        .attributes
        .push(kv("gen_ai.operation.name", Value::IntValue(7)));
    let raw = ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            scope_spans: vec![ScopeSpans {
                spans: vec![unknown, wrong_type, span(3, None, None, "uninstrumented")],
                ..Default::default()
            }],
            ..Default::default()
        }],
    };
    assert!(
        receiver
            .client
            .export(raw)
            .await
            .unwrap()
            .into_inner()
            .partial_success
            .is_none()
    );
    let trace = receiver.store.trace(trace_id()).await.unwrap();
    assert!(
        trace
            .spans()
            .values()
            .all(|s| s.operation == OperationKind::Other)
    );
    assert_eq!(
        trace.spans()[&span_id(1)].status,
        SpanStatus::Unknown {
            code: 99,
            message: "future status".into()
        }
    );
    assert!(trace.spans()[&span_id(3)].attributes.is_empty());
    assert!(trace.spans()[&span_id(3)].resource_attributes.is_empty());
    assert_eq!(trace.spans()[&span_id(3)].scope.name, "");
    assert!(
        receiver
            .client
            .export(ExportTraceServiceRequest::default())
            .await
            .unwrap()
            .into_inner()
            .partial_success
            .is_none()
    );
    receiver.shutdown().await;
}

#[tokio::test]
async fn malformed_spans_are_partial_success_and_server_remains_available() {
    let mut receiver = TestReceiver::start(MemoryStore::default()).await;
    let mut invalid = Vec::new();
    let mut short_trace = span(1, None, None, "bad");
    short_trace.trace_id = vec![1; 15];
    invalid.push(short_trace);
    let mut zero_trace = span(2, None, None, "bad");
    zero_trace.trace_id = vec![0; 16];
    invalid.push(zero_trace);
    let mut zero_span = span(3, None, None, "bad");
    zero_span.span_id = vec![0; 8];
    invalid.push(zero_span);
    let mut short_span = span(4, None, None, "bad");
    short_span.span_id = vec![1; 7];
    invalid.push(short_span);
    let mut parent = span(5, None, None, "bad");
    parent.parent_span_id = vec![1; 9];
    invalid.push(parent);
    let mut zero_parent = span(6, None, None, "bad");
    zero_parent.parent_span_id = vec![0; 8];
    invalid.push(zero_parent);
    let mut reversed = span(7, None, None, "bad");
    reversed.end_time_unix_nano = 1;
    invalid.push(reversed);
    let mut nested_value = AnyValue {
        value: Some(Value::StringValue("leaf".into())),
    };
    for _ in 0..20 {
        nested_value = AnyValue {
            value: Some(Value::ArrayValue(ArrayValue {
                values: vec![nested_value],
            })),
        };
    }
    let mut deep = span(8, None, None, "bad");
    deep.attributes.push(KeyValue {
        key: "nested".into(),
        value: Some(nested_value),
        ..Default::default()
    });
    invalid.push(deep);
    invalid.push(span(9, None, Some("invoke_agent"), "valid sibling"));
    let partial = receiver.export(invalid).await.partial_success.unwrap();
    assert_eq!(partial.rejected_spans, 8);
    assert!(!partial.error_message.is_empty());
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
    assert!(
        receiver
            .export(vec![span(10, Some(9), Some("chat"), "healthy")])
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
        2
    );
    receiver.shutdown().await;
}

#[tokio::test]
async fn oversize_request_is_rejected_without_poisoning_receiver() {
    let mut receiver = TestReceiver::start(MemoryStore::default()).await;
    let mut huge = span(1, None, None, "huge");
    huge.attributes
        .push(string("content", &"x".repeat(1024 * 1024)));
    assert!(
        timeout(
            Duration::from_secs(5),
            receiver.client.export(request(vec![huge]))
        )
        .await
        .unwrap()
        .is_err()
    );
    assert!(
        receiver
            .export(vec![span(2, None, None, "healthy")])
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
}

#[tokio::test]
async fn capacity_rejections_and_duplicate_exports_have_otlp_semantics() {
    let mut receiver = TestReceiver::start(MemoryStore::new(StoreLimits {
        max_spans: 1,
        max_bytes: 4096,
    }))
    .await;
    let partial = receiver
        .export(vec![
            span(1, None, None, "accepted"),
            span(2, None, None, "full"),
        ])
        .await
        .partial_success
        .unwrap();
    assert_eq!(partial.rejected_spans, 1);
    assert!(
        receiver
            .export(vec![span(1, None, None, "retry")])
            .await
            .partial_success
            .is_none()
    );
    let trace = receiver.store.trace(trace_id()).await.unwrap();
    assert_eq!(trace.spans().len(), 1);
    assert_eq!(trace.spans()[&span_id(1)].name, "retry");
    receiver.shutdown().await;
}

#[tokio::test]
async fn representative_model_operations_and_parent_cycles_are_supported() {
    let mut receiver = TestReceiver::start(MemoryStore::default()).await;
    let spans = ["chat", "text_completion", "generate_content", "embeddings"]
        .iter()
        .enumerate()
        .map(|(i, op)| {
            let id = i as u8 + 1;
            span(id, Some(if id == 4 { 1 } else { id + 1 }), Some(op), op)
        })
        .collect();
    assert!(receiver.export(spans).await.partial_success.is_none());
    let trace = receiver.store.trace(trace_id()).await.unwrap();
    assert!(
        trace
            .spans()
            .values()
            .all(|s| s.operation == OperationKind::Model)
    );
    assert!(render_tree(&trace).contains("cyclic parent component"));
    receiver.shutdown().await;
}
