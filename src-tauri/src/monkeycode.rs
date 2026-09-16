//! MonkeyCode signing bridge. Only the bundled core talks to this loopback listener.
//! Provider metadata lives in a reserved core header so stock CPA versions preserve it.
//! It is decoded for the GUI and is never forwarded to the upstream service.
use axum::{
    body::{to_bytes, Body},
    extract::Request,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{sync::OnceLock, time::Duration};

const META_HEADER: &str = "X-EasyCLI-MonkeyCode";
const SIGNATURE_HEADER: &str = "x-ohmyagent-signature";
const SECTIONS: [&str; 3] = ["openai-compatibility", "codex-api-key", "claude-api-key"];
const MAX_BODY: usize = 32 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
struct Metadata {
    section: String,
    #[serde(default)]
    websockets: Option<bool>,
    upstream: String,
    signing_secret: String,
    #[serde(default)]
    proxy_url: Option<String>,
    #[serde(default)]
    key_proxy_url: Option<String>,
}

fn metadata(record: &Value) -> Option<Metadata> {
    let encoded = record
        .get("headers")?
        .as_object()?
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(META_HEADER))?
        .1
        .as_str()?;
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(encoded).ok()?).ok()
}

fn restore_optional(record: &mut Value, name: &str, value: Option<String>) {
    if let Some(value) = value {
        record[name] = json!(value);
    } else if let Some(object) = record.as_object_mut() {
        object.remove(name);
    }
}

fn decode_record(record: &mut Value) {
    let Some(meta) = metadata(record) else {
        return;
    };
    record["base-url"] = json!(meta.upstream);
    record["signing_secret"] = json!(meta.signing_secret);
    restore_optional(record, "proxy-url", meta.proxy_url);
    if let Some(enabled) = meta.websockets {
        record["websockets"] = json!(enabled);
    } else if meta.section == "codex-api-key" {
        record.as_object_mut().unwrap().remove("websockets");
    }
    if let Some(entry) = record.pointer_mut("/api-key-entries/0") {
        restore_optional(entry, "proxy-url", meta.key_proxy_url);
    }
    if let Some(headers) = record.get_mut("headers").and_then(Value::as_object_mut) {
        headers.retain(|key, _| !key.eq_ignore_ascii_case(META_HEADER));
    }
}

// Apply only to provider records; never traverse arbitrary request bodies or headers.
pub(crate) fn decode_config(value: &mut Value) {
    if let Some(records) = value.as_array_mut() {
        records.iter_mut().for_each(decode_record);
    } else {
        for section in SECTIONS {
            if let Some(records) = value.get_mut(section).and_then(Value::as_array_mut) {
                records.iter_mut().for_each(decode_record);
            }
        }
    }
}

fn upstream_url(value: &str) -> Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(value).map_err(|_| "MonkeyCode Base URL is invalid")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(
            "MonkeyCode Base URL must be HTTP(S), without credentials, query or fragment".into(),
        );
    }
    Ok(url)
}

