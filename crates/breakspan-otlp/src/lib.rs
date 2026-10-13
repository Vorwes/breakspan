//! OTLP transport and normalization boundary. Public receiver APIs expose no
//! protobuf or tonic types; the canonical spans are stored in breakspan-store.

mod normalize;

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
}

#[tonic::async_trait]
impl TraceService for Receiver {
    async fn export(
        &self,
        request: Request<ExportTraceServiceRequest>,
    ) -> Result<Response<ExportTraceServiceResponse>, Status> {
        // Normalization may traverse lots of metadata; keep that CPU work off
        // network workers. Limit concurrent requests at the server below.
        let parsed =
            tokio::task::spawn_blocking(move || normalize::normalize(request.into_inner()))
                .await
                .map_err(|_| Status::internal("normalization task failed"))?;
        let rejected = parsed.rejected + self.store.ingest(parsed.spans).await as i64;
        let partial_success = (rejected > 0).then(|| ExportTracePartialSuccess {
            rejected_spans: rejected,
            // No untrusted content is echoed into logs or protocol errors.
            error_message: "spans rejected: invalid IDs/timing, attribute depth, export budget or store capacity".into(),
        });
        Ok(Response::new(ExportTraceServiceResponse {
            partial_success,
        }))
    }
}

/// Serve a pre-bound local listener until shutdown resolves. Binding belongs to
/// the caller so CLI startup and integration tests can discover the actual port.
pub async fn serve(
    listener: TcpListener,
    store: MemoryStore,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), ReceiverError> {
    let service =
        TraceServiceServer::new(Receiver { store }).max_decoding_message_size(MAX_MESSAGE_BYTES);
    Server::builder()
        .concurrency_limit_per_connection(2)
        .add_service(service)
        .serve_with_incoming_shutdown(TcpListenerStream::new(listener), shutdown)
        .await
        .map_err(|error| ReceiverError(error.to_string()))
}
