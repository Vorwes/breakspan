# Breakspan architecture

## Purpose and non-goals

Breakspan is a local debugger for agentic AI systems. Its eventual question is:
**Where did behavior begin to go wrong, and what earlier execution difference
likely contributed?** The first milestone establishes a reliable TRACE pipeline,
not that diagnostic capability.

Breakspan is not an agent harness, tool chooser, LLM client, generic cloud
observability service or dashboard-first product. Existing agents remain
responsible for their execution and instrumentation.

## Implemented pipeline and ownership

```text
existing agent / harness
        │ OpenTelemetry traces, OTLP/gRPC
        ▼
breakspan-otlp ── decoding, validation, GenAI interpretation
        │ canonical core spans (no transport types)
        ▼
breakspan-store ── bounded memory, trace-ID grouping, snapshot queries
        │
        ├── breakspan-cli ── startup, notifications, text output
        └── future breakspan-tui / other frontends
                     │
              breakspan-core ── domain model, deterministic tree rendering
```

| Crate | Owns | Must not own |
| --- | --- | --- |
| `breakspan-core` | Canonical IDs, spans, timing, metadata, traces and tree rendering | OTel/protobuf/tonic, runtimes, databases, frontend types |
| `breakspan-otlp` | TraceService server, wire-input validation and semantic-convention mapping | Agent loops, storage implementation, UI logic |
| `breakspan-store` | In-memory retention, capacity accounting, snapshot queries and change notification | Transport decoding, GenAI interpretation |
| `breakspan-cli` | Executable, flags, loopback binding, shutdown and output orchestration | Normalization or domain debugger logic |
| `breakspan-tui` | Reserved interactive frontend crate | Implementation in this milestone, debugger business logic |

Core is dependency-free. OTLP depends on core/store; store depends on core and
Tokio; CLI depends on those domain/infrastructure crates. The TUI placeholder has
no dependencies or behavior. There is no GUI crate.

Frontends consume domain models and queries rather than owning debugger logic,
so future headless CI, TUI and GUI paths can share the same behavior. No speculative
frontend/plugin/query trait hierarchy is introduced now.

## Canonical model

- Fixed-size, nonzero `TraceId` (16 bytes) and `SpanId` (8 bytes).
- `Span`: trace/parent IDs, original name, validated timing, status, operation
  classification, typed attributes, resource attributes/schema URL, and
  instrumentation-scope name/version/attributes/schema URL.
- `Timing`: start/end Unix nanoseconds, with end >= start. Zero timestamps are
  retained as received; no missing time is fabricated.
- `SpanStatus`: unset, OK, error with message, or unknown numeric status with
  message. Error text is retained in the domain, but trees only show an error marker.
- `AttributeValue`: empty, string, boolean, integer, double, bytes, array or map.
  Unknown attribute keys are retained. Duplicate keys use the last value. Missing
  values are empty. Profiling-only string-dictionary references have no trace
  meaning and are treated as empty; dictionary keys are not resolved.
- `OperationKind`: agent, model, tool, other. Original operation attributes remain
  available even if the convention/operation is unknown.
- `Trace`: spans keyed by ID. Initially one trace is the run grouping; no
  framework-specific run ID or complete-run lifecycle is inferred.

OTel types end inside `breakspan-otlp`, including its tests. The public receiver
API accepts a Tokio listener, the memory store, and a shutdown future; it exposes
no protobuf or tonic types. Neither OTLP nor any database schema should dictate
the debugger's domain API. This insulation matters because telemetry conventions
and transport APIs evolve independently of trajectory analysis.

This model is intentionally incomplete: span events, links, wire span-kind/flags,
trace state, resource entities and dropped-field counts are not normalized yet.
Content recorded in span attributes is retained as typed metadata; event-based
content is not ingested. There is no tool-result domain object or context model.

## Hierarchy and incomplete data

Exports are upserted by `(trace ID, span ID)`; the last accepted copy wins.
Relationships are reconstructed **at query/render time**, never fixed by arrival
order. A child arriving before its parent initially appears as a missing-parent
root; after the parent arrives it becomes that parent's child. IDs never connect
across traces. Multiple roots are allowed.

Tree traversal is iterative, with a visited set. Cyclic components (including
self-parenting) are displayed once with a cycle annotation rather than recursively
looping or hiding spans. Roots and children sort by start time and span ID, not
network arrival. Visual indentation is capped after 65 levels; all spans still
render. Labels escape control characters and are limited to 200 characters.
Parent relationships remain in the canonical data even when malformed as a graph.

OTLP exports completed spans, but does not announce a complete trace. Therefore
CLI output is explicitly a **snapshot**, not a final run. Change notifications
coalesce; after a change, the CLI prints all currently stored trace snapshots.
This intentionally simple output can repeat unrelated traces. No inactivity-based
completion heuristic or streaming UI protocol is invented.

## Storage, concurrency and input limits

The cloneable `MemoryStore` shares one Tokio mutex. Critical sections contain no
await; the mutex provides consistent batch updates and queries. Snapshot queries
return owned copies; this is deliberately simple, not a zero-copy query engine.
Normalization and rendering/stdout work run on blocking-pool tasks. Network
requests continue while the CLI renders. Receiver shutdown is graceful.

Defaults:
- 1 MiB maximum decoded gRPC message; no compression enabled.
- 1,024 raw spans examined for acceptance per export (later spans rejected).
- 16 attribute nesting levels; deeper spans rejected.
- 16 MiB conservative normalization accounting budget per export, including
  repeated resource/scope metadata, preventing resource-to-span amplification.
