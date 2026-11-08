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
- Opt-in bounded raw OTLP recordings and offline execution-tree loading.
- Structural comparison of two recordings, reporting the first divergent step.

There is **no failure diagnosis or replay yet**, and comparison makes no causal
or semantic claim.

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

### Record and inspect a trace

Create a local `recordings` directory, then:

```sh
breakspan listen --record recordings/run.bspan
# Stop the receiver with Ctrl-C, then inspect the recording:
breakspan show recordings/run.bspan
# Or inspect the repository's synthetic fixture without running a receiver:
cargo run -p breakspan-cli -- show crates/breakspan-otlp/tests/fixtures/synthetic.bspan
```

Recording refuses to overwrite an existing file. Use `--force` with `--record`
to explicitly replace it with a new private file (`0600` on Unix; an existing
symlink is replaced, not followed). `--max-recording-bytes BYTES` sets a total file cap
for recording or loading (default 256 MiB, minimum 8 bytes). On `listen`, this
option and `--force` require `--record`. The recording cap is independent of
the in-memory span/byte limits; recordings include exports before normalization,
including spans later rejected by validation or memory capacity.

`show` loads through the same normalizer into bounded memory and prints the
stored execution-tree snapshots once. It accepts `--max-spans` (default 10,000);
the 64 MiB storage-accounting limit still applies. Rejected spans are reported
on stderr. A damaged header fails without printing trees. If corruption,
truncation or the loading byte cap occurs after complete decoded frames, `show`
prints the valid prefix, warns on stderr and exits nonzero. An empty recording
with a valid header is accepted.

If recording reaches its cap or a write fails, the receiver warns once and
disables recording; live ingestion continues. There is no separate recording
buffer or queue to flush. Successful writes do not guarantee `fsync` durability.

**Recordings contain raw, potentially sensitive telemetry without redaction,**
including fields not shown in trees. They are not byte-for-byte network captures:
unknown protobuf wire fields are not preserved. Never commit captured recordings,
even if they appear harmless. Root `/recordings/` is ignored for local use;
only intentionally synthetic fixtures belong in Git.

### Compare two recordings

```sh
breakspan diff recordings/good.bspan recordings/bad.bspan
```

`diff` loads each recording like `show` (the same `--max-recording-bytes` and
`--max-spans` options apply to each) and walks both runs' **steps** in the same
order until they first differ:

```text
First divergence at step 3
  path: agent calculator
  a: tool calculator  depth 1  [error]
  b: tool calculator  depth 1  [ok]
2 matching steps before divergence.
```

A step is a span classified as agent, model or tool. Other spans (graph nodes,
chains) are skipped; their step descendants count as children of the nearest
step ancestor. A step's identity is its kind plus its step name (agent name,
requested model or tool name), or the span name when the telemetry has none.
Siblings are ordered by start time, then identity, then error status, then span
ID. Steps differ
when their depth in the step tree, identity or error status differs (unset, OK
and unknown status codes all count as not an error), or when one run has steps
left. Span IDs, timing, error messages, prompts, arguments, results and other
attributes are ignored. `path` lists the step ancestors both runs share.

Exit codes follow `diff(1)`: `0` no divergence, `1` the runs diverge, `2` usage,
load or input error (unlike `show`, which exits `1` on errors). Each recording
must contain exactly one trace. A recording that loads only partially, or in
which any span is rejected (invalid, or over `--max-spans` or the 64 MiB storage
limit), is an error, so an incomplete run is never reported as a divergence.
If a large recording hits the span limit, raise `--max-spans`.

Only the first divergence is reported; there is no realignment afterwards.
Missing parents, multiple roots and parent cycles are compared deterministically,
but such malformed hierarchies can make the reported position less meaningful.
Sibling steps that start at the same time and match in identity and status, but
differ in their descendants, are still ordered by span ID.

Synthetic pass/fail pairs for common agent failures, with the output `diff`
reports for each and where that output falls short, are in
[failure pairs](docs/failure-pairs.md).

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
[`gemini-3.5-flash-lite`](https://ai.google.dev/gemini-api/docs/models/gemini-3.5-flash-lite),
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
├── chat gemini-3.5-flash-lite
├── execute_tool calculator
└── chat gemini-3.5-flash-lite
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
There is no silent fallback to another model if `gemini-3.5-flash-lite` is unavailable.

On failure, the example prints a simple failure message and still exports the
trace. Provider error messages, HTTP failure reasons and response details are
not printed or exported.

Credential-free tests use real LangChain agent execution, fake model responses,
a stubbed Gemini SDK call, and a loopback OTLP/gRPC receiver:

```sh
uv run examples/langchain_gemini_agent.py --self-test
```

`examples/test_langchain_gemini_agent.py` contains the example's offline unit
tests, not a second runnable Gemini example. These tests never call Gemini and
do not certify live account/model access. CI runs them on Linux and Windows
separately from the Rust workspace.

### Options and safety

```sh
breakspan listen --bind 127.0.0.1:4317 --max-spans 10000
breakspan listen --help
```

Only loopback binds are allowed by the CLI. Ctrl-C requests graceful shutdown;
in-memory traces disappear when the process exits unless explicitly reloaded
from an opt-in recording. Limits default to 10,000 spans and 64 MiB of conservative
storage accounting (not a hard RSS limit). At capacity, new spans are rejected;
there is no automatic eviction. Duplicate span IDs use the last accepted export.
Restart to clear the in-memory store.

The receiver limits decoded messages to 1 MiB, exports to 1,024 spans, attribute
nesting to 16 levels and normalized exports to a 16 MiB accounting budget.
Invalid spans yield OTLP partial success without losing valid siblings;
oversize/invalid protobuf messages fail at the gRPC boundary. Span names are
escaped for terminal controls and truncated to 200 characters in output.

**Telemetry can contain sensitive prompts, tool arguments and credentials.**
Breakspan keeps received attributes in memory, and in opt-in recordings, without
automatic redaction. Use your instrumentation's content-recording controls,
never commit captures, and do not expose this unauthenticated receiver beyond
the local machine.

## Development

```sh
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo build --workspace
uv run examples/langchain_gemini_agent.py --self-test
# Run only the real loopback gRPC integration suite:
cargo test -p breakspan-otlp --test grpc_ingestion
# Regenerate only the intentionally synthetic recording fixture:
cargo test -p breakspan-otlp --test recording regenerate_synthetic_fixture -- --ignored --exact --nocapture
# Regenerate only the synthetic failure pairs used by the diff tests:
cargo test -p breakspan-otlp --test recording regenerate_failure_fixtures -- --ignored --exact
```

See [architecture](docs/architecture.md), [contributing](CONTRIBUTING.md),
[agent instructions](AGENTS.md) and the [Rust skill](.agents/skills/rust/SKILL.md).
CI validates Linux and Windows. `Cargo.lock` pins the application dependencies.

## Status and limitations

Rust OpenTelemetry trace APIs are Beta; GenAI semantic conventions are
Development and can change. The implemented operation mapping and authoritative
sources are documented in [architecture](docs/architecture.md#interoperability-and-dependency-decisions).

No database-backed storage, full TUI/GUI, span-event/link analysis, semantic or
realigning run comparison, semantic-loop detection, behavioral assertions,
context profiling or replay is implemented. OpenAI Agents SDK, LangGraph and
other harness integrations depend on those systems emitting compatible spans;
they are not bundled adapters or individually certified integrations.

MIT licensed. Agent-generated implementation requires human review; implementing
agents never merge their own PRs.