fn key(record: &Value) -> &str {
    record
        .get("api-key")
        .or_else(|| record.pointer("/api-key-entries/0/api-key"))
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn route_id(record: &Value, section: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(section);
    hash.update([0]);
    hash.update(record["base-url"].as_str().unwrap_or_default());
    hash.update([0]);
    hash.update(key(record));
    format!("{:x}", hash.finalize())
}

fn encode_record(record: &mut Value, origin: &str, section: &str) -> Result<(), String> {
    decode_record(record);
    if record["signing_secret"]
        .as_str()
        .unwrap_or_default()
        .is_empty()
    {
        return Ok(());
    }
    let upstream = record["base-url"].as_str().unwrap_or_default().to_string();
    upstream_url(&upstream)?;
    let secret = record["signing_secret"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    if secret.is_empty() || secret.len() > 4096 {
        return Err("MonkeyCode signing_secret is required (maximum 4096 bytes)".into());
    }
    if (section == "openai-compatibility"
        && record["api-key-entries"].as_array().map(Vec::len) != Some(1))
        || key(record).is_empty()
    {
        return Err("MonkeyCode requires exactly one API key paired with signing_secret".into());
    }
    let id = route_id(record, section);
    let meta = Metadata {
        section: section.to_string(),
        websockets: record["websockets"].as_bool(),
        upstream,
        signing_secret: secret,
        proxy_url: record["proxy-url"].as_str().map(String::from),
        key_proxy_url: record
            .pointer("/api-key-entries/0/proxy-url")
            .and_then(Value::as_str)
            .map(String::from),
    };
    let suffix = if section == "claude-api-key" {
        ""
    } else {
        "/v1"
    };
    record["base-url"] = json!(format!("{origin}/monkeycode/{id}{suffix}"));
    if section == "codex-api-key" {
        record["websockets"] = json!(false);
    }
    // Ensure CPA reaches the local bridge even when a global proxy is configured.
    record["proxy-url"] = json!("direct");
    if let Some(entry) = record.pointer_mut("/api-key-entries/0") {
        entry["proxy-url"] = json!("direct");
    }
    if !record["headers"].is_object() {
        record["headers"] = json!({});
    }
    record["headers"][META_HEADER] = json!(URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&meta).map_err(|_| "Cannot encode MonkeyCode configuration")?));
    let object = record
        .as_object_mut()
        .ok_or("Invalid MonkeyCode provider")?;
    object.remove("signing_secret");
    Ok(())
}

pub(crate) fn encode_config(value: &mut Value, section: Option<&str>) -> Result<(), String> {
    if let Some(records) = value.as_array_mut() {
        let section = section.ok_or("Missing provider section")?;
        for record in records {
            if !record["signing_secret"]
                .as_str()
                .unwrap_or_default()
                .is_empty()
                || metadata(record).is_some()
            {
                encode_record(record, bridge_origin()?, section)?;
            }
        }
    } else {
        for section in SECTIONS {
            if let Some(records) = value.get_mut(section) {
                encode_config(records, Some(section))?;
            }
        }
    }
    Ok(())
}

pub(crate) fn refresh_routes() -> Result<(), String> {
    crate::patch_existing_core_config(|yaml| {
        let mut changed = false;
        for section in SECTIONS {
            let Some(value) = yaml.get_mut(section) else {
                continue;
            };
            let mut records =
                serde_json::to_value(&*value).map_err(|_| "Invalid provider configuration")?;
            let before = records.clone();
            encode_config(&mut records, Some(section))?;
            if records != before {
                *value = serde_norway::to_value(records)
                    .map_err(|_| "Invalid provider configuration")?;
                changed = true;
            }
        }
        Ok(changed)
    })
}

fn bridge_origin() -> Result<&'static str, String> {
    static ORIGIN: OnceLock<Result<String, String>> = OnceLock::new();
    ORIGIN
        .get_or_init(|| {
            let listener = std::net::TcpListener::bind("127.0.0.1:0")
                .map_err(|_| "Cannot bind MonkeyCode bridge")?;
            listener
                .set_nonblocking(true)
                .map_err(|_| "Cannot configure MonkeyCode bridge")?;
            let origin = format!(
                "http://{}",
                listener
                    .local_addr()
                    .map_err(|_| "Cannot read bridge address")?
            );
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|_| "Cannot initialize MonkeyCode runtime")?;
            std::thread::Builder::new()
                .name("monkeycode-bridge".into())
                .spawn(move || {
                    runtime.block_on(async {
                        let listener = tokio::net::TcpListener::from_std(listener)
                            .expect("MonkeyCode listener initialization failed");
                        let _ = axum::serve(listener, Router::new().fallback(forward)).await;
                    });
                })
                .map_err(|_| "Cannot start MonkeyCode bridge")?;
            Ok(origin)
        })
        .as_deref()
        .map_err(Clone::clone)
}

#[derive(Deserialize)]
struct Message {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    content: Value,
}
#[derive(Default, Deserialize)]
struct PromptPayload {
    #[serde(default)]
    system: Value,
    #[serde(default)]
    instructions: Value,
    #[serde(default)]
    messages: Option<Vec<Option<Message>>>,
    #[serde(default)]
    input: Option<Vec<Option<Message>>>,
}

