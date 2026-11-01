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
| `breakspan-otlp` | TraceService server, recording/loading format, wire-input validation and semantic-convention mapping | Agent loops, storage implementation, UI logic |
| `breakspan-store` | In-memory retention, capacity accounting, snapshot queries and change notification | Transport decoding, GenAI interpretation |
| `breakspan-cli` | Executable, flags, loopback binding, recording/loading orchestration, shutdown and text output | Protobuf types, normalization or domain debugger logic |
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
- Step name: an optional string naming the agent, model or tool a classified span
  represents. Core holds only the value, never the convention keys it came from.
  `None` means no name was found; the span name is not substituted.
- `Trace`: spans keyed by ID. Initially one trace is the run grouping; no
  framework-specific run ID or complete-run lifecycle is inferred.

OTel types end inside `breakspan-otlp`, including its tests. The public receiver
API accepts a Tokio listener, the memory store, an optional `Recorder`, and a
shutdown future; it exposes no protobuf or tonic types. Recording creation and
loading likewise expose only paths, options, the memory store and bounded reports.
Neither OTLP nor any database schema should dictate the debugger's domain API.
This insulation matters because telemetry conventions and transport APIs evolve
independently of trajectory analysis.

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

Inside `tree.rs`, private helpers separate hierarchy indexing and component-root
selection from iterative traversal and text-line formatting. `render_tree` only
orchestrates these steps; no public graph or renderer abstraction is introduced.

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
they fit. Restarting clears in-memory state; recordings must be explicitly loaded.
SQLite is not needed for this slice.

The CLI rejects non-loopback bind addresses. The library expects callers to
supply a local listener. There is no TLS, authentication or redaction. Sensitive
content should be disabled at the emitter; users must keep private traces out of
Git. This is a local development backend, not a production security perimeter.

## Trace recordings and offline loading

Recordings are an opt-in TRACE prerequisite, not a database, comparison engine
or replay feature. `breakspan-otlp` owns encoding, framing, file I/O and loading;
protobuf remains inside that crate. The CLI only wires `listen --record FILE`
and `show FILE`, obtains store snapshots and renders them through the same
blocking-pool output helper used by live snapshots. `show` prints once after
loading, not after each export.

The version-1 `.bspan` format is:

```text
8-byte header: b"BRKSPAN\x01" (seven magic bytes, then version 1)
repeated frames:
    u32 little-endian payload length
    that many bytes of prost-encoded OTLP ExportTraceServiceRequest
```

Each payload is at most 1 MiB. A zero-length payload is a valid empty protobuf
export. Recording and loading default to a 256 MiB total byte cap, including
the header and four-byte frame lengths. `--max-recording-bytes` can change it;
values below the eight-byte header size are rejected. On `listen`, the option
requires `--record`. These file limits are distinct from the default 10,000-span
and 64 MiB conservative in-memory limits. Loading uses the normal store limits,
not unbounded accumulation; snapshots, decoding and rendering still add overhead.

The receiver records decoded exports before normalization, so rejected spans,
events, links and other known protobuf fields absent from the canonical model
can remain in recordings. This is not a byte-exact network capture: prost does
not preserve unknown wire fields. Offline loading reuses the existing normalizer
and store; validation/capacity rejection counts are reported without exposing
telemetry. One blocking-pool worker per export encodes the frame, normalizes and
ingests. Encoding and normalization run concurrently across exports; the
recorder's synchronous mutex covers only the frame write and store ingestion,
which preserves file/ingest order and last-accepted-copy-wins semantics.
The ordering lock is acquired only on the blocking worker; no async mutex is
held across an await.
Once started, this blocking work completes ingestion even if its RPC is canceled.

The recorder creates files exclusively by default. `--force` requires `--record`
and explicitly replaces an existing file: the old directory entry is removed and
a new file is created exclusively (mode `0600` on Unix), so a permissive existing
file or a symlink at the destination is never reused or followed. If the header
write fails, the recorder removes the file it just created. Each frame, length
prefix included, is written with one unbuffered `std::fs::File::write_all` on a
blocking-pool task. There is no separate recording buffer or queue to flush at
shutdown; the runtime waits for started blocking work before it exits. Disk
latency adds to recording-enabled export latency. The first cap or I/O failure
is sticky: recording is disabled, the CLI warns once and stops monitoring it,
and live ingestion continues. Failure can leave an incomplete trailing frame.
Successful writes do not imply `fsync`, power-loss durability or crash consistency.

Loading checks the cap incrementally, retaining a valid prefix even when the
whole file exceeds the cap. An invalid/truncated header, unsupported version,
or frame failure before any complete decoded frame is an error with no tree
output. A header-only file is valid and empty. After at least one complete
decoded frame, an invalid/oversize/truncated frame, read failure, cap or a panic
while ingesting a frame returns the loaded prefix with a warning. The CLI prints its snapshots, warns on stderr
and exits nonzero, so partial output cannot masquerade as a complete recording.
Errors and warnings do not echo telemetry content.

