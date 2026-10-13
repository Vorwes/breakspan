//! Breakspan's framework-independent execution model.

mod tree;

use std::{collections::BTreeMap, fmt, time::Duration};
pub use tree::render_tree;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelError {
    ZeroId,
    ReversedTiming,
    TraceMismatch,
}

impl fmt::Display for ModelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ZeroId => "identifier must not be all zero",
            Self::ReversedTiming => "span ends before it starts",
            Self::TraceMismatch => "span belongs to another trace",
        })
    }
}
impl std::error::Error for ModelError {}

macro_rules! identifier {
    ($name:ident, $size:expr) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name([u8; $size]);
        impl $name {
            pub fn new(bytes: [u8; $size]) -> Result<Self, ModelError> {
                if bytes.iter().all(|byte| *byte == 0) {
                    return Err(ModelError::ZeroId);
                }
                Ok(Self(bytes))
            }
            pub fn bytes(&self) -> &[u8; $size] {
                &self.0
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                for byte in self.0 {
                    write!(f, "{byte:02x}")?;
                }
                Ok(())
            }
        }
    };
}
identifier!(TraceId, 16);
identifier!(SpanId, 8);

/// Nanoseconds since Unix epoch. Zero is allowed; no timestamp is invented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    start_unix_nanos: u64,
    end_unix_nanos: u64,
}
impl Timing {
    pub fn new(start_unix_nanos: u64, end_unix_nanos: u64) -> Result<Self, ModelError> {
        if end_unix_nanos < start_unix_nanos {
            return Err(ModelError::ReversedTiming);
        }
        Ok(Self {
            start_unix_nanos,
            end_unix_nanos,
        })
    }
    pub fn start_unix_nanos(&self) -> u64 {
        self.start_unix_nanos
    }
    pub fn end_unix_nanos(&self) -> u64 {
        self.end_unix_nanos
    }
    pub fn duration(&self) -> Duration {
        Duration::from_nanos(self.end_unix_nanos - self.start_unix_nanos)
    }
}

pub type Attributes = BTreeMap<String, AttributeValue>;

#[derive(Debug, Clone, PartialEq)]
pub enum AttributeValue {
    Empty,
    String(String),
    Bool(bool),
    Int(i64),
    Double(f64),
    Bytes(Vec<u8>),
    Array(Vec<AttributeValue>),
    Map(Attributes),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpanStatus {
    Unset,
    Ok,
    Error(String),
    Unknown { code: i32, message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationKind {
    Agent,
    Model,
    Tool,
    Other,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct InstrumentationScope {
    pub name: String,
    pub version: String,
    pub attributes: Attributes,
    pub schema_url: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Span {
    pub trace_id: TraceId,
    pub id: SpanId,
    pub parent_id: Option<SpanId>,
    pub name: String,
    pub timing: Timing,
    pub status: SpanStatus,
    pub operation: OperationKind,
    pub attributes: Attributes,
    pub resource_attributes: Attributes,
    pub resource_schema_url: String,
    pub scope: InstrumentationScope,
}

/// One trace is the initial run grouping. No framework-specific run ID is guessed.
#[derive(Debug, Clone, PartialEq)]
pub struct Trace {
    id: TraceId,
    spans: BTreeMap<SpanId, Span>,
}
impl Trace {
    pub fn new(id: TraceId) -> Self {
        Self {
            id,
            spans: BTreeMap::new(),
        }
    }
    pub fn id(&self) -> TraceId {
        self.id
    }
    pub fn spans(&self) -> &BTreeMap<SpanId, Span> {
        &self.spans
    }
    /// Retransmitted spans replace the previous snapshot with the same ID.
    pub fn insert(&mut self, span: Span) -> Result<Option<Span>, ModelError> {
        if span.trace_id != self.id {
            return Err(ModelError::TraceMismatch);
        }
        Ok(self.spans.insert(span.id, span))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn span(id: u8, parent: Option<u8>, name: &str) -> Span {
        Span {
            trace_id: TraceId::new([1; 16]).unwrap(),
            id: SpanId::new([id; 8]).unwrap(),
            parent_id: parent.map(|p| SpanId::new([p; 8]).unwrap()),
            name: name.into(),
            timing: Timing::new(u64::from(id), u64::from(id) + 6_000_000).unwrap(),
            status: SpanStatus::Unset,
            operation: OperationKind::Other,
            attributes: Attributes::new(),
            resource_attributes: Attributes::new(),
            resource_schema_url: String::new(),
            scope: InstrumentationScope::default(),
        }
    }

    #[test]
    fn invalid_states_are_rejected() {
        assert_eq!(SpanId::new([0; 8]), Err(ModelError::ZeroId));
        assert_eq!(TraceId::new([0; 16]), Err(ModelError::ZeroId));
        assert_eq!(Timing::new(2, 1), Err(ModelError::ReversedTiming));
        assert_eq!(Timing::new(0, 0).unwrap().duration(), Duration::ZERO);
        assert_eq!(SpanId::new([1; 8]).unwrap().to_string(), "0101010101010101");
        let mut trace = Trace::new(TraceId::new([2; 16]).unwrap());
        assert_eq!(
            trace.insert(span(1, None, "root")),
            Err(ModelError::TraceMismatch)
        );
    }
}