fn prompt_text(value: &Value, first_only: bool) -> Option<String> {
    if let Some(text) = value.as_str() {
        return (!text.is_empty()).then(|| text.to_string());
    }
    let parts = value.as_array()?;
    // Match Go's []struct{Text string}: an invalid text type rejects the whole array.
    if parts.iter().any(|part| {
        !part.is_object() && !part.is_null()
            || part
                .get("text")
                .is_some_and(|text| !text.is_string() && !text.is_null())
    }) {
        return None;
    }
    if first_only {
        return parts
            .first()?
            .get("text")?
            .as_str()
            .filter(|text| !text.is_empty())
            .map(String::from);
    }
    let texts: Vec<_> = parts
        .iter()
        .filter_map(|part| part.get("text")?.as_str())
        .filter(|text| !text.is_empty())
        .collect();
    (!texts.is_empty()).then(|| texts.join("\n"))
}

pub(crate) fn signature(body: &[u8], secret: &str) -> Result<String, String> {
    if secret.is_empty() {
        return Err("MonkeyCode signing_secret is empty".into());
    }
    let payload: PromptPayload =
        serde_json::from_slice(body).map_err(|_| "Invalid MonkeyCode prompt fields")?;
    let prompt = prompt_text(&payload.system, true)
        .or_else(|| prompt_text(&payload.instructions, false))
        .or_else(|| {
            payload
                .messages
                .iter()
                .flatten()
                .flatten()
                .find(|m| m.role.as_deref() == Some("system"))
                .and_then(|m| prompt_text(&m.content, false))
        })
        .or_else(|| {
            payload
                .input
                .iter()
                .flatten()
                .flatten()
                .find(|m| matches!(m.role.as_deref(), Some("developer" | "system")))
                .and_then(|m| prompt_text(&m.content, false))
        })
        .ok_or("MonkeyCode requires a non-empty system prompt")?;
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).map_err(|_| "Invalid signing key")?;
    mac.update(prompt.as_bytes());
    Ok(format!("v1={:x}", mac.finalize().into_bytes()))
}

fn strip_private_headers(headers: &mut HeaderMap) {
    let connection_headers: Vec<String> = headers
        .get_all("connection")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|value| value.trim().to_string())
        .collect();
    for name in connection_headers {
        headers.remove(name);
    }
    for name in [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        "host",
        "content-length",
        "x-easycli-monkeycode",
        SIGNATURE_HEADER,
    ] {
        headers.remove(name);
    }
}

fn read_config() -> Result<Value, String> {
    let path = crate::core_install_dir()?.join(crate::CORE_CONFIG_FILE);
    let bytes = std::fs::read(path).map_err(|_| "Cannot read MonkeyCode provider configuration")?;
    serde_norway::from_slice(&bytes).map_err(|_| "Invalid provider configuration".into())
}

async fn forward(request: Request) -> Response {
    match forward_inner(request).await {
        Ok(response) => response,
        Err((status, message)) => (status, message).into_response(),
    }
}