- Two concurrent requests per gRPC connection.
- 10,000 stored spans and 64 MiB conservative storage accounting; `--max-spans`
  adjusts the count. The library can set both storage limits.

Budgets are allocation estimates, **not strict RSS limits**. Protobuf decoding,
snapshots and output add memory overhead. There is no global connection quota or
protection against a hostile local process opening many connections.

Invalid lengths, all-zero IDs, malformed parents and reversed timing reject the
span. Missing attributes/resource/scope/content are valid. Unknown status codes
and operations are retained. Valid siblings survive a rejected span, with
`rejected_spans` in an OTLP partial-success response. Decode errors/oversize
messages fail at tonic's protocol boundary. Errors never echo telemetry content.
Capacity rejection does not evict existing data; replacements remain possible if
they fit. Restarting clears everything. SQLite is not needed for this slice.

The CLI rejects non-loopback bind addresses. The library expects callers to
supply a local listener. There is no TLS, authentication or redaction. Sensitive
content should be disabled at the emitter; users must keep private traces out of
Git. This is a local development backend, not a production security perimeter.

## Interoperability and dependency decisions

Research checked upstream documentation, released crate manifests and generated
server code on 2026-10-07. Sources:
- [Rust OpenTelemetry status](https://github.com/open-telemetry/opentelemetry-rust#project-status):
  traces API, SDK and OTLP exporter are **Beta**; do not assume stable Rust APIs.
- [`opentelemetry-proto 0.33.0`](https://docs.rs/opentelemetry-proto/0.33.0/opentelemetry_proto/)
  and [generated TraceServiceServer](https://docs.rs/opentelemetry-proto/0.33.0/opentelemetry_proto/tonic/collector/trace/v1/trace_service_server/struct.TraceServiceServer.html).
- [`tonic 0.14.6`](https://docs.rs/tonic/0.14.6/tonic/): compatible with the generated
  0.33 OTLP server, with generated codecs supplied transitively by tonic-prost.
- [GenAI conventions moved](https://opentelemetry.io/docs/specs/semconv/gen-ai/gen-ai-agent-spans/)
  to [semantic-conventions-genai](https://github.com/open-telemetry/semantic-conventions-genai).
  [Agent spans](https://github.com/open-telemetry/semantic-conventions-genai/blob/4f85037ef86e92c510d2ef881a58f1076f6fc0e4/docs/gen-ai/gen-ai-agent-spans.md)
  and [model spans](https://github.com/open-telemetry/semantic-conventions-genai/blob/4f85037ef86e92c510d2ef881a58f1076f6fc0e4/docs/gen-ai/gen-ai-spans.md)
  remain **Development**. The linked revision records the researched mapping;
  this is not a promise of support for every convention or framework.

Classification uses a string-valued `gen_ai.operation.name` only:

| Values | Domain kind |
| --- | --- |
| `invoke_agent` | Agent |
| `chat`, `text_completion`, `generate_content`, `embeddings` | Model |
| `execute_tool` | Tool |
| Missing, wrong type, or any other value | Other |

Names, prompts, provider/model identity and tool arguments/results are not needed
for classification. No framework heuristics or speculative aliases are used.
New conventions require a scoped normalization change with fixtures.

Dependencies are actively maintained upstream libraries with concrete roles:
- `opentelemetry-proto`: upstream generated messages and service; only `trace`
  and `gen-tonic` features enabled. It transitively brings the OTel API/SDK, but
  Breakspan does not instrument itself or directly depend on an exporter/SDK.
  No local protobuf generation, build script or `protoc` installation needed.
- `tonic`: gRPC transport/server matching upstream generated types.
- `tokio`: networking, process signals, mutex/watch notification and task runtime.
- `tokio-stream`: listener-to-stream adapter required by tonic's incoming API.
- `clap`: typed command/options parsing, validation and generated help.

No serialization crate, database, TUI library, general error library, LLM client
or semantic-convention constant crate is added. Small library errors implement
standard Error/Display without an extra dependency. Cargo.lock pins the application;
stable Rust is used (locally validated with 1.99.0). No lower MSRV is promised yet.

## Validation and future direction

Unit tests cover validated types, deterministic trees, deep/cyclic relationships,
store limits/replacements, cross-trace isolation and normalization budgets.
Loopback gRPC tests bind an ephemeral port and use the upstream generated client:
representative exports, late parents across requests, typed unknown attributes,
missing content/metadata, malformed spans, partial success, oversize requests,
capacity limits and receiver recovery. No timing sleeps or external services are
needed for the automated suite. The Python demo separately proves interoperability
with an actual external OTel SDK/exporter.

The eventual progression is TRACE (what happened), DIFF (what changed), DIAGNOSE
(where failure began), then REPLAY through compatible harnesses. First meaningful
divergence, repeated equivalent tool loops, missing verification, stale state,
context growth and downstream consequences are future analysis concerns. Nothing
here claims causal diagnosis. Replay would delegate operations to compatible
harnesses; Breakspan still would not own an agent loop.

No comparison, failure localization, semantic-loop detection, AI diagnosis,
context profiling, assertions, replay/forking, MCP proxy, multi-agent UI, GPUI,
cloud services, authentication, distributed deployment or plugin system is
implemented. Extend only against human-approved issues and acceptance criteria.
