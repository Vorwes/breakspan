# Failure pairs: what `diff` reports

Five committed pairs of synthetic recordings show what `breakspan diff` reports
for common agent failure shapes. They are evaluation material for the first DIFF
slice, not a claim that it diagnoses these failures. Each pair is one passing
and one failing run of the same agent → model → tool shape; output below is
copied from the CLI and asserted by
`crates/breakspan-cli/tests/diff.rs` (`failure_pairs_report_their_first_divergence`).

## The fixtures

Files live in `crates/breakspan-cli/tests/fixtures/` as
`<scenario>-pass.bspan` and `<scenario>-fail.bspan`. Each is one export of
one trace (under 1.3 KiB). Spans carry only `gen_ai.operation.name`, the
step-name attribute for their kind (`gen_ai.agent.name`,
`gen_ai.request.model`, `gen_ai.tool.name`) and, for failed tools, an error
status with the message `synthetic failure`. There are no prompts, arguments,
results, resources or scopes. Commands below run from the repository root, with
`F=crates/breakspan-cli/tests/fixtures`.

The passing run is the same for the first four scenarios:

```text
$ breakspan show $F/tool-loop-pass.bspan
Trace 01010101010101010101010101010101 (snapshot, 4 spans)
invoke_agent synthetic-agent  37ms
├── chat synthetic-model  6ms
├── execute_tool calculator  6ms
└── chat synthetic-model  6ms
```

They are defined by `failure_pairs` in
`crates/breakspan-otlp/tests/recording.rs` and recorded through the real
loopback OTLP/gRPC receiver. Regenerate all ten files with:

```sh
cargo test -p breakspan-otlp --test recording regenerate_failure_fixtures -- --ignored --exact
```

Regeneration is byte-identical. The non-ignored test
`committed_failure_fixtures_match_their_generator` fails when the committed
files and the generator drift apart.

Every passing run diffed against itself reports
`No divergence: 4 matching steps.` and exits `0`. Every pair below exits `1`.

## 1. Tool error, then recovery

The failing tool call errors; the model calls it again, which succeeds, and
then answers.

```text
$ breakspan diff $F/tool-error-recovery-pass.bspan $F/tool-error-recovery-fail.bspan
First divergence at step 3
  path: agent synthetic-agent
  a: tool calculator  depth 1  [ok]
  b: tool calculator  depth 1  [error]
2 matching steps before divergence.
```

The divergence points straight at the failed call. It says nothing about the
recovery: the extra model and tool steps that follow, and the fact that the run
eventually succeeded, are not visible without `show`.

## 2. Repeated tool loop

The failing run calls the tool two extra times, each followed by a model call,
before answering.

```text
$ breakspan diff $F/tool-loop-pass.bspan $F/tool-loop-fail.bspan
First divergence at step 5
  path: agent synthetic-agent
  a: (no more steps)
  b: tool calculator  depth 1  [ok]
4 matching steps before divergence.
```

The loop is reported only at its first extra call, as an ordinary extra step.
Nothing says how many extra calls there were, or that they repeat the step
before them. Steps 3–4 of both runs are identical, so this is
indistinguishable from a run that legitimately needed one more tool call.

## 3. Early stop

The failing run ends after its first model call without calling the tool.

```text
$ breakspan diff $F/early-stop-pass.bspan $F/early-stop-fail.bspan
First divergence at step 3
  path: agent synthetic-agent
  a: tool calculator  depth 1  [ok]
  b: (no more steps)
2 matching steps before divergence.
```

This is the clearest case: the missing step is the one the passing run took
next. Nothing in the telemetry says *why* the model stopped (finish reason,
output), and `diff` does not compare model output.

## 4. Wrong tool

The failing run calls `web_search` where the passing run calls `calculator`.

```text
$ breakspan diff $F/wrong-tool-pass.bspan $F/wrong-tool-fail.bspan
First divergence at step 3
  path: agent synthetic-agent
  a: tool calculator  depth 1  [ok]
  b: tool web_search  depth 1  [ok]
2 matching steps before divergence.
```

Both sides of the divergence are useful as reported. The earlier model step
that chose the tool is matched as identical, because prompts and outputs are
not compared.

## 5. Error below a transparent node

The tools sit under graph/chain spans without `gen_ai.operation.name`, so
those spans are classified `Other`. The failing run's tool errors:

```text
$ breakspan show $F/transparent-error-fail.bspan
Trace 01010101010101010101010101010101 (snapshot, 6 spans)
invoke_agent synthetic-agent  58ms
└── agent graph  47ms
    ├── chat synthetic-model  6ms
    ├── tool node  17ms
    │   └── execute_tool calculator  6ms [error]
    └── chat synthetic-model  6ms
```

```text
$ breakspan diff $F/transparent-error-pass.bspan $F/transparent-error-fail.bspan
First divergence at step 3
  path: agent synthetic-agent
  a: tool calculator  depth 1  [ok]
  b: tool calculator  depth 1  [error]
2 matching steps before divergence.
```

The `Other` spans stay transparent: the tool is reported at depth 1 under the
agent, exactly as in scenario 1. That also means the output does not show
*where* in the graph the tool ran; `agent graph › tool node` is visible only
through `show`.

## Observed limitations (candidate follow-ups)

These are recorded for future issues; none is implemented here.

- **Only the first difference is reported.** Recovery after an error, the rest
  of a loop and everything after a wrong tool are invisible (scenarios 1, 2, 4).
  Realignment after the divergence or a summary of remaining differences would
  address this.
- **Repetition is not recognized.** A loop is one extra step, not "the same tool
  called three times" (scenario 2).
- **Error details are not shown.** `diff` prints `[error]` but not the status
  message, and `show` prints only an error marker. The failure text is available
  in the domain but no command displays it (scenarios 1, 5).
- **Transparent ancestors are dropped from the path.** Scenarios 1 and 5 print
  identical output even though the failing tool sits in a different place
  (scenario 5).
- **A recovered run is still a divergence.** `diff` cannot tell a failure that
  changed the result from one the agent recovered from; both exit `1`
  (scenario 1).
- **Causes before the divergence are matched as equal.** The model step that
  chose the wrong tool or stopped early compares equal, because content is not
  compared (scenarios 3, 4).
