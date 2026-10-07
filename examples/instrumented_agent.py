# /// script
# requires-python = ">=3.10"
# dependencies = [
#   "opentelemetry-sdk>=1.39,<2",
#   "opentelemetry-exporter-otlp-proto-grpc>=1.39,<2",
# ]
# ///
"""Synthetic instrumentation demo, not an agent harness or a real model call.

Start `cargo run -p breakspan-cli -- listen`, then run:
    uv run examples/instrumented_agent.py

Only fixed synthetic metadata is exported; no prompts or private files are read.
"""

import os
import time

from opentelemetry import trace
from opentelemetry.exporter.otlp.proto.grpc.trace_exporter import OTLPSpanExporter
from opentelemetry.sdk.resources import Resource
from opentelemetry.sdk.trace import TracerProvider
from opentelemetry.sdk.trace.export import BatchSpanProcessor

provider = TracerProvider(resource=Resource.create({"service.name": "breakspan-demo"}))
exporter = OTLPSpanExporter(
    endpoint=os.environ.get("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "http://127.0.0.1:4317"),
    insecure=True,
)
provider.add_span_processor(BatchSpanProcessor(exporter))
trace.set_tracer_provider(provider)
tracer = trace.get_tracer("breakspan.instrumentation-demo", "0.1.0")

with tracer.start_as_current_span(
    "invoke_agent", attributes={"gen_ai.operation.name": "invoke_agent"}
):
    with tracer.start_as_current_span(
        "chat", attributes={"gen_ai.operation.name": "chat"}
    ):
        time.sleep(0.02)
    with tracer.start_as_current_span(
        "execute_tool read_file",
        attributes={
            "gen_ai.operation.name": "execute_tool",
            "gen_ai.tool.name": "read_file",
        },
    ):
        time.sleep(0.005)
    with tracer.start_as_current_span(
        "chat", attributes={"gen_ai.operation.name": "chat"}
    ):
        time.sleep(0.02)

try:
    if not provider.force_flush(timeout_millis=5000):
        raise RuntimeError("telemetry flush timed out")
finally:
    provider.shutdown()
