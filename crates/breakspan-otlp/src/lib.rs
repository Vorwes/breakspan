//! OTLP transport and normalization boundary. Public receiver APIs expose no
//! protobuf or tonic types; the canonical spans are stored in breakspan-store.

mod normalize;
mod recording;

pub use recording::{
    DEFAULT_MAX_RECORDING_BYTES, LoadError, LoadReport, Recorder, RecordingError, RecordingOptions,
    load,
};

use breakspan_store::MemoryStore;
use opentelemetry_proto::tonic::collector::trace::v1::{
    ExportTracePartialSuccess, ExportTraceServiceRequest, ExportTraceServiceResponse,
    trace_service_server::{TraceService, TraceServiceServer},
};
use std::{fmt, future::Future};
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{Request, Response, Status, transport::Server};

const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

/// Error text is intentionally independent of tonic's error type.
#[derive(Debug)]
pub struct ReceiverError(String);
impl fmt::Display for ReceiverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ReceiverError {}

struct Receiver {
    store: MemoryStore,
    recorder: Option<Recorder>,
}

#[tonic::async_trait]
impl TraceService for Receiver {
    async fn export(
        &self,
        request: Request<ExportTraceServiceRequest>,
    ) -> Result<Response<ExportTraceServiceResponse>, Status> {
        // Recording, normalization and ingestion complete together on a blocking
        // worker, even if the RPC is cancelled. A recorder serializes this whole
        // operation so replay preserves live last-export-wins/capacity decisions.
        let store = self.store.clone();
        let recorder = self.recorder.clone();
        let rejected = tokio::task::spawn_blocking(move || {
            let request = request.into_inner();
            match recorder {
                Some(recorder) => recorder.ingest(request, &store),
                None => ingest_export(request, &store),
            }
        })
        .await
        .map_err(|_| Status::internal("ingestion task failed"))?;
        let partial_success = (rejected > 0).then(|| ExportTracePartialSuccess {
            rejected_spans: rejected as i64,
            // No untrusted content is echoed into logs or protocol errors.
            error_message: "spans rejected: invalid IDs/timing, attribute depth, export budget or store capacity".into(),
        });
        Ok(Response::new(ExportTraceServiceResponse {
            partial_success,
        }))
    }
}

// Called only on blocking workers by both gRPC and recording loading. Store
// mutex critical sections contain no I/O, and block_on never blocks a network
// worker. No transport-specific validation/normalization fork exists.
fn ingest_export(request: ExportTraceServiceRequest, store: &MemoryStore) -> u64 {
    let parsed = normalize::normalize(request);
    parsed.rejected as u64
        + tokio::runtime::Handle::current().block_on(store.ingest(parsed.spans)) as u64
}

/// Serve a pre-bound local listener until shutdown resolves. Binding belongs to
/// the caller so CLI startup and integration tests can discover the actual port.
pub async fn serve(
    listener: TcpListener,
    store: MemoryStore,
    recorder: Option<Recorder>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), ReceiverError> {
    let service = TraceServiceServer::new(Receiver { store, recorder })
        .max_decoding_message_size(MAX_MESSAGE_BYTES);
    Server::builder()
        .concurrency_limit_per_connection(2)
        .add_service(service)
        .serve_with_incoming_shutdown(TcpListenerStream::new(listener), shutdown)
        .await
        .map_err(|error| ReceiverError(error.to_string()))
}
