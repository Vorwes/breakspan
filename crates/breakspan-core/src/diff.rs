use crate::{
    OperationKind, Span, SpanId, SpanStatus, Trace,
    tree::{safe_text, walk},
};
use std::{collections::BTreeMap, fmt};

/// Ancestors shown in a divergence path before the middle is elided.
const MAX_PATH: usize = 64;

/// Structural comparison of two runs: where their step sequences first differ.
/// This makes no causal or semantic claim.
#[derive(Debug)]
pub struct StepComparison<'a> {
    matching: usize,
    divergence: Option<Divergence<'a>>,
}

#[derive(Debug)]
struct Divergence<'a> {
    /// Step ancestors shared by both runs at the divergent position.
    path: Vec<&'a Span>,
    a: Option<Step<'a>>,
    b: Option<Step<'a>>,
}

impl StepComparison<'_> {
    pub fn diverged(&self) -> bool {
        self.divergence.is_some()
    }
}

/// A span classified as agent, model or tool, at its position in the step tree.
#[derive(Debug, Clone, Copy)]
struct Step<'a> {
    span: &'a Span,
    depth: usize,
    /// Index of the nearest step ancestor in the same pre-order sequence.
    parent: Option<usize>,
}

impl<'a> Step<'a> {
    /// Everything compared between runs; IDs, timing and metadata are ignored.
    fn key(&self) -> (usize, OperationKind, &'a str, bool) {
        (
            self.depth,
            self.span.operation,
            step_name(self.span),
            is_error(self.span),
        )
    }
}

/// Walk both runs' step trees in the same deterministic pre-order and report
/// the first position where depth, identity or error status differs, or where
/// one run has steps left and the other does not.
pub fn compare_steps<'a>(a: &'a Trace, b: &'a Trace) -> StepComparison<'a> {
    let a_steps = steps(a);
    let b_steps = steps(b);
    let matching = a_steps
        .iter()
        .zip(&b_steps)
        .take_while(|(left, right)| left.key() == right.key())
        .count();
    let left = a_steps.get(matching).copied();
    let right = b_steps.get(matching).copied();
    let divergence = match (left, right) {
        (None, None) => None,
        (Some(anchor), other) => Some((anchor, &a_steps, other)),
        (None, Some(anchor)) => Some((anchor, &b_steps, None)),
    }
    .map(|(anchor, sequence, other)| {
        // Earlier steps match, so ancestors above the shallower side are
        // identical in both runs.
        let shared_depth = other.map_or(anchor.depth, |o| o.depth.min(anchor.depth));
        let mut path = Vec::new();
        let mut parent = anchor.parent;
        while let Some(step) = parent.and_then(|index| sequence.get(index)) {
            if step.depth < shared_depth {
                path.push(step.span);
            }
            parent = step.parent;
        }
        path.reverse();
        Divergence {
            path,
            a: left,
            b: right,
        }
    });
    StepComparison {
        matching,
        divergence,
    }
}

/// Steps in pre-order. `Other` spans are transparent: their step descendants
/// attach to the nearest step ancestor, or become roots. Siblings and roots
/// sort by start time, step identity, error status and span ID.
fn steps(trace: &Trace) -> Vec<Step<'_>> {
    let mut children: BTreeMap<Option<SpanId>, Vec<&Span>> = BTreeMap::new();
    // The shared traversal visits each span once and terminates on cycles, so
    // every step gets exactly one, earlier-visited, step parent: a forest.
    walk(
        trace,
        |_| None,
        |parent, nearest: &Option<SpanId>, _, _| {
            if is_step(parent) {
                Some(parent.id)
            } else {
                *nearest
            }
        },
        |span, nearest| {
            if is_step(span) {
                children.entry(*nearest).or_default().push(span);
            }
        },
    );
    for siblings in children.values_mut() {
        siblings.sort_by(|x, y| order(x).cmp(&order(y)));
    }
    let mut sequence = Vec::new();
    let mut stack: Vec<_> = children
        .get(&None)
        .into_iter()
        .flatten()
        .rev()
        .map(|span| (*span, 0, None))
        .collect();
    while let Some((span, depth, parent)) = stack.pop() {
        let index = sequence.len();
        sequence.push(Step {
            span,
            depth,
            parent,
        });
        if let Some(siblings) = children.get(&Some(span.id)) {
            stack.extend(
                siblings
                    .iter()
                    .rev()
                    .map(|child| (*child, depth + 1, Some(index))),
            );
        }
    }
    sequence
}

