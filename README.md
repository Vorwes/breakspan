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

### Real LangChain + Gemini calculator agent

This optional **external example** uses LangChain's agent loop, Gemini
[`gemini-3.8-flash`](https://ai.google.dev/gemini-api/docs/models/gemini-3.8-flash),
and one calculator tool. Breakspan itself still makes no LLM calls.
There is no Pi extension, collector, dashboard or LangSmith account to configure.
Python 3.10+ and `uv` are sufficient; `uv` installs the inline dependencies.

**Terminal 1:**

```sh
cargo run -p breakspan-cli -- listen
```

**Terminal 2:**

```sh
uv run examples/langchain_gemini_agent.py
```

Get a key from [Google AI Studio](https://aistudio.google.com/apikey). The script
prompts for it without echoing or saving it. Alternatively, set `GEMINI_API_KEY`
in your local environment (PowerShell: `$env:GEMINI_API_KEY`; Bash:
`export GEMINI_API_KEY`). Do not share or commit the key. The example explicitly
uses the Gemini Developer API and this key, not Vertex AI or `GOOGLE_API_KEY`.

The default prompt asks the agent to multiply 12345 by 6789 using its calculator
(expected result: **83,810,205**). You can supply another arithmetic prompt:

```sh
uv run examples/langchain_gemini_agent.py "Use the calculator to divide 144 by 12."
```

A normal run produces an agent span with model → tool → model children:

```text
invoke_agent calculator
├── chat gemini-3.8-flash
├── execute_tool calculator
└── chat gemini-3.8-flash
```

Spans are buffered in the example and exported in one batch after it finishes,
including on failure. The final answer is printed only in the example terminal.
The callback exports operation/model/tool names, timing, status and error **types**,
not prompts, arguments, results, credentials, raw errors or exception events.
LangSmith tracing is explicitly disabled, and ambient OTel resource attributes
and exporter auth headers are not copied. Model/tool spans are direct agent
children; internal LangGraph nodes are omitted. This is a small smoke test, not
a general-purpose LangChain instrumentation adapter.

The calculator performs finite floating-point arithmetic without `eval`, file,
shell or network access. Execution has an eight-step graph limit, 30-second
per-request timeout, no automatic model retries, a 1,024-output-token limit,
and a two-second OTLP export timeout. Export failures return a nonzero exit code;
OTLP partial rejections can still occur, so check the receiver output. Use
`--endpoint http://127.0.0.1:PORT` if listening on another local port.

**Live requests require model access and consume your Gemini quota.** Free-tier
eligibility is not guaranteed; check [pricing](https://ai.google.dev/gemini-api/docs/pricing)
and [limits](https://ai.google.dev/gemini-api/docs/rate-limits). Use public/synthetic
prompts, especially on unpaid services; review Google's [terms](https://ai.google.dev/gemini-api/terms).
There is no silent fallback to another model if `gemini-3.8-flash` is unavailable.

On failure, the example prints a numeric HTTP status when the Google SDK makes
one available (including through LangChain exception wrappers), plus fixed
troubleshooting guidance. Raw response bodies, headers and error messages remain
redacted. In this integration, `GoogleAPIError` wraps server errors: HTTP 500/503
may mean a Gemini backend or request-specific failure; they do not by themselves
indicate an invalid key.
For HTTP 429 check quota; for 401/403 check authentication/access; for 404 check
model availability. Share only the sanitized diagnostic line when asking for
help. See Google's [troubleshooting guide](https://ai.google.dev/gemini-api/docs/troubleshooting).

Credential-free tests use real LangChain agent execution, fake model responses,
a stubbed Gemini SDK call, and a loopback OTLP/gRPC receiver:

```sh
uv run examples/langchain_gemini_agent.py --self-test
```

These tests never call Gemini and do not certify live account/model access.
CI runs them on Linux and Windows separately from the Rust workspace.

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
uv run examples/langchain_gemini_agent.py --self-test
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
