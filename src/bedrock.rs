use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::Read;

type HmacSha256 = Hmac<Sha256>;

/// Map a friendly model name to its full Bedrock model ID.
///
/// Looks up the model in `CLAUDE_MODELS` (the single source of truth in
/// `config.rs`).  If the `bedrock_id` field is empty, or the friendly name
/// is not in the catalog at all, the input string is returned unchanged so
/// callers can pass raw Bedrock model IDs directly.
pub fn bedrock_model_id(model: &str) -> &str {
    crate::config::CLAUDE_MODELS
        .iter()
        .find(|m| m.id == model)
        .filter(|m| !m.bedrock_id.is_empty())
        .map(|m| m.bedrock_id)
        .unwrap_or(model)
}

/// Sign an AWS Bedrock management-plane GET request (no body, no content-type).
///
/// Differs from `sign_bedrock_request` in three ways:
/// 1. Uses the **management** host `bedrock.{region}` instead of the runtime
///    host `bedrock-runtime.{region}`.
/// 2. Carries no request body → payload hash = SHA-256 of the empty string.
/// 3. Accepts a pre-canonicalized `query_string` (parameters sorted
///    alphabetically, e.g. `"byOutputModality=TEXT&byProvider=Anthropic"`).
///
/// Returns the HTTP headers map to attach to the GET request.
pub fn sign_bedrock_get_request(
    path: &str,
    query_string: &str, // sorted & encoded, e.g. "byOutputModality=TEXT&byProvider=Anthropic"
    region: &str,
    access_key: &str,
    secret_key: &str,
    session_token: Option<&str>,
) -> HashMap<String, String> {
    use chrono::Utc;
    let now = Utc::now();
    let datetime = now.format("%Y%m%dT%H%M%SZ").to_string();
    let date = &datetime[..8];

    // Management-plane host (not bedrock-runtime)
    let host = format!("bedrock.{}.amazonaws.com", region);
    let payload_hash = sha256_hex(""); // empty-body hash

    let mut base_headers: std::collections::BTreeMap<String, String> = [
        ("host".to_string(), host.clone()),
        ("x-amz-content-sha256".to_string(), payload_hash.clone()),
        ("x-amz-date".to_string(), datetime.clone()),
    ]
    .into_iter()
    .collect();

    if let Some(tok) = session_token {
        base_headers.insert("x-amz-security-token".to_string(), tok.to_string());
    }

    let canonical_headers: String = base_headers
        .iter()
        .map(|(k, v)| format!("{}:{}\n", k, v))
        .collect();
    let signed_headers: String = base_headers.keys().cloned().collect::<Vec<_>>().join(";");

    // SigV4 canonical request for GET:
    //   Method\n URI\n QueryString\n Headers\n SignedHeaders\n BodyHash
    let canonical_req = format!(
        "GET\n{}\n{}\n{}\n{}\n{}",
        path, query_string, canonical_headers, signed_headers, payload_hash
    );

    let scope = format!("{}/{}/bedrock/aws4_request", date, region);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{}\n{}",
        datetime,
        scope,
        sha256_hex(&canonical_req)
    );

    let sig_key = {
        let k = hmac_sha256(format!("AWS4{}", secret_key).as_bytes(), date);
        let k = hmac_sha256(&k, region);
        let k = hmac_sha256(&k, "bedrock");
        hmac_sha256(&k, "aws4_request")
    };
    let signature = hex::encode(hmac_sha256(&sig_key, &string_to_sign));

    let auth = format!(
        "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
        access_key, scope, signed_headers, signature
    );

    let mut out: HashMap<String, String> = base_headers.into_iter().collect();
    out.insert("authorization".to_string(), auth);
    out
}

pub fn sha256_hex(s: &str) -> String {
    let hash = Sha256::digest(s.as_bytes());
    hex::encode(hash)
}

pub fn hmac_sha256(key: &[u8], data: &str) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC can accept any key size");
    mac.update(data.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

/// Sign a Bedrock request using AWS SigV4. Returns headers map.
pub fn sign_bedrock_request(
    method: &str,
    uri: &str,
    body: &str,
    region: &str,
    access_key: &str,
    secret_key: &str,
    session_token: Option<&str>,
) -> HashMap<String, String> {
    use chrono::Utc;
    let now = Utc::now();
    let datetime = now.format("%Y%m%dT%H%M%SZ").to_string();
    let date = &datetime[..8];

    let host = format!("bedrock-runtime.{}.amazonaws.com", region);
    let payload_hash = sha256_hex(body);

    let mut base_headers: std::collections::BTreeMap<String, String> = [
        ("content-type".to_string(), "application/json".to_string()),
        ("host".to_string(), host.clone()),
        ("x-amz-date".to_string(), datetime.clone()),
        ("x-amz-content-sha256".to_string(), payload_hash.clone()),
    ]
    .into_iter()
    .collect();

    if let Some(tok) = session_token {
        base_headers.insert("x-amz-security-token".to_string(), tok.to_string());
    }

    let canonical_headers: String = base_headers
        .iter()
        .map(|(k, v)| format!("{}:{}\n", k, v))
        .collect();
    let signed_headers: String = base_headers.keys().cloned().collect::<Vec<_>>().join(";");

    let canonical_req = format!(
        "{}\n{}\n\n{}\n{}\n{}",
        method, uri, canonical_headers, signed_headers, payload_hash
    );

    let scope = format!("{}/{}/bedrock/aws4_request", date, region);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{}\n{}",
        datetime,
        scope,
        sha256_hex(&canonical_req)
    );

    let sig_key = {
        let k = hmac_sha256(format!("AWS4{}", secret_key).as_bytes(), date);
        let k = hmac_sha256(&k, region);
        let k = hmac_sha256(&k, "bedrock");
        hmac_sha256(&k, "aws4_request")
    };
    let signature = hex::encode(hmac_sha256(&sig_key, &string_to_sign));

    let auth = format!(
        "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
        access_key, scope, signed_headers, signature
    );

    let mut out: HashMap<String, String> = base_headers.into_iter().collect();
    out.insert("authorization".to_string(), auth);
    out
}