/// Every compared field of a step itself precedes the span ID, so equal start
/// times only fall back to random IDs between steps that compare equal. Such
/// steps can still differ in their descendants.
fn order(span: &Span) -> (u64, u8, &str, bool, SpanId) {
    let kind = match span.operation {
        OperationKind::Agent => 0,
        OperationKind::Model => 1,
        OperationKind::Tool => 2,
        OperationKind::Other => 3,
    };
    (
        span.timing.start_unix_nanos(),
        kind,
        step_name(span),
        is_error(span),
        span.id,
    )
}

fn is_step(span: &Span) -> bool {
    span.operation != OperationKind::Other
}

/// Step identity uses the normalized step name, falling back to the span name.
fn step_name(span: &Span) -> &str {
    span.step_name.as_deref().unwrap_or(&span.name)
}

/// Unset, OK and unknown status codes are all "not an error".
fn is_error(span: &Span) -> bool {
    matches!(span.status, SpanStatus::Error(_))
}

fn label(span: &Span) -> String {
    let kind = match span.operation {
        OperationKind::Agent => "agent",
        OperationKind::Model => "model",
        OperationKind::Tool => "tool",
        OperationKind::Other => "other",
    };
    format!("{kind} {}", safe_text(step_name(span)))
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "step" } else { "steps" }
}

