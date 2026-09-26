//! The shared vectors in `spec/vectors/messages.json`: every message must
//! parse into its typed form and serialize back to the same JSON.

use std::path::PathBuf;

use moil_proto::http::{
    BundleResponse, BundlesResponse, ErrorBody, InfoResponse, PairRequest, PairResponse,
    TokenRequest, TokenResponse,
};
use moil_proto::{JobContext, MachineMessage, ScriptEvent, ServiceMessage};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

fn vectors() -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../spec/vectors/messages.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// JSON equality where numbers compare by value, so `1` equals `1.0`.
fn same_json(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(x, y)| same_json(x, y))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).is_some_and(|w| same_json(v, w)))
        }
        _ => a == b,
    }
}

fn round_trip<T: DeserializeOwned + Serialize + std::fmt::Debug>(
    name: &str,
    original: &Value,
) -> T {
    let parsed: T = serde_json::from_value(original.clone())
        .unwrap_or_else(|e| panic!("{name}: failed to parse: {e}\n{original:#}"));
    let back = serde_json::to_value(&parsed).unwrap();
    assert!(
        same_json(original, &back),
        "{name}: didn't round-trip\noriginal: {original:#}\nparsed: {parsed:#?}\nback: {back:#}"
    );
    parsed
}

#[test]
fn machine_messages_round_trip() {
    let v = vectors();
    let cases = v["machine_to_service"].as_array().unwrap();
    assert!(!cases.is_empty());
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let msg: MachineMessage = round_trip(name, &case["message"]);
        assert_ne!(msg, MachineMessage::Unknown, "{name}");
    }
}

#[test]
fn service_messages_round_trip() {
    let v = vectors();
    let cases = v["service_to_machine"].as_array().unwrap();
    assert!(!cases.is_empty());
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let msg: ServiceMessage = round_trip(name, &case["message"]);
        assert_ne!(msg, ServiceMessage::Unknown, "{name}");
    }
}

#[test]
fn http_bodies_round_trip() {
    let v = vectors();
    let http = &v["http"];
    round_trip::<InfoResponse>("info_response", &http["info_response"]);
    round_trip::<PairRequest>("pair_request", &http["pair_request"]);
    round_trip::<PairResponse>("pair_response", &http["pair_response"]);
    round_trip::<TokenRequest>("token_request", &http["token_request"]);
    round_trip::<TokenResponse>("token_response", &http["token_response"]);
    round_trip::<BundlesResponse>("bundles_response", &http["bundles_response"]);
    for body in http["error_responses"].as_array().unwrap() {
        round_trip::<ErrorBody>("error_response", body);
    }
}

#[test]
fn script_events_round_trip_with_version() {
    let v = vectors();
    for original in v["script_events"].as_array().unwrap() {
        let parsed: ScriptEvent = serde_json::from_value(original.clone()).unwrap();
        let back = moil_proto::event::versioned::to_value(&parsed).unwrap();
        assert!(
            same_json(original, &back),
            "original: {original}\nback: {back}"
        );
    }
}

#[test]
fn job_context_round_trips() {
    let v = vectors();
    round_trip::<JobContext>("job_context", &v["job_context"]);
}

#[test]
fn unknown_message_types_are_ignorable() {
    let msg: ServiceMessage = serde_json::from_str(r#"{"type":"hint","anything":1}"#).unwrap();
    assert_eq!(msg, ServiceMessage::Unknown);
    let msg: MachineMessage = serde_json::from_str(r#"{"type":"telemetry"}"#).unwrap();
    assert_eq!(msg, MachineMessage::Unknown);
}

#[test]
fn unknown_fields_are_ignored() {
    let msg: ServiceMessage =
        serde_json::from_str(r#"{"type":"ack","job_id":"j_1","attempt":2,"future":{"x":1}}"#)
            .unwrap();
    let ServiceMessage::Ack(attempt) = msg else {
        panic!("{msg:?}")
    };
    assert_eq!(attempt.job_id, "j_1");
    assert_eq!(attempt.attempt, 2);
}

#[test]
fn invalid_identifiers_reject_the_message() {
    let bad = r#"{"type":"cancel","job_id":"../../etc","attempt":1}"#;
    assert!(serde_json::from_str::<ServiceMessage>(bad).is_err());
    let bad_output = r#"{"type":"assign","job_id":"j","attempt":1,
        "bundle_hash":"a00a65c166291e905da7546a67518a81cc27cb498f009c75b824f28a8adfffb2",
        "params":null,"outputs":{"../x":{"url":"https://e.com"}},"lease_ms":1}"#;
    assert!(serde_json::from_str::<ServiceMessage>(bad_output).is_err());
}

#[test]
fn bundle_response_parses() {
    let body = r#"{"hash":"a00a65c166291e905da7546a67518a81cc27cb498f009c75b824f28a8adfffb2",
        "files":{"job.py":"print(1)\r\n","job.py.lock":"version = 1\n","manifest.json":"{}"}}"#;
    let parsed: BundleResponse = serde_json::from_str(body).unwrap();
    assert_eq!(parsed.files["job.py"], "print(1)\r\n");
}
