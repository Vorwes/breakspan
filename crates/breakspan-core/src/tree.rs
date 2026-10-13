use crate::{Span, SpanId, SpanStatus, Trace};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write,
};

/// Render a provisional hierarchy, ordered by start time then span ID.
/// Missing parents remain visible, cycles cannot recurse forever, and traversal
/// is iterative so deeply nested untrusted input cannot overflow the stack.
pub fn render_tree(trace: &Trace) -> String {
    let hierarchy = Hierarchy::new(trace);
    let mut output = format!(
        "Trace {} (snapshot, {} spans)\n",
        trace.id(),
        trace.spans().len()
    );
    let mut visited = BTreeSet::new();
    // Unvisited components after roots are necessarily parent cycles.
    for seed in hierarchy
        .roots
        .iter()
        .chain(&hierarchy.ordered_spans)
        .copied()
    {
        if visited.contains(&seed.id) {
            continue;
        }
        let root = component_root(trace, seed);
        render_component(trace, &hierarchy, root, &mut visited, &mut output);
    }
    output
}

/// A sorted index borrowing spans, not a second domain model.
struct Hierarchy<'a> {
    children: BTreeMap<SpanId, Vec<&'a Span>>,
    roots: Vec<&'a Span>,
    ordered_spans: Vec<&'a Span>,
}

impl<'a> Hierarchy<'a> {
    fn new(trace: &'a Trace) -> Self {
        let mut children: BTreeMap<SpanId, Vec<&Span>> = BTreeMap::new();
        let mut roots = Vec::new();
        for span in trace.spans().values() {
            match span.parent_id {
                Some(parent) if trace.spans().contains_key(&parent) => {
                    children.entry(parent).or_default().push(span);
                }
                _ => roots.push(span),
            }
        }
        let order = |span: &&Span| (span.timing.start_unix_nanos(), span.id);
        roots.sort_by_key(order);
        for siblings in children.values_mut() {
            siblings.sort_by_key(order);
        }
        let mut ordered_spans: Vec<_> = trace.spans().values().collect();
        ordered_spans.sort_by_key(order);
        Self {
            children,
            roots,
            ordered_spans,
        }
    }
}

fn component_root<'a>(trace: &'a Trace, seed: &'a Span) -> &'a Span {
    // A remaining seed may be a descendant of a cycle with an earlier
    // timestamp (clock skew). Walk to the cycle before rendering, rather
    // than splitting one component into several apparent roots.
    let mut root = seed;
    let mut ancestry = BTreeSet::new();
    while let Some(parent_id) = root.parent_id {
        if !ancestry.insert(root.id) {
            break;
        }
        match trace.spans().get(&parent_id) {
            Some(parent) => root = parent,
            None => break,
        }
    }
    root
}

fn render_component(
    trace: &Trace,
    hierarchy: &Hierarchy<'_>,
    root: &Span,
    visited: &mut BTreeSet<SpanId>,
    output: &mut String,
) {
    let annotation = match root.parent_id {
        Some(parent) if !trace.spans().contains_key(&parent) => " [missing parent]",
        Some(_) => " [cyclic parent component]",
        None => "",
    };
    let mut stack = vec![(root, Vec::<bool>::new(), annotation)];
    while let Some((span, path, note)) = stack.pop() {
        if !visited.insert(span.id) {
            continue;
        }
        write_span_line(output, span, &path, note);
        if let Some(siblings) = hierarchy.children.get(&span.id) {
            for (i, child) in siblings.iter().enumerate().rev() {
                if visited.contains(&child.id) {
                    continue;
                }
                // Cap visual indentation and path allocation, not traversal depth.
                let mut child_path = path.clone();
                if child_path.len() < 65 {
                    child_path.push(i + 1 == siblings.len());
                } else if let Some(last) = child_path.last_mut() {
                    *last = i + 1 == siblings.len();
                }
                stack.push((child, child_path, ""));
            }
        }
    }
}

fn write_span_line(output: &mut String, span: &Span, path: &[bool], note: &str) {
    let depth = path.len();
    for &last in path.iter().take(64).take(depth.saturating_sub(1)) {
        output.push_str(if last { "    " } else { "│   " });
    }
    if let Some(last) = path.last() {
        output.push_str(if *last { "└── " } else { "├── " });
    }
    let name = safe_text(&span.name);
    let duration = span.timing.duration().as_secs_f64();
    let time = if duration >= 1.0 {
        format!("{duration:.2}s")
    } else {
        format!("{:.0}ms", duration * 1000.0)
    };
    let status = match &span.status {
        SpanStatus::Error(_) => " [error]",
        SpanStatus::Unknown { .. } => " [unknown status]",
        _ => "",
    };
    let _ = writeln!(output, "{name}  {time}{status}{note}");
}