/// Parse AWS EventStream binary header bytes.
/// Returns a map of header name → value (string type 7 only; others skipped).
pub fn parse_eventstream_headers(header_bytes: &[u8]) -> HashMap<String, String> {
    let mut headers = HashMap::new();
    let mut pos = 0;

    while pos < header_bytes.len() {
        // Name
        if pos >= header_bytes.len() {
            break;
        }
        let name_len = header_bytes[pos] as usize;
        pos += 1;
        if pos + name_len > header_bytes.len() {
            break;
        }
        let name = match std::str::from_utf8(&header_bytes[pos..pos + name_len]) {
            Ok(s) => s.to_string(),
            Err(_) => break,
        };
        pos += name_len;

        // Value type
        if pos >= header_bytes.len() {
            break;
        }
        let val_type = header_bytes[pos];
        pos += 1;

        match val_type {
            7 => {
                // string: 2-byte length, then UTF-8 bytes
                if pos + 2 > header_bytes.len() {
                    break;
                }
                let val_len =
                    u16::from_be_bytes([header_bytes[pos], header_bytes[pos + 1]]) as usize;
                pos += 2;
                if pos + val_len > header_bytes.len() {
                    break;
                }
                let val = match std::str::from_utf8(&header_bytes[pos..pos + val_len]) {
                    Ok(s) => s.to_string(),
                    Err(_) => break,
                };
                pos += val_len;
                headers.insert(name, val);
            }
            0 | 1 => { /* boolean: no body */ }
            2 => {
                pos += 1;
            } // byte
            3 => {
                pos += 2;
            } // short
            4 => {
                pos += 4;
            } // int
            5 | 8 => {
                pos += 8;
            } // long / timestamp
            6 => {
                // byte array: 2-byte length
                if pos + 2 > header_bytes.len() {
                    break;
                }
                let l = u16::from_be_bytes([header_bytes[pos], header_bytes[pos + 1]]) as usize;
                pos += 2 + l;
            }
            9 => {
                pos += 16;
            } // UUID
            _ => {
                break;
            }
        }
    }
    headers
}

/// Parse AWS EventStream binary frames from a reader.
/// For each "chunk" event, decodes the base64 payload and calls `on_event`.
pub fn parse_bedrock_stream<R: Read>(mut reader: R, mut on_event: impl FnMut(serde_json::Value)) {
    loop {
        // Read 12-byte prelude: total_len(4) + headers_len(4) + prelude_crc(4)
        let mut prelude = [0u8; 12];
        match reader.read_exact(&mut prelude) {
            Ok(()) => {}
            Err(_) => break,
        }
        let total_len = u32::from_be_bytes(prelude[0..4].try_into().unwrap()) as usize;
        let headers_len = u32::from_be_bytes(prelude[4..8].try_into().unwrap()) as usize;
        // prelude[8..12] = prelude CRC (ignored)

        let payload_len = total_len.saturating_sub(headers_len).saturating_sub(16);

        // Read header bytes
        let mut header_bytes = vec![0u8; headers_len];
        if reader.read_exact(&mut header_bytes).is_err() {
            break;
        }

        // Read payload bytes
        let mut payload_bytes = vec![0u8; payload_len];
        if payload_len > 0 && reader.read_exact(&mut payload_bytes).is_err() {
            break;
        }

        // Read message CRC (4 bytes, ignored)
        let mut msg_crc = [0u8; 4];
        if reader.read_exact(&mut msg_crc).is_err() {
            break;
        }

        let headers = parse_eventstream_headers(&header_bytes);
        if headers.get(":event-type").map(|s| s.as_str()) == Some("chunk") {
            if let Ok(payload_json) = serde_json::from_slice::<serde_json::Value>(&payload_bytes) {
                if let Some(b64) = payload_json["bytes"].as_str() {
                    use base64::Engine as _;
                    if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(b64) {
                        if let Ok(event_data) =
                            serde_json::from_slice::<serde_json::Value>(&decoded)
                        {
                            on_event(event_data);
                        }
                    }
                }
            }
        }
    }
}