Recording is raw and unredacted, potentially including prompts, arguments,
credentials and fields not shown in trees. Startup warns when it is enabled;
in-memory-only startup wording is used only when recording is disabled.
Never commit captured recordings. Root `/recordings/` is ignored for local
captures; the only committed capture is the intentionally synthetic
`crates/breakspan-otlp/tests/fixtures/synthetic.bspan`. Regenerate it with:

```sh
cargo test -p breakspan-otlp --test recording regenerate_synthetic_fixture -- --ignored --exact --nocapture
```

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

After classification, one string attribute supplies the span's step name:

| Domain kind | Step-name attribute |
| --- | --- |
| Agent | `gen_ai.agent.name` |
| Model | `gen_ai.request.model` |
| Tool | `gen_ai.tool.name` |
| Other | none |

A missing, non-string or empty attribute gives no name and never rejects the
span. An attribute belonging to another kind (for example `gen_ai.tool.name` on
a model span) is ignored. The copied name is charged to the normalization budget
and to store byte accounting; the source attribute is retained unchanged. Provider
name, token usage and content attributes are not interpreted.

In `normalize.rs`, ID/timing validation and final span construction stay together;
private helpers isolate operation classification, step-name extraction, status
conversion and optional scope conversion. Budgeted copies retain their charge
order, and the export-level loop restores the budget for rejected spans before
processing siblings.

Dependencies use upstream libraries with concrete roles:
- `opentelemetry-proto`: upstream generated messages and service; only `trace`
  and `gen-tonic` features enabled. It transitively brings the OTel API/SDK, but
  Breakspan does not instrument itself or directly depend on an exporter/SDK.
  No local protobuf generation, build script or `protoc` installation needed.
- `prost` 0.14: direct encoding/decoding of the existing generated OTLP messages
  for recording frames; no new protobuf schema or generation step. This matches
  upstream OTLP's existing prost 0.14 requirement and uses only the `std` feature.
  Its [upstream README](https://github.com/tokio-rs/prost#readme) describes the
  project as passively maintained, not actively maintained.
- `tonic`: gRPC transport/server matching upstream generated types.
- `tokio`: networking, process signals, mutex/watch notification and task runtime.
- `tokio-stream`: listener-to-stream adapter required by tonic's incoming API.
- `clap`: typed command/options parsing, validation and generated help.

No general-purpose serialization crate, database, TUI library, general error
library, LLM client or semantic-convention constant crate is added. Small library
errors implement standard Error/Display without an extra dependency. Cargo.lock
pins the application; stable Rust is used (locally validated with 1.99.0).
No lower MSRV is promised yet.

## Validation and future direction

Unit tests cover validated types, deterministic trees, deep/cyclic relationships,
store limits/replacements, cross-trace isolation and normalization budgets.
Loopback gRPC tests bind an ephemeral port and use the upstream generated client:
representative exports, late parents across requests, typed unknown attributes,
missing content/metadata, malformed spans, partial success, oversize requests,
capacity limits and receiver recovery. No timing sleeps or external services are
needed for the automated suite. The synthetic Python demo separately proves
interoperability with an actual external OTel SDK/exporter.

`examples/langchain_gemini_agent.py` is an optional external smoke harness using
LangChain `create_agent`, Gemini `gemini-3.5-flash-lite`, and a calculator tool. Its
example-local callback emits an agent root with model/tool children using the
operation mapping above; it intentionally omits internal graph/chain nodes.
Only operation metadata and error types are exported, with content/exception
recording and LangSmith tracing disabled. Explicit parent context and callback
run IDs preserve parentage when tools execute on worker threads. Ended spans
are collected locally and exported in one bounded-time OTLP/gRPC batch after
success or failure. A failed export is distinct from failed agent execution;
OTLP partial rejection still requires checking the receiver. Graph steps,
model request time/retries/output tokens are bounded, but this is not a hard
wall-clock or memory sandbox.

Dependencies are inline and confined to the example: `langchain` supplies the
agent loop, `langchain-google-genai` supplies the Gemini Developer API model,
and the upstream Python OTel SDK/gRPC exporter supplies telemetry. No Rust
crate gains a harness or LLM dependency. Direct dependency versions are pinned
and tested; transitive dependencies are resolved by uv, not fully locked.
Relevant upstream APIs are documented in the [LangChain agent guide](https://docs.langchain.com/oss/python/langchain/agents)
and [Google integration guide](https://docs.langchain.com/oss/python/integrations/chat/google_generative_ai).
Automated example tests exercise real framework callbacks and Gemini request
construction using fake/stubbed responses, calculator validation, error and
incomplete-span cleanup, limits, metadata privacy and actual loopback OTLP
transport. They need no credentials and make no Gemini calls; live model access,
quota and answer quality must be checked separately by the user. The example
prints only a simple failure message; raw provider details and HTTP failure
reasons are neither printed nor exported.

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