type BridgeError = (StatusCode, &'static str);
async fn forward_inner(request: Request) -> Result<Response, BridgeError> {
    let config = tokio::task::spawn_blocking(read_config)
        .await
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Cannot load provider"))?
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Cannot load provider"))?;
    forward_with_config(request, config).await
}

async fn forward_with_config(request: Request, config: Value) -> Result<Response, BridgeError> {
    let path = request.uri().path().to_string();
    let parts: Vec<_> = path.trim_start_matches('/').splitn(3, '/').collect();
    if parts.len() != 3 || parts[0] != "monkeycode" {
        return Err((StatusCode::NOT_FOUND, "Unknown route"));
    }
    let endpoint = format!("/{}", parts[2]);
    let post = request.method() == axum::http::Method::POST;
    if !(post
        && matches!(
            endpoint.as_str(),
            "/v1/chat/completions"
                | "/v1/responses"
                | "/v1/responses/compact"
                | "/v1/messages"
                | "/v1/messages/count_tokens"
        )
        || request.method() == axum::http::Method::GET && endpoint == "/v1/models")
    {
        return Err((StatusCode::NOT_FOUND, "Unsupported MonkeyCode endpoint"));
    }
    let (record, section) = SECTIONS
        .iter()
        .find_map(|section| {
            config[*section].as_array()?.iter().find_map(|record| {
                let meta = metadata(record)?;
                if meta.section != *section {
                    return None;
                }
                let mut decoded = record.clone();
                decode_record(&mut decoded);
                (route_id(&decoded, section) == parts[1] && decoded["disabled"] != true)
                    .then_some((decoded, *section))
            })
        })
        .ok_or((StatusCode::NOT_FOUND, "Signed provider is unavailable"))?;
    let bearer = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let token = if section == "claude-api-key" {
        request
            .headers()
            .get("x-api-key")
            .and_then(|v| v.to_str().ok())
            .or(bearer)
    } else {
        bearer
    }
    .unwrap_or_default();
    if token != key(&record) || token.is_empty() {
        return Err((StatusCode::UNAUTHORIZED, "Invalid MonkeyCode API key"));
    }
    let upstream = record["base-url"].as_str().unwrap_or_default();
    upstream_url(upstream).map_err(|_| (StatusCode::BAD_GATEWAY, "Invalid MonkeyCode upstream"))?;
    // Preserve the existing protocol's Base URL semantics; only substitute the host/base.
    let suffix = if section == "claude-api-key" {
        endpoint.as_str()
    } else {
        endpoint.strip_prefix("/v1").unwrap_or(&endpoint)
    };
    let mut target = format!("{}{suffix}", upstream.trim_end_matches('/'));
    if let Some(query) = request.uri().query() {
        target.push('?');
        target.push_str(query);
    }
    let (parts, body) = request.into_parts();
    if parts
        .headers
        .get("content-encoding")
        .is_some_and(|v| v != "identity")
    {
        return Err((
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Compressed request bodies are not supported",
        ));
    }
    let body = tokio::time::timeout(Duration::from_secs(60), to_bytes(body, MAX_BODY))
        .await
        .map_err(|_| (StatusCode::REQUEST_TIMEOUT, "Request body timed out"))?
        .map_err(|_| {
            (
                StatusCode::PAYLOAD_TOO_LARGE,
                "Request body exceeds 32 MiB or is unreadable",
            )
        })?;
    let mut headers = parts.headers;
    strip_private_headers(&mut headers);
    if post {
        let signed = signature(&body, record["signing_secret"].as_str().unwrap_or_default())
            .map_err(|_| {
                (
                    StatusCode::BAD_REQUEST,
                    "MonkeyCode requires a valid non-empty system prompt",
                )
            })?;
        headers.insert(
            SIGNATURE_HEADER,
            signed
                .parse()
                .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Cannot sign request"))?,
        );
    }
    // The upstream prefers X-Api-Key; do not allow an unrelated caller header to override it.
    headers.insert(
        "x-api-key",
        key(&record)
            .parse()
            .map_err(|_| (StatusCode::BAD_GATEWAY, "Invalid provider key"))?,
    );
    let proxy = record
        .pointer("/api-key-entries/0/proxy-url")
        .and_then(Value::as_str)
        .or_else(|| record["proxy-url"].as_str())
        .or_else(|| config["proxy-url"].as_str())
        .unwrap_or_default();
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15));
    if !proxy.is_empty() && proxy != "direct" {
        builder = builder.proxy(
            reqwest::Proxy::all(proxy)
                .map_err(|_| (StatusCode::BAD_GATEWAY, "Invalid provider proxy"))?,
        );
    }
    let client = builder
        .build()
        .map_err(|_| (StatusCode::BAD_GATEWAY, "Cannot create upstream client"))?;
    let response = tokio::time::timeout(
        Duration::from_secs(300),
        client
            .request(parts.method, target)
            .headers(headers)
            .body(body)
            .send(),
    )
    .await
    .map_err(|_| (StatusCode::GATEWAY_TIMEOUT, "MonkeyCode upstream timed out"))?
    .map_err(|_| {
        (
            StatusCode::BAD_GATEWAY,
            "MonkeyCode upstream request failed",
        )
    })?;
    let status = response.status();
    let mut headers = response.headers().clone();
    strip_private_headers(&mut headers);
    let mut outgoing = Response::new(Body::from_stream(response.bytes_stream()));
    *outgoing.status_mut() = status;
    *outgoing.headers_mut() = headers;
    Ok(outgoing)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(upstream: &str) -> Value {
        json!({"name":"test", "base-url":upstream,
            "signing_secret":"omas_test_secret", "api-key-entries":[{"api-key":"oma_test"}],
            "headers":{"X-Team":"test"}, "models":[{"name":"test-model"}]})
    }

    #[test]
    fn signing_uses_full_secret_and_preserves_prompt_bytes() {
        // Independently calculated with Node crypto.createHmac('sha256', ...).
        let body = json!({"instructions":"你好\n  world "}).to_string();
        assert_eq!(
            signature(body.as_bytes(), "omas_test_secret").unwrap(),
            "v1=b529b56a975dabea4b14956e84dbce9dd6c00dc3b6017e41a961899a9fa47a85"
        );
        assert_ne!(
            signature(body.as_bytes(), "test_secret").unwrap(),
            signature(body.as_bytes(), "omas_test_secret").unwrap()
        );
        assert!(signature(body.as_bytes(), "").is_err());
    }

    #[test]
    fn prompt_precedence_and_block_rules_match_monkeycode() {
        let sign =
            |value: Value| signature(value.to_string().as_bytes(), "omas_test_secret").unwrap();
        let expected = sign(json!({"instructions":"first"}));
        assert_eq!(
            sign(json!({"system":[{"text":"first"},{"text":"ignored"}],"instructions":"ignored"})),
            expected
        );
        assert_eq!(
            sign(json!({"messages":[{"role":"system","content":"first"}]})),
            expected
        );
        assert_eq!(
            sign(json!({"input":[{"role":"developer","content":[{"text":"first"}]}]})),
            expected
        );
        assert_eq!(
            sign(json!({"instructions":[{"text":"one"},{"text":""},{"text":"two"}]})),
            sign(json!({"instructions":"one\ntwo"}))
        );
        assert_eq!(
            sign(json!({"system":[{"text":""},{"text":"ignored"}],"instructions":"first"})),
            expected
        );
        assert_eq!(
            sign(json!({"messages":[null,{"role":"system","content":"first"}],"input":null})),
            expected
        );
        for body in [
            json!({"messages":[{"role":"user","content":"hi"}]}),
            json!({"messages":[{"role":"system","content":""},{"role":"system","content":"must not use"}]}),
            json!({"input":"string input","instructions":"first"}),
        ] {
            assert!(signature(body.to_string().as_bytes(), "omas_test_secret").is_err());
        }
    }

    #[test]
    fn provider_round_trip_and_restart_rebinding_preserve_settings() {
        let mut original = provider("https://mc.example/v1");
        original["proxy-url"] = json!("http://proxy.example:8080");
        original["api-key-entries"][0]["proxy-url"] = json!("direct");
        let mut record = original.clone();
        encode_record(
            &mut record,
            "http://127.0.0.1:12345",
            "openai-compatibility",
        )
        .unwrap();
        assert!(record["base-url"]
            .as_str()
            .unwrap()
            .starts_with("http://127.0.0.1:12345/monkeycode/"));
        assert!(record.get("signing_secret").is_none());
        assert_eq!(record["api-key-entries"][0]["proxy-url"], "direct");
        encode_record(
            &mut record,
            "http://127.0.0.1:23456",
            "openai-compatibility",
        )
        .unwrap();
        assert!(record["base-url"]
            .as_str()
            .unwrap()
            .starts_with("http://127.0.0.1:23456/monkeycode/"));
        decode_record(&mut record);
        assert_eq!(record, original);
        let mut ordinary =
            json!({"name":"ordinary","base-url":"https://api.example","custom":true});
        let before = ordinary.clone();
        encode_record(&mut ordinary, "http://127.0.0.1:1", "openai-compatibility").unwrap();
        assert_eq!(ordinary, before);
    }

    #[test]
    fn all_protocols_round_trip_and_can_disable_signing() {
        let mut config = json!({
            "openai-compatibility": [provider("https://mc.example/v1")],
            "codex-api-key": [{"api-key":"oma_test", "base-url":"https://mc.example/v1",
                "signing_secret":"omas_test_secret", "websockets":true}],
            "claude-api-key": [{"api-key":"oma_test", "base-url":"https://mc.example",
                "signing_secret":"omas_test_secret", "proxy-url":"direct"}],
            "gemini-api-key": [{"api-key":"untouched", "custom":true}]
        });
        let original = config.clone();
        encode_config(&mut config, None).unwrap();
        assert_eq!(config["codex-api-key"][0]["websockets"], false);
        assert!(!config["claude-api-key"][0]["base-url"]
            .as_str()
            .unwrap()
            .ends_with("/v1"));
        assert_eq!(config["gemini-api-key"], original["gemini-api-key"]);
        decode_config(&mut config);
        // Core headers may have been absent originally; an empty header object is harmless.
        for section in ["codex-api-key", "claude-api-key"] {
            assert_eq!(config[section][0]["signing_secret"], "omas_test_secret");
            config[section][0]
                .as_object_mut()
                .unwrap()
                .remove("headers");
        }
        assert_eq!(config, original);
        for section in SECTIONS {
            config[section][0]
                .as_object_mut()
                .unwrap()
                .remove("signing_secret");
        }
        let unsigned = config.clone();
        encode_config(&mut config, None).unwrap();
        assert_eq!(config, unsigned);
    }

    #[tokio::test]
    async fn forwards_each_protocol_without_changing_path_body_or_prompt() {
        for (section, base_suffix, endpoint, body, auth_header, auth_value) in [
            (
                "openai-compatibility",
                "/custom/v1",
                "/v1/chat/completions",
                json!({"messages":[{"role":"system","content":"chat prompt"}]}),
                "authorization",
                "Bearer oma_test",
            ),
            (
                "codex-api-key",
                "/custom/v1",
                "/v1/responses",
                json!({"instructions":"responses prompt", "input":[{"role":"user","content":"hi"}]}),
                "authorization",
                "Bearer oma_test",
            ),
            (
                "claude-api-key",
                "/custom",
                "/v1/messages",
                json!({"system":[{"type":"text","text":"anthropic prompt"}], "messages":[{"role":"user","content":"hi"}]}),
                "x-api-key",
                "oma_test",
            ),
        ] {
            let raw = body.to_string();
            let expected_body = raw.clone();
            let expected_signature = signature(raw.as_bytes(), "omas_test_secret").unwrap();
            let expected_path = format!("/custom{endpoint}?beta=true");
            let upstream = Router::new().fallback(move |request: Request| {
                let expected_body = expected_body.clone();
                let expected_signature = expected_signature.clone();
                let expected_path = expected_path.clone();
                async move {
                    assert_eq!(request.uri().to_string(), expected_path);
                    assert_eq!(request.headers()[SIGNATURE_HEADER], expected_signature);
                    assert_eq!(request.headers()[auth_header], auth_value);
                    assert!(!request.headers().contains_key(META_HEADER));
                    assert_eq!(
                        to_bytes(request.into_body(), MAX_BODY)
                            .await
                            .unwrap()
                            .as_ref(),
                        expected_body.as_bytes()
                    );
                    (StatusCode::CREATED, "upstream response")
                }
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server =
                tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
            let mut record = provider(&format!("http://{address}{base_suffix}"));
            if section != "openai-compatibility" {
                record.as_object_mut().unwrap().remove("api-key-entries");
                record["api-key"] = json!("oma_test");
            }
            let id = route_id(&record, section);
            encode_record(&mut record, "http://127.0.0.1:1", section).unwrap();
            let mut config = json!({});
            config[section] = json!([record]);
            let request = Request::builder()
                .method("POST")
                .uri(format!("/monkeycode/{id}{endpoint}?beta=true"))
                .header(auth_header, auth_value)
                .header(META_HEADER, "must-not-leak")
                .body(Body::from(raw))
                .unwrap();
            let response = forward_with_config(request, config).await.unwrap();
            assert_eq!(response.status(), StatusCode::CREATED);
            assert_eq!(
                to_bytes(response.into_body(), MAX_BODY)
                    .await
                    .unwrap()
                    .as_ref(),
                b"upstream response"
            );
            server.abort();
        }
    }

    #[test]
    fn invalid_configuration_is_rejected() {
        for upstream in [
            "file:///etc/passwd",
            "https://user:pass@example.com",
            "https://example.com?token=x",
        ] {
            assert!(encode_record(
                &mut provider(upstream),
                "http://127.0.0.1:1",
                "openai-compatibility"
            )
            .is_err());
        }
        let mut record = provider("https://mc.example");
        record["signing_secret"] = json!("");
        let before = record.clone();
        encode_record(&mut record, "http://127.0.0.1:1", "openai-compatibility").unwrap();
        assert_eq!(record, before);
        record["signing_secret"] = json!("omas_test");
        record["api-key-entries"]
            .as_array_mut()
            .unwrap()
            .push(json!({"api-key":"oma_other"}));
        assert!(encode_record(&mut record, "http://127.0.0.1:1", "openai-compatibility").is_err());
    }

    #[tokio::test]
    async fn forwards_signed_body_and_stream_without_private_headers() {
        use futures_util::{stream, StreamExt};
        let raw = r#"{ "model": "test-model", "messages": [{"role":"system","content":"first"}], "stream":true }"#;
        let expected_signature = signature(raw.as_bytes(), "omas_test_secret").unwrap();
        let upstream = Router::new().fallback(move |request: Request| {
            let expected_signature = expected_signature.clone();
            async move {
                assert_eq!(request.uri().path(), "/v1/chat/completions");
                assert_eq!(request.headers()[SIGNATURE_HEADER], expected_signature);
                assert_eq!(request.headers()["authorization"], "Bearer oma_test");
                assert_eq!(request.headers()["x-api-key"], "oma_test");
                assert!(!request.headers().contains_key(META_HEADER));
                let body = to_bytes(request.into_body(), MAX_BODY).await.unwrap();
                assert_eq!(body.as_ref(), raw.as_bytes());
                let chunks = stream::once(async { Ok::<_, std::io::Error>("data: first\n\n") })
                    .chain(stream::pending());
                let mut response = Response::new(Body::from_stream(chunks));
                response
                    .headers_mut()
                    .insert("content-type", "text/event-stream".parse().unwrap());
                response
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
        let mut record = provider(&format!("http://{address}/v1"));
        let id = route_id(&record, "openai-compatibility");
        encode_record(&mut record, "http://127.0.0.1:1", "openai-compatibility").unwrap();
        let config = json!({"openai-compatibility":[record]});
        let make_request = |key: &str| {
            Request::builder()
                .method("POST")
                .uri(format!("/monkeycode/{id}/v1/chat/completions"))
                .header("authorization", format!("Bearer {key}"))
                .header(META_HEADER, "must-not-leak")
                .header(SIGNATURE_HEADER, "stale-signature")
                .header("x-api-key", "unrelated-key")
                .body(Body::from(raw))
                .unwrap()
        };
        assert_eq!(
            forward_with_config(make_request("wrong"), config.clone())
                .await
                .unwrap_err()
                .0,
            StatusCode::UNAUTHORIZED
        );
        let response = forward_with_config(make_request("oma_test"), config.clone())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let mut stream = response.into_body().into_data_stream();
        let chunk = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(chunk.as_ref(), b"data: first\n\n");
        let mut disabled = config;
        disabled["openai-compatibility"][0]["disabled"] = json!(true);
        assert_eq!(
            forward_with_config(make_request("oma_test"), disabled)
                .await
                .unwrap_err()
                .0,
            StatusCode::NOT_FOUND
        );
        server.abort();
    }
}
