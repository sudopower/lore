//! OTLP/HTTP trace ingest: decoding a request body and turning its spans into rows.
//!
//! `decode` accepts protobuf or JSON, with optional gzip. `to_rows` flattens the request into one
//! `SpanRow` per span, with `AnyValue` attributes mapped to plain JSON.

use std::io::Read;
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chrono::{DateTime, SecondsFormat, Utc};
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::any_value::Value as AnyKind;
use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue};
use opentelemetry_proto::tonic::trace::v1::span::{Event, SpanKind};
use opentelemetry_proto::tonic::trace::v1::status::StatusCode;
use opentelemetry_proto::tonic::trace::v1::{Span, Status};
use prost::Message;
use serde_json::{Map, Number, Value};
use sha2::{Digest, Sha256};

use crate::error::AppError;

/// A request may expand to at most this many bytes when gunzipped.
pub const MAX_DECOMPRESSED_BYTES: u64 = 64 * 1024 * 1024;

/// The rows of one request may hold at most this many bytes of resource, attributes and events
/// (the resource counted once per span). Without it a small body with a large resource and many
/// spans multiplies the resource far beyond the body.
pub const MAX_ROW_BYTES: u64 = 64 * 1024 * 1024;

/// A session id longer than this is cut and gets a hash of the whole id, so it still fits a
/// btree key.
pub const MAX_SESSION_ID_BYTES: usize = 512;

/// Strings longer than this are cut.
pub const MAX_STRING_BYTES: usize = 64 * 1024;

/// Appended to a string that was cut.
pub const TRUNCATED_SUFFIX: &str = "\u{2026}[truncated]";

pub const PROTOBUF: &str = "application/x-protobuf";
pub const JSON: &str = "application/json";

/// One stored span.
#[derive(Debug, Clone, PartialEq)]
pub struct SpanRow {
    pub trace_id: String,
    pub span_id: String,
    pub parent_span_id: Option<String>,
    pub session_id: String,
    pub name: String,
    pub kind: String,
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    pub status_code: String,
    pub status_message: Option<String>,
    pub resource: Arc<Value>,
    pub attributes: Value,
    pub events: Value,
}

