/// Tests for bedrock.rs (Phase 4 mandatory gate).
use syl_rs::bedrock::{
    bedrock_model_id, hmac_sha256, parse_bedrock_stream, parse_eventstream_headers,
    sha256_hex, sign_bedrock_request,
};

// ---------------------------------------------------------------------------
// sha256_hex
// ---------------------------------------------------------------------------

#[test]
fn sha256_hex_empty_string() {
    // SHA-256("") = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
    assert_eq!(
        sha256_hex(""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

#[test]
fn sha256_hex_nonempty_is_64_hex_chars() {
    let h = sha256_hex("hello world");
    assert_eq!(h.len(), 64, "SHA-256 hex should be 64 chars");
    assert!(h.chars().all(|c| c.is_ascii_hexdigit()), "output must be lowercase hex");
}

#[test]
fn sha256_hex_deterministic() {
    // Same input → same output
    assert_eq!(sha256_hex("test input"), sha256_hex("test input"));
}

#[test]
fn sha256_hex_different_inputs_differ() {
    assert_ne!(sha256_hex("abc"), sha256_hex("abd"));
}

// ---------------------------------------------------------------------------
// hmac_sha256
// ---------------------------------------------------------------------------

#[test]
fn hmac_sha256_known_vector() {
    // RFC 4231 Test Case 1:
    // Key  = 0x0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b (20 bytes)
    // Data = "Hi There"
    // HMAC-SHA256 = b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7
    let key = vec![0x0bu8; 20];
    let result = hmac_sha256(&key, "Hi There");
    assert_eq!(result.len(), 32, "HMAC-SHA256 output should be 32 bytes");
    let hex = hex::encode(&result);
    assert_eq!(hex, "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7");
}

#[test]
fn hmac_sha256_returns_32_bytes() {
    let result = hmac_sha256(b"key", "message");
    assert_eq!(result.len(), 32);
}

#[test]
fn hmac_sha256_different_keys_differ() {
    let a = hmac_sha256(b"key1", "data");
    let b = hmac_sha256(b"key2", "data");
    assert_ne!(a, b);
}

// ---------------------------------------------------------------------------
// bedrock_model_id
// ---------------------------------------------------------------------------

#[test]
fn bedrock_model_id_known_mapping() {
    assert!(bedrock_model_id("claude-sonnet-4-6").contains("anthropic.claude-sonnet"));
    assert!(bedrock_model_id("claude-haiku-4-5").contains("anthropic.claude-haiku"));
    assert!(bedrock_model_id("claude-opus-4-6").contains("anthropic.claude-opus"));
}

#[test]
fn bedrock_model_id_unknown_passthrough() {
    assert_eq!(bedrock_model_id("some-custom-model"), "some-custom-model");
}

// ---------------------------------------------------------------------------
// sign_bedrock_request — structure tests (no network)
// ---------------------------------------------------------------------------

fn make_headers(with_session_token: bool) -> std::collections::HashMap<String, String> {
    let tok = if with_session_token { Some("test-session-token") } else { None };
    sign_bedrock_request(
        "POST",
        "/model/anthropic.claude-sonnet-4-6-20250514-v1:0/invoke-with-response-stream",
        r#"{"messages":[]}"#,
        "us-east-1",
        "AKIAIOSFODNN7EXAMPLE",
        "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
        tok,
    )
}

#[test]
fn sign_bedrock_request_has_authorization_header() {
    let headers = make_headers(false);
    assert!(headers.contains_key("authorization"), "must have authorization header");
}

#[test]
fn sign_bedrock_request_authorization_starts_with_algorithm() {
    let headers = make_headers(false);
    let auth = &headers["authorization"];
    assert!(
        auth.starts_with("AWS4-HMAC-SHA256 "),
        "Authorization should start with algorithm: {}", auth
    );
}

#[test]
fn sign_bedrock_request_authorization_has_credential_field() {
    let headers = make_headers(false);
    let auth = &headers["authorization"];
    assert!(auth.contains("Credential="), "Authorization must contain Credential=: {}", auth);
    assert!(auth.contains("AKIAIOSFODNN7EXAMPLE"), "Credential must include access key");
}

#[test]
fn sign_bedrock_request_authorization_has_signed_headers() {
    let headers = make_headers(false);
    let auth = &headers["authorization"];
    assert!(auth.contains("SignedHeaders="), "Authorization must contain SignedHeaders=");
}

#[test]
fn sign_bedrock_request_authorization_has_signature() {
    let headers = make_headers(false);
    let auth = &headers["authorization"];
    assert!(auth.contains("Signature="), "Authorization must contain Signature=");
    // Signature is a hex string (64 hex chars)
    let sig_part = auth.split("Signature=").nth(1).unwrap_or("");
    assert_eq!(sig_part.len(), 64, "Signature hex must be 64 chars");
    assert!(sig_part.chars().all(|c| c.is_ascii_hexdigit()), "Signature must be hex");
}

#[test]
fn sign_bedrock_request_has_required_headers() {
    let headers = make_headers(false);
    assert!(headers.contains_key("x-amz-date"), "must have x-amz-date");
    assert!(headers.contains_key("host"), "must have host");
    assert!(headers.contains_key("content-type"), "must have content-type");
    assert!(headers.contains_key("x-amz-content-sha256"), "must have x-amz-content-sha256");
}

#[test]
fn sign_bedrock_request_no_session_token_when_none() {
    let headers = make_headers(false);
    assert!(
        !headers.contains_key("x-amz-security-token"),
        "must not have x-amz-security-token when none provided"
    );
}

#[test]
fn sign_bedrock_request_session_token_present_when_provided() {
    let headers = make_headers(true);
    assert!(
        headers.contains_key("x-amz-security-token"),
        "must include x-amz-security-token when provided"
    );
    assert_eq!(headers["x-amz-security-token"], "test-session-token");
}

#[test]
fn sign_bedrock_request_host_contains_region() {
    let headers = make_headers(false);
    assert!(
        headers["host"].contains("us-east-1"),
        "host must contain region: {}", headers["host"]
    );
    assert!(
        headers["host"].contains("amazonaws.com"),
        "host must contain amazonaws.com"
    );
}

#[test]
fn sign_bedrock_request_signed_headers_includes_token_when_present() {
    let headers = make_headers(true);
    let auth = &headers["authorization"];
    let signed = auth
        .split("SignedHeaders=")
        .nth(1)
        .and_then(|s| s.split(',').next())
        .unwrap_or("");
    assert!(
        signed.contains("x-amz-security-token"),
        "SignedHeaders must include security token when present: {}", signed
    );
}

// ---------------------------------------------------------------------------
// parse_eventstream_headers
// ---------------------------------------------------------------------------

fn build_header_bytes(name: &str, value: &str) -> Vec<u8> {
    // Format: [name_len: u8][name: bytes][val_type: 7][val_len: u16 BE][val: bytes]
    let mut bytes = Vec::new();
    bytes.push(name.len() as u8);
    bytes.extend_from_slice(name.as_bytes());
    bytes.push(7u8); // string type
    let val_len = value.len() as u16;
    bytes.extend_from_slice(&val_len.to_be_bytes());
    bytes.extend_from_slice(value.as_bytes());
    bytes
}

#[test]
fn parse_eventstream_headers_single_string_header() {
    let bytes = build_header_bytes(":event-type", "chunk");
    let headers = parse_eventstream_headers(&bytes);
    assert_eq!(headers.get(":event-type").map(|s| s.as_str()), Some("chunk"));
}

#[test]
fn parse_eventstream_headers_multiple_headers() {
    let mut bytes = build_header_bytes(":event-type", "chunk");
    bytes.extend(build_header_bytes(":content-type", "application/json"));
    let headers = parse_eventstream_headers(&bytes);
    assert_eq!(headers.get(":event-type").map(|s| s.as_str()), Some("chunk"));
    assert_eq!(headers.get(":content-type").map(|s| s.as_str()), Some("application/json"));
}

#[test]
fn parse_eventstream_headers_empty_input() {
    let headers = parse_eventstream_headers(&[]);
    assert!(headers.is_empty());
}

#[test]
fn parse_eventstream_headers_truncated_graceful() {
    // Truncated input should not panic — just return what was parsed
    let bytes = vec![5u8, b'h', b'e', b'l', b'l']; // name_len=5 but only 4 name bytes
    let headers = parse_eventstream_headers(&bytes);
    assert!(headers.is_empty(), "truncated input should yield empty map without panic");
}

// ---------------------------------------------------------------------------
// parse_bedrock_stream
// ---------------------------------------------------------------------------

fn build_eventstream_frame(event_type: &str, payload_bytes: &[u8]) -> Vec<u8> {
    // Build header bytes
    let headers_bytes = build_header_bytes(":event-type", event_type);
    let headers_len  = headers_bytes.len() as u32;
    let payload_len  = payload_bytes.len() as u32;
    let total_len    = 12 + headers_len + payload_len + 4; // prelude + headers + payload + msg_crc

    let mut frame = Vec::new();
    frame.extend_from_slice(&total_len.to_be_bytes());
    frame.extend_from_slice(&headers_len.to_be_bytes());
    frame.extend_from_slice(&0u32.to_be_bytes()); // prelude CRC (ignored)
    frame.extend_from_slice(&headers_bytes);
    frame.extend_from_slice(payload_bytes);
    frame.extend_from_slice(&0u32.to_be_bytes()); // message CRC (ignored)
    frame
}

fn build_chunk_frame(inner_json: &str) -> Vec<u8> {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(inner_json.as_bytes());
    let payload = format!(r#"{{"bytes":"{}"}}"#, b64);
    build_eventstream_frame("chunk", payload.as_bytes())
}

#[test]
fn parse_bedrock_stream_chunk_event_decoded() {
    let inner = r#"{"type":"content_block_delta","delta":{"text":"hello"}}"#;
    let frame = build_chunk_frame(inner);

    let mut events: Vec<serde_json::Value> = Vec::new();
    parse_bedrock_stream(std::io::Cursor::new(frame), |ev| events.push(ev));

    assert_eq!(events.len(), 1, "should decode exactly one chunk event");
    assert_eq!(events[0]["type"].as_str(), Some("content_block_delta"));
    assert_eq!(events[0]["delta"]["text"].as_str(), Some("hello"));
}

#[test]
fn parse_bedrock_stream_non_chunk_events_ignored() {
    // Build a non-chunk event
    let frame = build_eventstream_frame("message-start", b"{}");
    let mut events: Vec<serde_json::Value> = Vec::new();
    parse_bedrock_stream(std::io::Cursor::new(frame), |ev| events.push(ev));
    assert!(events.is_empty(), "non-chunk events should not call on_event");
}

#[test]
fn parse_bedrock_stream_multiple_frames() {
    let frame1 = build_chunk_frame(r#"{"type":"content_block_delta","delta":{"text":"a"}}"#);
    let frame2 = build_chunk_frame(r#"{"type":"content_block_delta","delta":{"text":"b"}}"#);
    let mut data = frame1;
    data.extend(frame2);

    let mut events: Vec<serde_json::Value> = Vec::new();
    parse_bedrock_stream(std::io::Cursor::new(data), |ev| events.push(ev));

    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["delta"]["text"].as_str(), Some("a"));
    assert_eq!(events[1]["delta"]["text"].as_str(), Some("b"));
}

#[test]
fn parse_bedrock_stream_empty_input_no_panic() {
    let mut events: Vec<serde_json::Value> = Vec::new();
    parse_bedrock_stream(std::io::Cursor::new(vec![]), |ev| events.push(ev));
    assert!(events.is_empty());
}
