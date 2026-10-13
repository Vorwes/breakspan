//! Bounded, process-local trace storage. No persistence or eviction yet.

use breakspan_core::{AttributeValue, Attributes, Span, Trace, TraceId};
use std::{collections::BTreeMap, sync::Arc};
use tokio::sync::{Mutex, watch};

#[derive(Debug, Clone, Copy)]
pub struct StoreLimits {
    pub max_spans: usize,
    /// Conservative accounting budget, not an exact allocator/RSS limit.
    pub max_bytes: usize,
}
impl Default for StoreLimits {
    fn default() -> Self {
        Self {
            max_spans: 10_000,
            max_bytes: 64 * 1024 * 1024,
        }
    }
}

#[derive(Default)]
struct State {
    traces: BTreeMap<TraceId, Trace>,
    spans: usize,
    bytes: usize,
}

#[derive(Clone)]
pub struct MemoryStore {
    state: Arc<Mutex<State>>,
    limits: StoreLimits,
    revision: watch::Sender<u64>,
}
impl Default for MemoryStore {
    fn default() -> Self {
        Self::new(StoreLimits::default())
    }
}
impl MemoryStore {
    pub fn new(limits: StoreLimits) -> Self {
        let (revision, _) = watch::channel(0);
        Self {
            state: Arc::new(Mutex::new(State::default())),
            limits,
            revision,
        }
    }
    /// Coalescing notification: query current snapshots after a revision changes.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.revision.subscribe()
    }

    /// Returns the number rejected by capacity limits. Existing span IDs are
    /// last-export-wins; retries do not consume another span slot.
    pub async fn ingest(&self, spans: Vec<Span>) -> usize {
        let mut state = self.state.lock().await;
        let mut rejected = 0;
        let mut changed = false;
        for span in spans {
            let old = state
                .traces
                .get(&span.trace_id)
                .and_then(|t| t.spans().get(&span.id));
            let is_new = old.is_none();
            let previous_bytes = old.map(span_bytes).unwrap_or(0);
            let next_bytes = state
                .bytes
                .saturating_sub(previous_bytes)
                .saturating_add(span_bytes(&span));
            if (is_new && state.spans >= self.limits.max_spans)
                || next_bytes > self.limits.max_bytes
            {
                rejected += 1;
                continue;
            }
            let trace = state
                .traces
                .entry(span.trace_id)
                .or_insert_with(|| Trace::new(span.trace_id));
            // Trace construction above guarantees matching IDs. Still propagate a
            // rejection instead of panicking if that invariant changes later.
            if trace.insert(span).is_err() {
                rejected += 1;
                continue;
            }
            state.bytes = next_bytes;
            if is_new {
                state.spans += 1;
            }
            changed = true;
        }
        if changed {
            self.revision.send_modify(|r| *r = r.wrapping_add(1));
        }
        rejected
    }

    pub async fn trace(&self, id: TraceId) -> Option<Trace> {
        self.state.lock().await.traces.get(&id).cloned()
    }
    pub async fn snapshots(&self) -> Vec<Trace> {
        self.state.lock().await.traces.values().cloned().collect()
    }
}

fn attributes_bytes(attributes: &Attributes) -> usize {
    attributes
        .iter()
        .map(|(key, value)| key.len() + 96 + value_bytes(value))
        .sum()
}
fn value_bytes(value: &AttributeValue) -> usize {
    match value {
        AttributeValue::String(s) => s.len(),
        AttributeValue::Bytes(b) => b.len(),
        AttributeValue::Array(a) => a.iter().map(|v| 64 + value_bytes(v)).sum(),
        AttributeValue::Map(m) => attributes_bytes(m),
        _ => 0,
    }
}
fn span_bytes(span: &Span) -> usize {
    let status_bytes = match &span.status {
        breakspan_core::SpanStatus::Error(message)
        | breakspan_core::SpanStatus::Unknown { message, .. } => message.len(),
        _ => 0,
    };
    1024 + span.name.len()
        + status_bytes
        + attributes_bytes(&span.attributes)
        + attributes_bytes(&span.resource_attributes)
        + span.resource_schema_url.len()
        + span.scope.name.len()
        + span.scope.version.len()
        + span.scope.schema_url.len()
        + attributes_bytes(&span.scope.attributes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use breakspan_core::{
        InstrumentationScope, OperationKind, SpanId, SpanStatus, Timing, render_tree,
    };

    fn span(id: u8, parent: Option<u8>) -> Span {
        Span {
            trace_id: TraceId::new([1; 16]).unwrap(),
            id: SpanId::new([id; 8]).unwrap(),
            parent_id: parent.map(|p| SpanId::new([p; 8]).unwrap()),
            name: format!("span{id}"),
            timing: Timing::new(u64::from(id), u64::from(id) + 1).unwrap(),
            status: SpanStatus::Unset,
            operation: OperationKind::Other,
            attributes: Attributes::new(),
            resource_attributes: Attributes::new(),
            resource_schema_url: String::new(),
            scope: InstrumentationScope::default(),
        }
    }

    #[tokio::test]
    async fn late_parents_repair_snapshots_and_retries_replace() {
        let store = MemoryStore::default();
        let mut revision = store.subscribe();
        let id = span(1, None).trace_id;
        assert_eq!(store.ingest(vec![span(2, Some(1))]).await, 0);
        revision.changed().await.unwrap();
        assert!(render_tree(&store.trace(id).await.unwrap()).contains("missing parent"));
        assert_eq!(store.ingest(vec![span(1, None)]).await, 0);
        let mut updated = span(2, Some(1));
        updated.name = "replacement".into();
        store.ingest(vec![updated]).await;
        let trace = store.trace(id).await.unwrap();
        assert_eq!(trace.spans().len(), 2);
        assert!(render_tree(&trace).contains("└── replacement"));
        assert!(!render_tree(&trace).contains("missing parent"));
    }

    #[tokio::test]
    async fn limits_reject_new_spans_but_allow_replacements() {
        let store = MemoryStore::new(StoreLimits {
            max_spans: 1,
            max_bytes: 4096,
        });
        assert_eq!(store.ingest(vec![span(1, None), span(2, None)]).await, 1);
        assert_eq!(store.ingest(vec![span(1, None)]).await, 0);
        let mut huge = span(1, None);
        huge.name = "x".repeat(4096);
        assert_eq!(store.ingest(vec![huge]).await, 1);
        assert_eq!(
            store.snapshots().await[0]
                .spans()
                .values()
                .next()
                .unwrap()
                .name,
            "span1"
        );
    }

    #[tokio::test]
    async fn different_traces_never_share_a_parent() {
        let store = MemoryStore::default();
        let mut child = span(2, Some(1));
        child.trace_id = TraceId::new([2; 16]).unwrap();
        store.ingest(vec![span(1, None), child]).await;
        let traces = store.snapshots().await;
        assert_eq!(traces.len(), 2);
        assert!(render_tree(&traces[1]).contains("missing parent"));
    }
}
