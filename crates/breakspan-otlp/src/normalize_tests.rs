use super::*;
use opentelemetry_proto::tonic::resource::v1::Resource;

fn raw_span(id: usize) -> RawSpan {
    RawSpan {
        trace_id: vec![1; 16],
        span_id: (id as u64).to_be_bytes().to_vec(),
        name: "span".into(),
        start_time_unix_nano: 1,
        end_time_unix_nano: 2,
        ..Default::default()
    }
}
fn resource(spans: Vec<RawSpan>) -> ResourceSpans {
    ResourceSpans {
        scope_spans: vec![ScopeSpans {
            spans,
            ..Default::default()
        }],
        ..Default::default()
    }
}

#[test]
fn span_count_limit_is_reported() {
    let request = ExportTraceServiceRequest {
        resource_spans: vec![resource((1..=1025).map(raw_span).collect())],
    };
    let parsed = normalize(request);
    assert_eq!(parsed.rejected, 1);
    assert_eq!(parsed.spans.len(), 1024);
}

#[test]
fn resource_amplification_is_bounded_and_smaller_siblings_can_still_fit() {
    let mut large = resource((1..=40).map(raw_span).collect());
    large.resource = Some(Resource {
        attributes: vec![KeyValue {
            key: "large".into(),
            value: Some(AnyValue {
                value: Some(Value::StringValue("x".repeat(512 * 1024))),
            }),
            ..Default::default()
        }],
        ..Default::default()
    });
    let parsed = normalize(ExportTraceServiceRequest {
        resource_spans: vec![large, resource(vec![raw_span(41)])],
    });
    assert!(parsed.rejected > 0);
    assert_eq!(parsed.spans.len() + parsed.rejected as usize, 41);
    assert!(
        parsed
            .spans
            .iter()
            .any(|s| s.id.bytes() == &41_u64.to_be_bytes())
    );
    assert!(parsed.spans.len() < 40);
}

#[test]
fn duplicate_keys_are_last_wins_and_profiling_references_are_nonfatal() {
    let mut raw = raw_span(1);
    raw.attributes = vec![
        KeyValue {
            key: "duplicate".into(),
            value: Some(AnyValue {
                value: Some(Value::IntValue(1)),
            }),
            ..Default::default()
        },
        KeyValue {
            key: "duplicate".into(),
            value: Some(AnyValue {
                value: Some(Value::IntValue(2)),
            }),
            ..Default::default()
        },
        KeyValue {
            key: "profile-only".into(),
            value: Some(AnyValue {
                value: Some(Value::StringValueStrindex(7)),
            }),
            ..Default::default()
        },
    ];
    let parsed = normalize(ExportTraceServiceRequest {
        resource_spans: vec![resource(vec![raw])],
    });
    assert_eq!(parsed.rejected, 0);
    assert_eq!(
        parsed.spans[0].attributes["duplicate"],
        AttributeValue::Int(2)
    );
    assert_eq!(
        parsed.spans[0].attributes["profile-only"],
        AttributeValue::Empty
    );
}
