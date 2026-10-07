use breakspan_core::{
    AttributeValue, Attributes, InstrumentationScope, OperationKind, Span, SpanId, SpanStatus,
    Timing, TraceId,
};
use opentelemetry_proto::tonic::{
    collector::trace::v1::ExportTraceServiceRequest,
    common::v1::{AnyValue, KeyValue, any_value::Value},
    trace::v1::{ResourceSpans, ScopeSpans, Span as RawSpan},
};

#[cfg(test)]
#[path = "normalize_tests.rs"]
mod tests;

const MAX_SPANS_PER_EXPORT: usize = 1024;
const MAX_ATTRIBUTE_DEPTH: usize = 16;
const MAX_NORMALIZED_BYTES: usize = 16 * 1024 * 1024;

// These strings stay internal; protocol errors deliberately contain no content.
type ParseResult<T> = Result<T, &'static str>;

pub(crate) struct NormalizedExport {
    pub spans: Vec<Span>,
    pub rejected: i64,
}

pub(crate) fn normalize(request: ExportTraceServiceRequest) -> NormalizedExport {
    let mut result = NormalizedExport {
        spans: Vec::new(),
        rejected: 0,
    };
    let mut budget = MAX_NORMALIZED_BYTES;
    let mut seen = 0;
    for resource in &request.resource_spans {
        for scope in &resource.scope_spans {
            for raw in &scope.spans {
                seen += 1;
                if seen > MAX_SPANS_PER_EXPORT {
                    result.rejected += 1;
                    continue;
                }
                let before = budget;
                match normalize_span(raw, resource, scope, &mut budget) {
                    Ok(span) => result.spans.push(span),
                    Err(_) => {
                        budget = before;
                        result.rejected += 1;
                    }
                }
            }
        }
    }
    result
}

fn normalize_span(
    raw: &RawSpan,
    resource: &ResourceSpans,
    scope: &ScopeSpans,
    budget: &mut usize,
) -> ParseResult<Span> {
    let trace_bytes = raw
        .trace_id
        .as_slice()
        .try_into()
        .map_err(|_| "invalid trace ID length")?;
    let span_bytes = raw
        .span_id
        .as_slice()
        .try_into()
        .map_err(|_| "invalid span ID length")?;
    let trace_id = TraceId::new(trace_bytes).map_err(|_| "zero trace ID")?;
    let id = SpanId::new(span_bytes).map_err(|_| "zero span ID")?;
    let parent_id = if raw.parent_span_id.is_empty() {
        None
    } else {
        let bytes = raw
            .parent_span_id
            .as_slice()
            .try_into()
            .map_err(|_| "invalid parent ID length")?;
        // OTLP specifies an empty parent for roots, not eight zero bytes.
        Some(SpanId::new(bytes).map_err(|_| "zero parent ID")?)
    };
    let timing = Timing::new(raw.start_time_unix_nano, raw.end_time_unix_nano)
        .map_err(|_| "reversed timing")?;
    spend(budget, 1024)?;
    let attributes = attributes(&raw.attributes, 0, budget)?;
    let operation = match attributes.get("gen_ai.operation.name") {
        Some(AttributeValue::String(name)) => match name.as_str() {
            "invoke_agent" => OperationKind::Agent,
            "chat" | "text_completion" | "generate_content" | "embeddings" => OperationKind::Model,
            "execute_tool" => OperationKind::Tool,
            _ => OperationKind::Other,
        },
        _ => OperationKind::Other,
    };
    let status = match &raw.status {
        None => SpanStatus::Unset,
        Some(status) => match status.code {
            0 => SpanStatus::Unset,
            1 => SpanStatus::Ok,
            2 => SpanStatus::Error(copy_text(&status.message, budget)?),
            code => SpanStatus::Unknown {
                code,
                message: copy_text(&status.message, budget)?,
            },
        },
    };
    let resource_attributes = match &resource.resource {
        Some(resource) => self::attributes(&resource.attributes, 0, budget)?,
        None => Attributes::new(),
    };
    let instrumentation = match &scope.scope {
        Some(instrumentation) => InstrumentationScope {
            name: copy_text(&instrumentation.name, budget)?,
            version: copy_text(&instrumentation.version, budget)?,
            attributes: self::attributes(&instrumentation.attributes, 0, budget)?,
            schema_url: copy_text(&scope.schema_url, budget)?,
        },
        None => InstrumentationScope {
            schema_url: copy_text(&scope.schema_url, budget)?,
            ..Default::default()
        },
    };
    Ok(Span {
        trace_id,
        id,
        parent_id,
        name: copy_text(&raw.name, budget)?,
        timing,
        status,
        operation,
        attributes,
        resource_attributes,
        resource_schema_url: copy_text(&resource.schema_url, budget)?,
        scope: instrumentation,
    })
}

fn spend(budget: &mut usize, bytes: usize) -> ParseResult<()> {
    *budget = budget
        .checked_sub(bytes)
        .ok_or("normalized export exceeds budget")?;
    Ok(())
}
fn copy_text(text: &str, budget: &mut usize) -> ParseResult<String> {
    spend(budget, text.len())?;
    Ok(text.to_owned())
}
fn attributes(raw: &[KeyValue], depth: usize, budget: &mut usize) -> ParseResult<Attributes> {
    let mut result = Attributes::new();
    for kv in raw {
        spend(budget, 96)?;
        let key = copy_text(&kv.key, budget)?;
        let value = match &kv.value {
            Some(value) => attribute_value(value, depth, budget)?,
            None => AttributeValue::Empty,
        };
        // Invalid duplicate keys are deterministic: last value wins.
        result.insert(key, value);
    }
    Ok(result)
}
fn attribute_value(
    raw: &AnyValue,
    depth: usize,
    budget: &mut usize,
) -> ParseResult<AttributeValue> {
    if depth > MAX_ATTRIBUTE_DEPTH {
        return Err("attributes nested too deeply");
    }
    spend(budget, 64)?;
    Ok(match &raw.value {
        None | Some(Value::StringValueStrindex(_)) => AttributeValue::Empty,
        Some(Value::StringValue(s)) => AttributeValue::String(copy_text(s, budget)?),
        Some(Value::BoolValue(b)) => AttributeValue::Bool(*b),
        Some(Value::IntValue(i)) => AttributeValue::Int(*i),
        Some(Value::DoubleValue(f)) => AttributeValue::Double(*f),
        Some(Value::BytesValue(b)) => {
            spend(budget, b.len())?;
            AttributeValue::Bytes(b.clone())
        }
        Some(Value::ArrayValue(a)) => AttributeValue::Array(
            a.values
                .iter()
                .map(|v| attribute_value(v, depth + 1, budget))
                .collect::<ParseResult<_>>()?,
        ),
        Some(Value::KvlistValue(m)) => {
            AttributeValue::Map(attributes(&m.values, depth + 1, budget)?)
        }
    })
}