// Names are external input: escape terminal controls and bound each label.
fn safe_text(text: &str) -> String {
    let mut result = String::new();
    for c in text.chars().take(200) {
        if c.is_control() {
            result.extend(c.escape_default());
        } else {
            result.push(c);
        }
    }
    if text.chars().nth(200).is_some() {
        result.push('…');
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{TraceId, tests::span};

    #[test]
    fn empty_trace_has_only_a_snapshot_header() {
        let trace = Trace::new(TraceId::new([1; 16]).unwrap());
        assert_eq!(
            render_tree(&trace),
            "Trace 01010101010101010101010101010101 (snapshot, 0 spans)\n"
        );
    }

    #[test]
    fn snapshot_formatting_and_forest_order_are_stable() {
        let mut trace = Trace::new(TraceId::new([1; 16]).unwrap());
        let mut root = span(1, None, "root");
        root.timing = crate::Timing::new(1, 1_000_000_001).unwrap();
        let mut child = span(2, Some(1), "child\t");
        child.timing = crate::Timing::new(2, 999_000_002).unwrap();
        child.status = SpanStatus::Error("not printed".into());
        let mut other_root = span(3, None, &"x".repeat(201));
        other_root.status = SpanStatus::Unknown {
            code: 99,
            message: "not printed".into(),
        };
        for span in [span(4, Some(99), "orphan"), other_root, child, root] {
            trace.insert(span).unwrap();
        }
        assert_eq!(
            render_tree(&trace),
            format!(
                "Trace 01010101010101010101010101010101 (snapshot, 4 spans)\n\
                 root  1.00s\n\
                 └── child\\t  999ms [error]\n\
                 {}…  6ms [unknown status]\n\
                 orphan  6ms [missing parent]\n",
                "x".repeat(200)
            )
        );
    }

    #[test]
    fn hierarchy_is_independent_of_arrival_order() {
        let mut trace = Trace::new(TraceId::new([1; 16]).unwrap());
        trace
            .insert(span(3, Some(1), "execute_tool read_file"))
            .unwrap();
        assert!(render_tree(&trace).contains("[missing parent]"));
        trace.insert(span(2, Some(1), "chat")).unwrap();
        trace.insert(span(1, None, "invoke_agent")).unwrap();
        let tree = render_tree(&trace);
        assert!(tree.contains("invoke_agent  6ms\n├── chat  6ms\n└── execute_tool read_file  6ms"));
        assert!(!tree.contains("missing parent"));
        let mut reverse = Trace::new(trace.id());
        for s in trace.spans().values().rev() {
            reverse.insert(s.clone()).unwrap();
        }
        assert_eq!(tree, render_tree(&reverse));
    }

    #[test]
    fn cycles_and_terminal_controls_are_safe() {
        let mut trace = Trace::new(TraceId::new([1; 16]).unwrap());
        trace.insert(span(1, Some(2), "one\x1b[2J\ntwo")).unwrap();
        trace.insert(span(2, Some(1), "child")).unwrap();
        trace.insert(span(3, Some(3), "self")).unwrap();
        let tree = render_tree(&trace);
        assert_eq!(tree.matches("cyclic parent component").count(), 2);
        assert_eq!(tree.matches("child").count(), 1);
        assert!(!tree.contains('\x1b'));
        assert!(tree.contains("\\n"));
    }

    #[test]
    fn early_cycle_descendants_do_not_split_the_component() {
        let mut trace = Trace::new(TraceId::new([1; 16]).unwrap());
        trace.insert(span(1, Some(2), "early child")).unwrap();
        trace.insert(span(2, Some(3), "cycle a")).unwrap();
        trace.insert(span(3, Some(2), "cycle b")).unwrap();
        trace.insert(span(4, Some(1), "grandchild")).unwrap();
        let tree = render_tree(&trace);
        assert_eq!(tree.matches("cyclic parent component").count(), 1);
        assert!(tree.contains("cycle a  6ms [cyclic parent component]\n├── early child  6ms\n│   └── grandchild  6ms\n└── cycle b  6ms"));
    }

    #[test]
    fn deep_hierarchy_uses_bounded_indentation() {
        let mut trace = Trace::new(TraceId::new([1; 16]).unwrap());
        for i in 1_u64..=2000 {
            let mut s = span(1, None, "deep");
            s.id = SpanId::new(i.to_be_bytes()).unwrap();
            s.parent_id = if i == 1 {
                None
            } else {
                Some(SpanId::new((i - 1).to_be_bytes()).unwrap())
            };
            trace.insert(s).unwrap();
        }
        let tree = render_tree(&trace);
        assert_eq!(tree.matches("deep").count(), 2000);
        assert!(tree.len() < 600_000);
    }
}
