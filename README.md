# Breakspan

**Find where your agent went off track.**

Breakspan is a local debugger for agentic AI systems that analyzes execution
trajectories to locate where failed behavior begins.

It ingests telemetry from existing agents and harnesses through OpenTelemetry;
it does **not** run agents, select tools, or call an LLM. The direction is
**TRACE → DIFF → DIAGNOSE → REPLAY**, with behavioral debugging—not generic
observability dashboards—as the product identity.

## Current milestone: TRACE

Implemented today:
- A local OTLP/gRPC trace receiver.
- Framework-independent spans with typed attributes, status, timing and metadata.
- Representative GenAI agent/model/tool classification.
- Bounded in-memory traces and arrival-order-independent execution trees.
- A headless CLI that prints provisional snapshots; a TUI crate is reserved.

There is **no comparison, failure diagnosis or replay yet**.

```text
existing agent / harness
         │ OTLP/gRPC
         ▼
  127.0.0.1:4317 → normalization → canonical spans → memory → execution tree
```

## Try it

Install stable Rust, then:

```sh
cargo run -p breakspan-cli -- listen
# Or install the executable locally:
cargo install --path crates/breakspan-cli
breakspan listen
```

The receiver accepts plaintext OTLP/gRPC traces on `127.0.0.1:4317`. Configure
an already-instrumented agent's **gRPC** OTLP exporter to use that endpoint. For
SDKs supporting the standard environment variables:

```sh
export OTEL_EXPORTER_OTLP_PROTOCOL=grpc
export OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4317
```

These variables configure an exporter; they do not instrument an agent by
themselves. Framework-specific instrumentation remains outside Breakspan.
There is no OTLP/HTTP endpoint on port 4318.

### Reproducible external instrumentation demo

With the receiver running, use Python 3.10+ and [uv](https://docs.astral.sh/uv/):

```sh
uv run examples/instrumented_agent.py
```

Alternatively, install `opentelemetry-sdk` and
`opentelemetry-exporter-otlp-proto-grpc` in a virtual environment and run the
script with Python. This is a synthetic instrumentation example, not an agent
runtime: it emits fixed spans, without making LLM calls or reading private files.

Example output (IDs and durations vary):

```text
Trace 4d8c418253d82e10796a85e4b4138916 (snapshot, 4 spans)
invoke_agent  50ms
├── chat  24ms
├── execute_tool read_file  5ms
└── chat  20ms
```

Snapshots may repeat and be incomplete. OTLP has no trace-completion marker.
Late parents repair subsequent snapshots; missing parents and cyclic components
remain visible rather than disappearing. Siblings sort by start time, then ID.

### Options and safety

```sh
breakspan listen --bind 127.0.0.1:4317 --max-spans 10000
breakspan listen --help
```

Only loopback binds are allowed by the CLI. Ctrl-C requests graceful shutdown;
all traces disappear when the process exits. Limits default to 10,000 spans and
64 MiB of conservative storage accounting (not a hard RSS limit). At capacity,
new spans are rejected; there is no automatic eviction. Duplicate span IDs use
the last accepted export. Restart to clear the store.

The receiver limits decoded messages to 1 MiB, exports to 1,024 spans, attribute
nesting to 16 levels and normalized exports to a 16 MiB accounting budget.
Invalid spans yield OTLP partial success without losing valid siblings;
oversize/invalid protobuf messages fail at the gRPC boundary. Span names are
escaped for terminal controls and truncated to 200 characters in output.

**Telemetry can contain sensitive prompts, tool arguments and credentials.**
Breakspan keeps received attributes in memory without automatic redaction. Use
your instrumentation's content-recording controls, do not commit private traces,
and do not expose this unauthenticated receiver beyond the local machine.

## Development

```sh
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo build --workspace
# Run only the real loopback gRPC integration suite:
cargo test -p breakspan-otlp --test grpc_ingestion
```

See [architecture](docs/architecture.md), [contributing](CONTRIBUTING.md),
[agent instructions](AGENTS.md) and the [Rust skill](.agents/skills/rust/SKILL.md).
CI validates Linux and Windows. `Cargo.lock` pins the application dependencies.

## Status and limitations

Rust OpenTelemetry trace APIs are Beta; GenAI semantic conventions are
Development and can change. The implemented operation mapping and authoritative
sources are documented in [architecture](docs/architecture.md#interoperability-and-dependency-decisions).

No persistence, full TUI/GUI, span-event/link analysis, run comparison,
first-divergence detection, semantic-loop detection, behavioral assertions,
context profiling or replay is implemented. OpenAI Agents SDK, LangGraph and
other harness integrations depend on those systems emitting compatible spans;
they are not bundled adapters or individually certified integrations.

MIT licensed. Agent-generated implementation requires human review; implementing
agents never merge their own PRs.
