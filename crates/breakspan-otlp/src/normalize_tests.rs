use super::*;
use opentelemetry_proto::tonic::{
    common::v1::InstrumentationScope as RawInstrumentationScope, resource::v1::Resource,
    trace::v1::Status as RawStatus,
};

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
fn status_and_scope_conversion_preserve_budget_accounting() {
    let resource = ResourceSpans {
        schema_url: "resource-schema".into(),
        ..Default::default()
    };
    let statuses = [
        (None, SpanStatus::Unset),
        (
            Some(RawStatus {
                code: 0,
                message: "ignored".into(),
            }),
            SpanStatus::Unset,
        ),
        (
            Some(RawStatus {
                code: 1,
                message: "ignored".into(),
            }),
            SpanStatus::Ok,
        ),
        (
            Some(RawStatus {
                code: 2,
                message: "failure".into(),
            }),
            SpanStatus::Error("failure".into()),
        ),
        (
            Some(RawStatus {
                code: 99,
                message: "échec".into(),
            }),
            SpanStatus::Unknown {
                code: 99,
                message: "échec".into(),
            },
        ),
    ];
    for (status, expected_status) in statuses {
        for metadata in [
            None,
            Some(RawInstrumentationScope {
                name: "instrumentation".into(),
                version: "1.0".into(),
                ..Default::default()
            }),
        ] {
            let mut raw = raw_span(1);
            raw.status = status.clone();
            let scope = ScopeSpans {
                scope: metadata,
                schema_url: "scope-schema".into(),
                ..Default::default()
            };
            let message_bytes = match &expected_status {
                SpanStatus::Error(message) | SpanStatus::Unknown { message, .. } => message.len(),
                _ => 0,
            };
            let scope_bytes = scope
                .scope
                .as_ref()
                .map(|s| s.name.len() + s.version.len())
                .unwrap_or(0);
            let expected_charge = 1024
                + raw.name.len()
                + message_bytes
                + scope_bytes
                + resource.schema_url.len()
                + scope.schema_url.len();
            let mut budget = 4096;
            let parsed = normalize_span(&raw, &resource, &scope, &mut budget).unwrap();
            assert_eq!(parsed.status, expected_status);
            let expected_scope = InstrumentationScope {
                name: scope
                    .scope
                    .as_ref()
                    .map(|s| s.name.clone())
                    .unwrap_or_default(),
                version: scope
                    .scope
                    .as_ref()
                    .map(|s| s.version.clone())
                    .unwrap_or_default(),
                schema_url: scope.schema_url.clone(),
                ..Default::default()
            };
            assert_eq!(parsed.scope, expected_scope);
            assert_eq!(parsed.resource_schema_url, resource.schema_url);
            assert_eq!(budget, 4096 - expected_charge);
        }
    }
}

#[test]
fn status_and_scope_copies_respect_budget_boundaries() {
    let cases = [
        (
            Some(RawStatus {
                code: 2,
                message: "x".repeat(32),
            }),
            None,
        ),
        (
            None,
            Some(RawInstrumentationScope {
                name: "x".repeat(32),
                ..Default::default()
            }),
        ),
    ];
    for (status, metadata) in cases {
        let mut raw = raw_span(1);
        raw.status = status;
        let resource = ResourceSpans::default();
        let scope = ScopeSpans {
            scope: metadata,
            ..Default::default()
        };
        let mut insufficient = 1024 + 31;
        assert_eq!(
            normalize_span(&raw, &resource, &scope, &mut insufficient),
            Err("normalized export exceeds budget")
        );
        let exact_charge = 1024 + 32 + raw.name.len();
        let mut exact = exact_charge;
        assert!(normalize_span(&raw, &resource, &scope, &mut exact).is_ok());
        assert_eq!(exact, 0);
        let mut one_byte_short = exact_charge - 1;
        assert!(normalize_span(&raw, &resource, &scope, &mut one_byte_short).is_err());
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