/// The media type of a `Content-Type` header: lowercase, without parameters.
pub fn media_type(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

/// Decodes an OTLP/HTTP trace export request.
pub fn decode(
    content_type: &str,
    content_encoding: Option<&str>,
    body: &[u8],
) -> Result<ExportTraceServiceRequest, AppError> {
    let media = media_type(content_type);
    if media != PROTOBUF && media != JSON {
        return Err(AppError::Unsupported(format!(
            "unsupported content type {content_type:?}: use {PROTOBUF} or {JSON}"
        )));
    }
    let gzip = match content_encoding.map(|e| e.trim().to_ascii_lowercase()) {
        None => false,
        Some(e) if e.is_empty() || e == "identity" => false,
        Some(e) if e == "gzip" => true,
        Some(e) => {
            return Err(AppError::Unsupported(format!(
                "unsupported content encoding {e:?}: use gzip or none"
            )));
        }
    };
    let unzipped;
    let bytes: &[u8] = if gzip {
        unzipped = gunzip(body)?;
        &unzipped
    } else {
        body
    };
    if media == PROTOBUF {
        ExportTraceServiceRequest::decode(bytes)
            .map_err(|e| AppError::BadRequest(format!("invalid protobuf body: {e}")))
    } else {
        serde_json::from_slice(bytes)
            .map_err(|e| AppError::BadRequest(format!("invalid OTLP JSON body: {e}")))
    }
}

/// Gunzips `body`, reading at most one byte more than the limit so a gzip bomb stops early.
fn gunzip(body: &[u8]) -> Result<Vec<u8>, AppError> {
    let mut out = Vec::new();
    flate2::read::MultiGzDecoder::new(body)
        .take(MAX_DECOMPRESSED_BYTES + 1)
        .read_to_end(&mut out)
        .map_err(|e| AppError::BadRequest(format!("invalid gzip body: {e}")))?;
    if out.len() as u64 > MAX_DECOMPRESSED_BYTES {
        return Err(AppError::PayloadTooLarge);
    }
    Ok(out)
}

/// What `to_rows` made of a request.
#[derive(Debug, PartialEq)]
pub struct Converted {
    pub rows: Vec<SpanRow>,
    /// Spans left out: an id that is missing or not 16 bytes (trace) or 8 bytes (span, parent).
    pub rejected: usize,
}

/// One row per valid span. A span whose trace id is not 16 bytes, whose span id is not 8 bytes, or
/// whose parent id is neither empty nor 8 bytes cannot be keyed, so it is counted in `rejected`.
/// More than `MAX_ROW_BYTES` of row data in one request is `PayloadTooLarge`.
pub fn to_rows(req: &ExportTraceServiceRequest) -> Result<Converted, AppError> {
    let mut rows = Vec::new();
    let mut rejected = 0;
    let mut total: u64 = 0;
    for rs in &req.resource_spans {
        let resource = Arc::new(
            rs.resource
                .as_ref()
                .map(|r| attributes_json(&r.attributes))
                .unwrap_or_else(|| Value::Object(Map::new())),
        );
        let resource_bytes = json_len(&resource);
        for ss in &rs.scope_spans {
            for span in &ss.spans {
                let valid = span.trace_id.len() == 16
                    && span.span_id.len() == 8
                    && matches!(span.parent_span_id.len(), 0 | 8);
                if !valid {
                    rejected += 1;
                    continue;
                }
                let row = span_row(span, &resource);
                total += resource_bytes + json_len(&row.attributes) + json_len(&row.events);
                if total > MAX_ROW_BYTES {
                    return Err(AppError::PayloadTooLarge);
                }
                rows.push(row);
            }
        }
    }
    Ok(Converted { rows, rejected })
}

fn json_len(value: &Value) -> u64 {
    serde_json::to_vec(value).map_or(0, |b| b.len() as u64)
}

fn span_row(span: &Span, resource: &Arc<Value>) -> SpanRow {
    let trace_id = hex::encode(&span.trace_id);
    let attributes = attributes_json(&span.attributes);
    let session_id = session_id(&attributes, resource, &trace_id);
    let (status_code, status_message) = status_parts(span.status.as_ref());
    SpanRow {
        trace_id,
        span_id: hex::encode(&span.span_id),
        parent_span_id: if span.parent_span_id.is_empty() {
            None
        } else {
            Some(hex::encode(&span.parent_span_id))
        },
        session_id,
        name: clean_string(&span.name),
        kind: kind_name(span.kind),
        start_time: nanos_to_time(span.start_time_unix_nano),
        end_time: nanos_to_time(span.end_time_unix_nano),
        status_code,
        status_message,
        resource: Arc::clone(resource),
        attributes,
        events: Value::Array(span.events.iter().map(event_json).collect()),
    }
}

/// The first non-empty of: span `session.id`, span `gen_ai.conversation.id`, resource
/// `session.id`, then the trace id.
fn session_id(span_attributes: &Value, resource: &Value, trace_id: &str) -> String {
    let non_empty = |attrs: &Value, key: &str| -> Option<String> {
        attrs
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let id = non_empty(span_attributes, "session.id")
        .or_else(|| non_empty(span_attributes, "gen_ai.conversation.id"))
        .or_else(|| non_empty(resource, "session.id"))
        .unwrap_or_else(|| trace_id.to_string());
    bound_session_id(id)
}

/// A session id over `MAX_SESSION_ID_BYTES` becomes its first part, `~`, and 16 hex characters of
/// the SHA-256 of the whole id, so two long ids that share a start stay apart.
fn bound_session_id(id: String) -> String {
    if id.len() <= MAX_SESSION_ID_BYTES {
        return id;
    }
    let mut cut = MAX_SESSION_ID_BYTES - 17;
    while !id.is_char_boundary(cut) {
        cut -= 1;
    }
    let digest = hex::encode(Sha256::digest(id.as_bytes()));
    format!("{}~{}", &id[..cut], &digest[..16])
}

/// The enum name without its `SPAN_KIND_` prefix, for example `INTERNAL`.
fn kind_name(kind: i32) -> String {
    match SpanKind::try_from(kind) {
        Ok(k) => k.as_str_name().trim_start_matches("SPAN_KIND_").to_string(),
        Err(_) => "UNSPECIFIED".to_string(),
    }
}

fn status_parts(status: Option<&Status>) -> (String, Option<String>) {
    let Some(status) = status else {
        return ("UNSET".to_string(), None);
    };
    let code = match StatusCode::try_from(status.code) {
        Ok(StatusCode::Ok) => "OK",
        Ok(StatusCode::Error) => "ERROR",
        _ => "UNSET",
    };
    let message = if status.message.is_empty() {
        None
    } else {
        Some(clean_string(&status.message))
    };
    (code.to_string(), message)
}

fn nanos_to_time(nanos: u64) -> DateTime<Utc> {
    DateTime::from_timestamp_nanos(i64::try_from(nanos).unwrap_or(i64::MAX))
}

fn event_json(event: &Event) -> Value {
    let mut obj = Map::new();
    obj.insert("name".to_string(), Value::String(clean_string(&event.name)));
    obj.insert(
        "time".to_string(),
        Value::String(
            nanos_to_time(event.time_unix_nano).to_rfc3339_opts(SecondsFormat::Nanos, true),
        ),
    );
    obj.insert("attributes".to_string(), attributes_json(&event.attributes));
    Value::Object(obj)
}

/// A list of key/value pairs as a JSON object. A repeated key keeps its last value.
pub fn attributes_json(attrs: &[KeyValue]) -> Value {
    let mut obj = Map::new();
    for kv in attrs {
        let value = kv.value.as_ref().map(any_value_json).unwrap_or(Value::Null);
        obj.insert(clean_string(&kv.key), value);
    }
    Value::Object(obj)
}

/// An OTLP `AnyValue` as plain JSON: strings, bools, numbers, arrays, objects, and bytes as a
/// base64 string. An empty value is `null`.
pub fn any_value_json(value: &AnyValue) -> Value {
    match &value.value {
        // A string table index only means something in the profiles signal.
        None | Some(AnyKind::StringValueStrindex(_)) => Value::Null,
        Some(AnyKind::StringValue(s)) => Value::String(clean_string(s)),
        Some(AnyKind::BoolValue(b)) => Value::Bool(*b),
        Some(AnyKind::IntValue(i)) => Value::Number(Number::from(*i)),
        Some(AnyKind::DoubleValue(d)) => Number::from_f64(*d).map_or(Value::Null, Value::Number),
        Some(AnyKind::ArrayValue(a)) => Value::Array(a.values.iter().map(any_value_json).collect()),
        Some(AnyKind::KvlistValue(l)) => attributes_json(&l.values),
        Some(AnyKind::BytesValue(b)) => Value::String(clean_string(&STANDARD.encode(b))),
    }
}

/// Prepares a string for storage: a NUL (which Postgres rejects in text and jsonb) becomes
/// U+FFFD, and anything over `MAX_STRING_BYTES` is cut on a character boundary and gets
/// `TRUNCATED_SUFFIX`.
pub fn clean_string(s: &str) -> String {
    let mut out = if s.contains('\0') {
        s.replace('\0', "\u{FFFD}")
    } else {
        s.to_string()
    };
    if out.len() > MAX_STRING_BYTES {
        let mut cut = MAX_STRING_BYTES;
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
        out.push_str(TRUNCATED_SUFFIX);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry_proto::tonic::common::v1::{ArrayValue, KeyValueList};
    use opentelemetry_proto::tonic::resource::v1::Resource;
    use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans};
    use serde_json::json;
    use std::io::Write;

    fn kv(key: &str, value: AnyKind) -> KeyValue {
        KeyValue {
            key: key.to_string(),
            value: Some(AnyValue { value: Some(value) }),
            ..Default::default()
        }
    }

    fn s(key: &str, value: &str) -> KeyValue {
        kv(key, AnyKind::StringValue(value.to_string()))
    }

    fn request(resource: Vec<KeyValue>, span: Span) -> ExportTraceServiceRequest {
        ExportTraceServiceRequest {
            resource_spans: vec![ResourceSpans {
                resource: Some(Resource {
                    attributes: resource,
                    ..Default::default()
                }),
                scope_spans: vec![ScopeSpans {
                    spans: vec![span],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        }
    }

    fn span(attrs: Vec<KeyValue>) -> Span {
        Span {
            trace_id: vec![0xAB; 16],
            span_id: vec![1, 2, 3, 4, 5, 6, 7, 8],
            name: "s".to_string(),
            attributes: attrs,
            ..Default::default()
        }
    }

    fn rows_of(req: &ExportTraceServiceRequest) -> Vec<SpanRow> {
        to_rows(req).unwrap().rows
    }

    fn session_of(resource: Vec<KeyValue>, attrs: Vec<KeyValue>) -> String {
        rows_of(&request(resource, span(attrs)))
            .remove(0)
            .session_id
    }

    #[test]
    fn session_id_fallback_order() {
        let trace = "ab".repeat(16);
        // 1. the span's session.id wins over everything.
        assert_eq!(
            session_of(
                vec![s("session.id", "res")],
                vec![s("gen_ai.conversation.id", "conv"), s("session.id", "span")]
            ),
            "span"
        );
        // 2. then gen_ai.conversation.id.
        assert_eq!(
            session_of(
                vec![s("session.id", "res")],
                vec![s("gen_ai.conversation.id", "conv")]
            ),
            "conv"
        );
        // 3. then the resource's session.id.
        assert_eq!(session_of(vec![s("session.id", "res")], vec![]), "res");
        // 4. then the trace id.
        assert_eq!(session_of(vec![], vec![]), trace);
        // An empty value falls through to the next candidate.
        assert_eq!(
            session_of(
                vec![s("session.id", "res")],
                vec![s("session.id", ""), s("gen_ai.conversation.id", "")]
            ),
            "res"
        );
    }

    #[test]
    fn long_strings_are_cut_with_a_suffix() {
        let exact = "a".repeat(MAX_STRING_BYTES);
        assert_eq!(clean_string(&exact), exact);

        let long = "a".repeat(MAX_STRING_BYTES + 1);
        let cut = clean_string(&long);
        assert_eq!(cut.len(), MAX_STRING_BYTES + TRUNCATED_SUFFIX.len());
        assert!(cut.ends_with("\u{2026}[truncated]"));
        assert!(cut.starts_with(&"a".repeat(MAX_STRING_BYTES)));

        // The cut falls inside a 3 byte character: it backs off to the boundary.
        let wide = "\u{20AC}".repeat(MAX_STRING_BYTES);
        let cut = clean_string(&wide);
        assert!(cut.ends_with(TRUNCATED_SUFFIX));
        let body = cut.strip_suffix(TRUNCATED_SUFFIX).unwrap();
        assert!(body.len() <= MAX_STRING_BYTES);
        assert_eq!(body.len() % 3, 0);

        // Attribute values go through the same cut.
        let rows = rows_of(&request(vec![], span(vec![s("input.value", &long)])));
        let v = rows[0].attributes["input.value"].as_str().unwrap();
        assert!(v.ends_with(TRUNCATED_SUFFIX));
    }

    #[test]
    fn nul_characters_are_replaced() {
        assert_eq!(clean_string("a\0b"), "a\u{FFFD}b");
    }

    #[test]
    fn any_values_map_to_json() {
        let any = |v: AnyKind| AnyValue { value: Some(v) };
        assert_eq!(
            any_value_json(&any(AnyKind::StringValue("x".into()))),
            json!("x")
        );
        assert_eq!(any_value_json(&any(AnyKind::BoolValue(true))), json!(true));
        assert_eq!(any_value_json(&any(AnyKind::IntValue(1200))), json!(1200));
        assert_eq!(any_value_json(&any(AnyKind::IntValue(-3))), json!(-3));
        assert_eq!(any_value_json(&any(AnyKind::DoubleValue(0.5))), json!(0.5));
        assert_eq!(
            any_value_json(&any(AnyKind::DoubleValue(f64::NAN))),
            Value::Null
        );
        assert_eq!(
            any_value_json(&any(AnyKind::BytesValue(vec![0, 1, 2, 255]))),
            json!("AAEC/w==")
        );
        assert_eq!(any_value_json(&AnyValue { value: None }), Value::Null);
        let array = AnyKind::ArrayValue(ArrayValue {
            values: vec![
                any(AnyKind::IntValue(1)),
                any(AnyKind::StringValue("two".into())),
            ],
        });
        assert_eq!(any_value_json(&any(array)), json!([1, "two"]));
        let list = AnyKind::KvlistValue(KeyValueList {
            values: vec![s("a", "b"), kv("n", AnyKind::IntValue(7))],
        });
        assert_eq!(any_value_json(&any(list)), json!({"a": "b", "n": 7}));
    }

    #[test]
    fn rows_have_hex_ids_names_and_times() {
        let mut sp = span(vec![]);
        sp.parent_span_id = vec![];
        sp.kind = SpanKind::Internal as i32;
        sp.start_time_unix_nano = 1_790_000_000_000_000_000;
        sp.end_time_unix_nano = 1_790_000_300_000_000_000;
        sp.status = Some(Status {
            message: "bad".into(),
            code: StatusCode::Error as i32,
        });
        sp.events = vec![Event {
            time_unix_nano: 1_790_000_001_000_000_000,
            name: "exception".into(),
            attributes: vec![s("exception.type", "ToolError")],
            ..Default::default()
        }];
        let row = rows_of(&request(vec![s("service.name", "claude-code")], sp)).remove(0);
        assert_eq!(row.trace_id, "ab".repeat(16));
        assert_eq!(row.span_id, "0102030405060708");
        assert_eq!(row.parent_span_id, None);
        assert_eq!(row.kind, "INTERNAL");
        assert_eq!(row.status_code, "ERROR");
        assert_eq!(row.status_message.as_deref(), Some("bad"));
        assert_eq!((row.end_time - row.start_time).num_seconds(), 300);
        assert_eq!(*row.resource, json!({"service.name": "claude-code"}));
        assert_eq!(
            row.events,
            json!([{
                "name": "exception",
                "time": "2026-09-21T14:13:21.000000000Z",
                "attributes": {"exception.type": "ToolError"}
            }])
        );
    }

    #[test]
    fn spans_without_valid_ids_are_rejected_and_counted() {
        let bad = |edit: fn(&mut Span)| {
            let mut sp = span(vec![]);
            edit(&mut sp);
            to_rows(&request(vec![], sp)).unwrap()
        };
        for converted in [
            bad(|sp| sp.span_id = vec![]),
            bad(|sp| sp.trace_id = vec![]),
            bad(|sp| sp.trace_id = vec![1; 8000]),
            bad(|sp| sp.span_id = vec![1; 9]),
            bad(|sp| sp.parent_span_id = vec![1; 3]),
        ] {
            assert!(converted.rows.is_empty());
            assert_eq!(converted.rejected, 1);
        }
        let ok = bad(|sp| sp.parent_span_id = vec![1; 8]);
        assert_eq!((ok.rows.len(), ok.rejected), (1, 0));
    }

    #[test]
    fn a_long_session_id_is_bounded_and_stays_distinct() {
        let a = format!("{}a", "x".repeat(600));
        let b = format!("{}b", "x".repeat(600));
        let (a, b) = (
            session_of(vec![], vec![s("session.id", &a)]),
            session_of(vec![], vec![s("session.id", &b)]),
        );
        assert!(a.len() <= MAX_SESSION_ID_BYTES && b.len() <= MAX_SESSION_ID_BYTES);
        assert_ne!(a, b);
        let short = "y".repeat(MAX_SESSION_ID_BYTES);
        assert_eq!(session_of(vec![], vec![s("session.id", &short)]), short);
    }

    #[test]
    fn keys_are_cleaned_like_values() {
        let long_key = "k".repeat(MAX_STRING_BYTES + 1);
        let attrs = attributes_json(&[s(&long_key, "v"), s("a\0b", "v")]);
        let keys: Vec<&String> = attrs.as_object().unwrap().keys().collect();
        assert!(
            keys.iter()
                .all(|k| k.len() <= MAX_STRING_BYTES + TRUNCATED_SUFFIX.len())
        );
        assert!(attrs.get("a\u{FFFD}b").is_some());
        assert!(keys.iter().any(|k| k.ends_with(TRUNCATED_SUFFIX)));
    }

    #[test]
    fn row_data_is_bounded_per_request() {
        // One 1 MiB resource shared by 70 spans is 70 MiB of row data, past the 64 MiB cap.
        let big = "r".repeat(MAX_STRING_BYTES);
        let resource: Vec<KeyValue> = (0..16).map(|i| s(&format!("k{i}"), &big)).collect();
        let spans = |n: usize| -> ExportTraceServiceRequest {
            let mut req = request(resource.clone(), span(vec![]));
            let one = req.resource_spans[0].scope_spans[0].spans[0].clone();
            req.resource_spans[0].scope_spans[0].spans = (0..n)
                .map(|i| Span {
                    span_id: (i as u64 + 1).to_be_bytes().to_vec(),
                    ..one.clone()
                })
                .collect();
            req
        };
        assert_eq!(to_rows(&spans(60)).unwrap().rows.len(), 60);
        assert!(matches!(
            to_rows(&spans(70)),
            Err(AppError::PayloadTooLarge)
        ));
    }

    const HEX_JSON: &str = r#"{"resourceSpans":[{"scopeSpans":[{"spans":[{
        "traceId":"5b8efff798038103d269b633813fc60c","spanId":"0102030405060708",
        "parentSpanId":"a1a2a3a4a5a6a7a8","name":"x","kind":1,
        "startTimeUnixNano":"1790000000000000000","endTimeUnixNano":"1790000001000000000",
        "attributes":[{"key":"n","value":{"intValue":"1200"}}],
        "status":{"code":2,"message":"m"}}]}]}]}"#;

    #[test]
    fn json_ids_are_hex_and_enums_are_numeric() {
        let req = decode("application/json", None, HEX_JSON.as_bytes()).unwrap();
        let row = rows_of(&req).remove(0);
        assert_eq!(row.trace_id, "5b8efff798038103d269b633813fc60c");
        assert_eq!(row.span_id, "0102030405060708");
        assert_eq!(row.parent_span_id.as_deref(), Some("a1a2a3a4a5a6a7a8"));
        assert_eq!(row.kind, "INTERNAL");
        assert_eq!(row.status_code, "ERROR");
        assert_eq!(row.attributes["n"], json!(1200));
        // The session falls back to the trace id.
        assert_eq!(row.session_id, row.trace_id);
    }

    #[test]
    fn protobuf_round_trips_and_gzip_works() {
        let req = decode("application/json", None, HEX_JSON.as_bytes()).unwrap();
        let bytes = req.encode_to_vec();
        let back = decode("application/x-protobuf", None, &bytes).unwrap();
        assert_eq!(rows_of(&back), rows_of(&req));

        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&bytes).unwrap();
        let zipped = gz.finish().unwrap();
        let back = decode(
            "application/x-protobuf; charset=utf-8",
            Some("gzip"),
            &zipped,
        )
        .unwrap();
        assert_eq!(rows_of(&back), rows_of(&req));
    }

    #[test]
    fn bad_inputs_are_client_errors() {
        let status =
            |r: Result<ExportTraceServiceRequest, AppError>| r.unwrap_err().status().as_u16();
        assert_eq!(status(decode("text/plain", None, b"{}")), 415);
        assert_eq!(status(decode("application/json", Some("br"), b"{}")), 415);
        assert_eq!(status(decode("application/json", None, b"not json")), 400);
        assert_eq!(
            status(decode("application/x-protobuf", None, &[0xFF, 0xFF])),
            400
        );
        assert_eq!(
            status(decode("application/json", Some("gzip"), b"plain")),
            400
        );
        // Non-hex ids are rejected.
        let bad = HEX_JSON.replace("5b8efff798038103d269b633813fc60c", "zz");
        assert_eq!(
            status(decode("application/json", None, bad.as_bytes())),
            400
        );
    }

    #[test]
    fn gzip_bombs_are_rejected() {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let zeros = vec![0u8; 1024 * 1024];
        for _ in 0..65 {
            gz.write_all(&zeros).unwrap();
        }
        let zipped = gz.finish().unwrap();
        let err = decode("application/x-protobuf", Some("gzip"), &zipped).unwrap_err();
        assert_eq!(err.status().as_u16(), 413);
    }
}