impl fmt::Display for StepComparison<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let matching = self.matching;
        let Some(divergence) = &self.divergence else {
            return write!(
                f,
                "No divergence: {matching} matching {}.",
                plural(matching)
            );
        };
        writeln!(f, "First divergence at step {}", matching + 1)?;
        f.write_str("  path: ")?;
        if divergence.path.is_empty() {
            f.write_str("(root)")?;
        }
        let elided = divergence.path.len().saturating_sub(MAX_PATH);
        for (i, span) in divergence.path.iter().enumerate() {
            if i == 0 {
                f.write_str(&label(span))?;
            } else if i == 1 && elided != 0 {
                write!(f, " › … {elided} more …")?;
            } else if i > elided {
                write!(f, " › {}", label(span))?;
            }
        }
        f.write_str("\n")?;
        for (side, step) in [("a", divergence.a), ("b", divergence.b)] {
            match step {
                Some(step) => {
                    let status = if is_error(step.span) { "error" } else { "ok" };
                    writeln!(
                        f,
                        "  {side}: {}  depth {}  [{status}]",
                        label(step.span),
                        step.depth
                    )?;
                }
                None => writeln!(f, "  {side}: (no more steps)")?,
            }
        }
        write!(
            f,
            "{matching} matching {} before divergence.",
            plural(matching)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Timing, TraceId, tests::span};

    fn step(id: u8, parent: Option<u8>, operation: OperationKind, name: &str) -> Span {
        let mut span = span(id, parent, &format!("span {id}"));
        span.operation = operation;
        span.step_name = Some(name.into());
        span
    }

    fn trace(spans: impl IntoIterator<Item = Span>) -> Trace {
        let mut trace = Trace::new(TraceId::new([1; 16]).unwrap());
        for span in spans {
            trace.insert(span).unwrap();
        }
        trace
    }

    fn at(mut span: Span, start: u64) -> Span {
        span.timing = Timing::new(start, start + 1).unwrap();
        span
    }

    fn compare(a: &Trace, b: &Trace) -> String {
        compare_steps(a, b).to_string()
    }

    /// agent › model, tool, model — the calculator agent's shape.
    fn calculator() -> Vec<Span> {
        vec![
            at(step(1, None, OperationKind::Agent, "calculator"), 10),
            at(step(2, Some(1), OperationKind::Model, "gemini"), 20),
            at(step(3, Some(1), OperationKind::Tool, "calculator"), 30),
            at(step(4, Some(1), OperationKind::Model, "gemini"), 40),
        ]
    }

    /// Same structure with other IDs, timings, span names and metadata.
    fn relabeled(spans: Vec<Span>, offset: u8, delay: u64) -> Vec<Span> {
        spans
            .into_iter()
            .map(|mut span| {
                span.id = SpanId::new([span.id.bytes()[0] + offset; 8]).unwrap();
                span.parent_id = span
                    .parent_id
                    .map(|p| SpanId::new([p.bytes()[0] + offset; 8]).unwrap());
                let start = span.timing.start_unix_nanos() * 3 + delay;
                span.timing = Timing::new(start, start + 999).unwrap();
                span.name = format!("renamed {}", span.name);
                span.status = SpanStatus::Ok;
                span.attributes
                    .insert("ignored".into(), crate::AttributeValue::Int(7));
                span
            })
            .collect()
    }

    #[test]
    fn identical_runs_with_other_ids_and_timings_do_not_diverge() {
        let a = trace(calculator());
        let b = trace(relabeled(calculator(), 100, 5_000));
        let comparison = compare_steps(&a, &b);
        assert!(!comparison.diverged());
        assert_eq!(comparison.to_string(), "No divergence: 4 matching steps.");
        let empty = trace([]);
        assert_eq!(compare(&empty, &empty), "No divergence: 0 matching steps.");
    }

    #[test]
    fn extra_tool_call_is_reported_at_its_step() {
        let a = trace(calculator());
        let mut spans = calculator();
        spans.push(at(step(5, Some(1), OperationKind::Tool, "calculator"), 35));
        let b = trace(spans);
        assert_eq!(
            compare(&a, &b),
            "First divergence at step 4\n  \
               path: agent calculator\n  \
               a: model gemini  depth 1  [ok]\n  \
               b: tool calculator  depth 1  [ok]\n\
             3 matching steps before divergence."
        );
    }

    #[test]
    fn error_status_differences_diverge_but_messages_do_not() {
        let mut spans = calculator();
        spans[2].status = SpanStatus::Error("division by zero".into());
        let a = trace(spans);
        let b = trace(calculator());
        assert_eq!(
            compare(&a, &b),
            "First divergence at step 3\n  \
               path: agent calculator\n  \
               a: tool calculator  depth 1  [error]\n  \
               b: tool calculator  depth 1  [ok]\n\
             2 matching steps before divergence."
        );
        let mut spans = calculator();
        spans[2].status = SpanStatus::Error("another message".into());
        assert!(!compare_steps(&a, &trace(spans)).diverged());
        // Unset, OK and unknown codes are all "not an error".
        let mut spans = calculator();
        spans[2].status = SpanStatus::Unknown {
            code: 9,
            message: String::new(),
        };
        assert!(!compare_steps(&b, &trace(spans)).diverged());
    }

    #[test]
    fn strict_prefix_diverges_at_first_missing_step() {
        let a = trace(calculator());
        let b = trace(calculator().into_iter().take(3));
        assert_eq!(
            compare(&a, &b),
            "First divergence at step 4\n  \
               path: agent calculator\n  \
               a: model gemini  depth 1  [ok]\n  \
               b: (no more steps)\n\
             3 matching steps before divergence."
        );
        assert!(compare(&b, &a).contains("  a: (no more steps)\n  b: model gemini"));
        let empty = trace([]);
        assert!(compare(&empty, &a).contains("at step 1\n  path: (root)\n"));
    }

    #[test]
    fn identity_and_depth_differences_diverge() {
        let mut spans = calculator();
        spans[2].step_name = Some("search".into());
        assert!(compare(&trace(spans), &trace(calculator())).contains("a: tool search"));
        // Without a step name, the span name is the identity.
        let mut spans = calculator();
        spans[2].step_name = None;
        spans[2].name = "calculator".into();
        assert!(!compare_steps(&trace(spans), &trace(calculator())).diverged());
        // Same identity, different kind.
        let mut spans = calculator();
        spans[2].operation = OperationKind::Agent;
        assert!(compare(&trace(spans), &trace(calculator())).contains("a: agent calculator"));
        // The tool runs inside the first model step instead of beside it.
        let mut spans = calculator();
        spans[2].parent_id = Some(spans[1].id);
        assert_eq!(
            compare(&trace(spans), &trace(calculator())),
            "First divergence at step 3\n  \
               path: agent calculator\n  \
               a: tool calculator  depth 2  [ok]\n  \
               b: tool calculator  depth 1  [ok]\n\
             2 matching steps before divergence."
        );
        // The tool runs after the agent instead of inside it.
        let mut spans = calculator();
        spans[2].parent_id = None;
        assert!(
            compare(&trace(spans), &trace(calculator())).contains(
                "at step 3\n  path: agent calculator\n  a: model gemini  depth 1  [ok]\n"
            )
        );
    }

    #[test]
    fn arrival_order_does_not_matter() {
        let a = trace(calculator());
        let b = trace(calculator().into_iter().rev());
        assert!(!compare_steps(&a, &b).diverged());
    }

    #[test]
    fn other_spans_are_transparent() {
        // agent › graph node › (tool at 30, graph node › model at 40), model at 20
        let graph =
            |id, parent, start| at(step(id, Some(parent), OperationKind::Other, "node"), start);
        let b = trace([
            at(step(1, None, OperationKind::Agent, "calculator"), 10),
            graph(10, 1, 11),
            graph(11, 10, 12),
            at(step(2, Some(1), OperationKind::Model, "gemini"), 20),
            at(step(3, Some(10), OperationKind::Tool, "calculator"), 30),
            at(step(4, Some(11), OperationKind::Model, "gemini"), 40),
            graph(12, 4, 41),
        ]);
        assert!(!compare_steps(&trace(calculator()), &b).diverged());
        // Other roots lift their steps to roots.
        let c = trace([
            at(step(9, None, OperationKind::Other, "root"), 1),
            at(step(1, Some(9), OperationKind::Agent, "calculator"), 10),
        ]);
        let d = trace([at(step(1, None, OperationKind::Agent, "calculator"), 10)]);
        assert!(!compare_steps(&c, &d).diverged());
        // Step descendants of Other spans are still compared.
        let mut spans = vec![graph(10, 1, 11)];
        spans.extend(calculator());
        spans[3].parent_id = Some(SpanId::new([10; 8]).unwrap());
        spans[3].status = SpanStatus::Error(String::new());
        assert!(
            compare(&trace(calculator()), &trace(spans))
                .contains("b: tool calculator  depth 1  [error]")
        );
    }

    #[test]
    fn equal_start_times_order_by_identity_not_span_id() {
        let tools = |first: u8, second: u8| {
            trace([
                at(step(1, None, OperationKind::Agent, "agent"), 1),
                at(step(first, Some(1), OperationKind::Tool, "alpha"), 5),
                at(step(second, Some(1), OperationKind::Tool, "beta"), 5),
                at(step(4, Some(1), OperationKind::Model, "model"), 5),
            ])
        };
        let a = tools(2, 3);
        let b = tools(3, 2);
        assert!(!compare_steps(&a, &b).diverged());
        // Parallel calls of the same tool, one failing: the failing call's
        // span ID must not decide the order.
        let parallel = |failing: u8| {
            let mut spans = vec![
                at(step(1, None, OperationKind::Agent, "agent"), 1),
                at(step(2, Some(1), OperationKind::Tool, "calculator"), 5),
                at(step(3, Some(1), OperationKind::Tool, "calculator"), 5),
            ];
            spans[usize::from(failing) - 1].status = SpanStatus::Error(String::new());
            trace(spans)
        };
        assert!(!compare_steps(&parallel(2), &parallel(3)).diverged());
        // Models sort before tools at equal start times.
        let mut spans = vec![at(step(5, Some(1), OperationKind::Tool, "gamma"), 5)];
        spans.extend(a.spans().values().cloned());
        assert_eq!(
            compare(&a, &trace(spans)),
            "First divergence at step 5\n  \
               path: agent agent\n  \
               a: (no more steps)\n  \
               b: tool gamma  depth 1  [ok]\n\
             4 matching steps before divergence."
        );
    }

    #[test]
    fn malformed_hierarchies_terminate_deterministically() {
        let malformed = || {
            trace([
                // Missing parent: a root.
                at(step(1, Some(99), OperationKind::Agent, "orphan"), 1),
                // Two-span cycle and a self-parent.
                at(step(2, Some(3), OperationKind::Tool, "cycle a"), 2),
                at(step(3, Some(2), OperationKind::Tool, "cycle b"), 3),
                at(step(4, Some(4), OperationKind::Model, "self"), 4),
                // A second true root.
                at(step(5, None, OperationKind::Agent, "root"), 5),
            ])
        };
        let a = malformed();
        let b = trace(malformed().spans().values().rev().cloned());
        assert!(!compare_steps(&a, &b).diverged());
        assert_eq!(steps(&a).len(), 5);
        let order: Vec<_> = steps(&a)
            .iter()
            .map(|s| (step_name(s.span), s.depth))
            .collect();
        assert_eq!(
            order,
            [
                ("orphan", 0),
                ("cycle a", 0),
                ("cycle b", 1),
                ("self", 0),
                ("root", 0)
            ]
        );
        let deep = |len: u64| {
            let mut trace = Trace::new(TraceId::new([1; 16]).unwrap());
            for i in 1..=len {
                let mut s = step(1, None, OperationKind::Agent, "deep");
                s.id = SpanId::new(i.to_be_bytes()).unwrap();
                s.parent_id = (i > 1).then(|| SpanId::new((i - 1).to_be_bytes()).unwrap());
                trace.insert(s).unwrap();
            }
            trace
        };
        let (long, short) = (deep(2000), deep(1999));
        let output = compare(&long, &short);
        assert!(output.starts_with(
            "First divergence at step 2000\n  path: agent deep › … 1935 more … › agent deep"
        ));
        assert_eq!(output.matches("agent deep").count(), 65);
    }

    #[test]
    fn labels_are_escaped_and_truncated() {
        let mut spans = calculator();
        spans[2].step_name = Some(format!("\x1b[2J{}", "x".repeat(300)));
        let output = compare(&trace(spans), &trace(calculator()));
        assert!(!output.contains('\x1b'));
        assert!(output.contains(&format!(
            "a: tool \\u{{1b}}[2J{}…  depth 1",
            "x".repeat(196)
        )));
    }
}
