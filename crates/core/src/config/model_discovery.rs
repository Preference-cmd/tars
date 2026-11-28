//! Endpoint `/models` URL derivation and bounded asynchronous model discovery.

use crate::definition::Provider;
use serde_json::Value;
use std::collections::HashSet;
use std::time::Duration;

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_DISCOVERY_BODY_BYTES: usize = 4 * 1024 * 1024;
const MAX_ERROR_BODY_BYTES: usize = 16 * 1024;
const MAX_ERROR_MESSAGE_CHARS: usize = 200;

/// A model entry discovered from a provider `/models` endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredModel {
    /// Canonical model identifier returned by the endpoint.
    pub id: String,
    /// Optional human-readable display name when provided and distinct from `id`.
    pub display_name: Option<String>,
    /// Optional owner or organization label reported by the endpoint.
    pub owned_by: Option<String>,
}

/// Errors returned while deriving a `/models` URL or querying an endpoint.
///
/// Error variants and formatted messages never expose API keys, authorization
/// headers, embedded URL credentials, or raw request URLs/query strings.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ModelDiscoveryError {
    /// Endpoint URL is invalid, non-HTTP(S), has whitespace/control chars, or embeds credentials.
    #[error(
        "invalid endpoint URL: must be an http(s) URL without whitespace, control characters, or embedded credentials"
    )]
    InvalidEndpoint,
    /// Transport, TLS, or timeout error (without exposing URLs or credentials).
    #[error("model discovery request failed: {0}")]
    Network(String),
    /// Endpoint returned a non-2xx HTTP status code.
    #[error("endpoint returned HTTP {status}: {message}")]
    HttpStatus {
        /// HTTP status code returned by the endpoint.
        status: u16,
        /// Sanitized short error summary.
        message: String,
    },
    /// Response body exceeded the 4 MiB discovery limit.
    #[error("model discovery response exceeded 4 MiB limit")]
    ResponseTooLarge,
    /// Response was not valid JSON or did not contain a recognized model list.
    #[error("invalid model discovery response: {0}")]
    InvalidResponse(String),
}

/// Derive the `/models` discovery URL for `provider` and an optional `endpoint`.
///
/// Validates custom endpoints against HTTP(S) rules (rejecting non-HTTP(S) schemes,
/// missing hosts, embedded `username`/`password` userinfo, whitespace, and control
/// characters), strips query/fragment components and trailing slashes, removes
/// protocol-specific chat/messages suffixes (`/chat/completions`, `/responses`,
/// `/messages`), and appends `/models` (with `?limit=100` for [`Provider::Anthropic`]).
pub fn derive_models_url(
    provider: Provider,
    endpoint: Option<&str>,
) -> Result<String, ModelDiscoveryError> {
    let trimmed = endpoint.map(str::trim).filter(|value| !value.is_empty());
    let Some(raw) = trimmed else {
        return Ok(match provider {
            Provider::OpenAiChat | Provider::OpenAiResponses => {
                "https://api.openai.com/v1/models".to_owned()
            }
            Provider::Anthropic => "https://api.anthropic.com/v1/models?limit=100".to_owned(),
        });
    };

    if raw.chars().any(char::is_whitespace) || raw.chars().any(char::is_control) {
        return Err(ModelDiscoveryError::InvalidEndpoint);
    }
    if raw
        .split_once("://")
        .and_then(|(_, rest)| rest.split(['/', '?', '#']).next())
        .is_some_and(|authority| authority.contains('@'))
    {
        return Err(ModelDiscoveryError::InvalidEndpoint);
    }
    let mut url = url::Url::parse(raw).map_err(|_| ModelDiscoveryError::InvalidEndpoint)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none_or(str::is_empty)
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(ModelDiscoveryError::InvalidEndpoint);
    }
    url.set_query(None);
    url.set_fragment(None);

    let path = url.path().trim_end_matches('/');
    let base = match provider {
        Provider::OpenAiChat | Provider::OpenAiResponses => path
            .strip_suffix("/chat/completions")
            .or_else(|| path.strip_suffix("/responses"))
            .unwrap_or(path),
        Provider::Anthropic => path.strip_suffix("/messages").unwrap_or(path),
    }
    .trim_end_matches('/');

    let models_path = if base.ends_with("/models") {
        base.to_owned()
    } else if base.is_empty() {
        "/models".to_owned()
    } else {
        format!("{base}/models")
    };
    url.set_path(&models_path);
    if provider == Provider::Anthropic {
        url.set_query(Some("limit=100"));
    }
    Ok(url.to_string())
}

/// Fetch available models from the provider's derived `/models` endpoint.
///
/// Disables HTTP redirects (`Policy::none()`), enforces a 10-second request timeout,
/// caps response bodies at 4 MiB, sends protocol-appropriate auth headers when
/// `api_key` is non-empty, and parses OpenAI/Anthropic/OpenRouter (`data`), Ollama
/// (`models`), or top-level array payloads while filtering invalid entries and
/// deduplicating in response order.
pub async fn fetch_available_models(
    provider: Provider,
    endpoint: Option<&str>,
    api_key: Option<&str>,
) -> Result<Vec<DiscoveredModel>, ModelDiscoveryError> {
    let url = derive_models_url(provider, endpoint)?;
    let trimmed_key = api_key.map(str::trim).filter(|key| !key.is_empty());

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(DISCOVERY_TIMEOUT)
        .build()
        .map_err(|err| {
            ModelDiscoveryError::Network(sanitize_reqwest_error(err, trimmed_key, &url))
        })?;

    let mut request = client.get(&url);
    match provider {
        Provider::OpenAiChat | Provider::OpenAiResponses => {
            if let Some(key) = trimmed_key {
                request = request.header(reqwest::header::AUTHORIZATION, format!("Bearer {key}"));
            }
        }
        Provider::Anthropic => {
            if let Some(key) = trimmed_key {
                request = request.header("x-api-key", key);
            }
            request = request.header("anthropic-version", "2023-06-01");
        }
    }

    let mut response = request.send().await.map_err(|err| {
        ModelDiscoveryError::Network(sanitize_reqwest_error(err, trimmed_key, &url))
    })?;

    let status = response.status();
    if !status.is_success() {
        let mut err_body = Vec::new();
        while err_body.len() < MAX_ERROR_BODY_BYTES {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    let remaining = MAX_ERROR_BODY_BYTES - err_body.len();
                    let take = chunk.len().min(remaining);
                    err_body.extend_from_slice(&chunk[..take]);
                    if take < chunk.len() {
                        break;
                    }
                }
                Ok(None) | Err(_) => break,
            }
        }
        let message = extract_error_message(status, &err_body, trimmed_key, &url);
        return Err(ModelDiscoveryError::HttpStatus {
            status: status.as_u16(),
            message,
        });
    }

    if let Some(content_length) = response.content_length()
        && content_length > MAX_DISCOVERY_BODY_BYTES as u64
    {
        return Err(ModelDiscoveryError::ResponseTooLarge);
    }

    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|err| {
        ModelDiscoveryError::Network(sanitize_reqwest_error(err, trimmed_key, &url))
    })? {
        if body.len().saturating_add(chunk.len()) > MAX_DISCOVERY_BODY_BYTES {
            return Err(ModelDiscoveryError::ResponseTooLarge);
        }
        body.extend_from_slice(&chunk);
    }

    parse_models_response(&body)
}

fn parse_models_response(bytes: &[u8]) -> Result<Vec<DiscoveredModel>, ModelDiscoveryError> {
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|_| ModelDiscoveryError::InvalidResponse("response is not valid JSON".into()))?;

    let items = match &value {
        Value::Object(map) => {
            if let Some(data) = map.get("data") {
                data.as_array().ok_or_else(|| {
                    ModelDiscoveryError::InvalidResponse("'data' field must be an array".into())
                })?
            } else if let Some(models) = map.get("models") {
                models.as_array().ok_or_else(|| {
                    ModelDiscoveryError::InvalidResponse("'models' field must be an array".into())
                })?
            } else {
                return Err(ModelDiscoveryError::InvalidResponse(
                    "missing 'data' or 'models' array".into(),
                ));
            }
        }
        Value::Array(items) => items,
        _ => {
            return Err(ModelDiscoveryError::InvalidResponse(
                "expected JSON object or array".into(),
            ));
        }
    };

    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for item in items {
        let Some(obj) = item.as_object() else {
            continue;
        };
        let Some(raw_id) = obj
            .get("id")
            .and_then(Value::as_str)
            .or_else(|| obj.get("name").and_then(Value::as_str))
            .or_else(|| obj.get("model").and_then(Value::as_str))
        else {
            continue;
        };
        let id = raw_id.trim();
        if id.is_empty() || id.chars().any(char::is_control) {
            continue;
        }
        if !seen.insert(id.to_owned()) {
            continue;
        }
        let display_name = obj
            .get("display_name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty() && !s.chars().any(char::is_control))
            .or_else(|| {
                obj.get("name")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty() && !s.chars().any(char::is_control) && *s != id)
            })
            .map(str::to_owned);
        let owned_by = obj
            .get("owned_by")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty() && !s.chars().any(char::is_control))
            .map(str::to_owned);
        out.push(DiscoveredModel {
            id: id.to_owned(),
            display_name,
            owned_by,
        });
    }
    Ok(out)
}

fn sanitize_reqwest_error(err: reqwest::Error, api_key: Option<&str>, url: &str) -> String {
    if err.is_timeout() {
        return "request timed out".to_owned();
    }
    let clean = err.without_url().to_string();
    let sanitized = sanitize_error_text(&clean, api_key, url);
    if sanitized.is_empty() {
        "transport error".to_owned()
    } else {
        sanitized
    }
}

fn extract_error_message(
    status: reqwest::StatusCode,
    body: &[u8],
    api_key: Option<&str>,
    url: &str,
) -> String {
    let extracted = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|err| err.get("message"))
                .and_then(Value::as_str)
                .or_else(|| value.get("error").and_then(Value::as_str))
                .or_else(|| value.get("message").and_then(Value::as_str))
                .map(str::to_owned)
        })
        .or_else(|| std::str::from_utf8(body).ok().map(str::to_owned));

    if let Some(raw) = extracted {
        let clean = sanitize_error_text(&raw, api_key, url);
        if !clean.is_empty() {
            return clean;
        }
    }
    status
        .canonical_reason()
        .unwrap_or("request failed")
        .to_owned()
}

fn sanitize_error_text(raw: &str, api_key: Option<&str>, url: &str) -> String {
    let mut text = raw.to_owned();
    if let Some(key) = api_key.filter(|k| !k.is_empty()) {
        text = text.replace(key, "[redacted]");
    }
    if !url.is_empty() {
        text = text.replace(url, "[redacted-url]");
    }
    let mut out = String::new();
    let mut prev_space = false;
    let mut chars_written = 0usize;
    for ch in text.chars() {
        let mapped = if ch.is_control() || ch.is_whitespace() {
            ' '
        } else {
            ch
        };
        if mapped == ' ' {
            if prev_space || out.is_empty() {
                continue;
            }
            prev_space = true;
        } else {
            prev_space = false;
        }
        if chars_written >= MAX_ERROR_MESSAGE_CHARS {
            out.push_str("...");
            break;
        }
        out.push(mapped);
        chars_written += 1;
    }
    out.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn spawn_http_server<F, Fut>(handler: F) -> String
    where
        F: FnOnce(String) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Vec<u8>> + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 8192];
            let n = socket.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).into_owned();
            let resp = handler(req).await;
            let _ = socket.write_all(&resp).await;
            let _ = socket.shutdown().await;
        });
        format!("http://{addr}")
    }

    #[test]
    fn derive_models_url_presets_custom_and_rejections() {
        assert_eq!(
            derive_models_url(Provider::OpenAiResponses, None).unwrap(),
            "https://api.openai.com/v1/models"
        );
        assert_eq!(
            derive_models_url(Provider::OpenAiChat, Some("   ")).unwrap(),
            "https://api.openai.com/v1/models"
        );
        assert_eq!(
            derive_models_url(Provider::Anthropic, None).unwrap(),
            "https://api.anthropic.com/v1/models?limit=100"
        );
        assert_eq!(
            derive_models_url(
                Provider::OpenAiChat,
                Some("https://openrouter.ai/api/v1/chat/completions")
            )
            .unwrap(),
            "https://openrouter.ai/api/v1/models"
        );
        assert_eq!(
            derive_models_url(
                Provider::OpenAiChat,
                Some("https://api.deepseek.com/v1/chat/completions")
            )
            .unwrap(),
            "https://api.deepseek.com/v1/models"
        );
        assert_eq!(
            derive_models_url(
                Provider::OpenAiChat,
                Some("https://api.x.ai/v1/chat/completions")
            )
            .unwrap(),
            "https://api.x.ai/v1/models"
        );
        assert_eq!(
            derive_models_url(
                Provider::OpenAiChat,
                Some("https://generativelanguage.googleapis.com/v1beta/openai/chat/completions")
            )
            .unwrap(),
            "https://generativelanguage.googleapis.com/v1beta/openai/models"
        );
        assert_eq!(
            derive_models_url(
                Provider::OpenAiChat,
                Some("http://127.0.0.1:11434/v1/chat/completions")
            )
            .unwrap(),
            "http://127.0.0.1:11434/v1/models"
        );
        assert_eq!(
            derive_models_url(
                Provider::OpenAiResponses,
                Some("https://custom.example/v1/responses?foo=bar#frag")
            )
            .unwrap(),
            "https://custom.example/v1/models"
        );
        assert_eq!(
            derive_models_url(
                Provider::Anthropic,
                Some("https://custom.example/v1/messages?foo=bar#frag")
            )
            .unwrap(),
            "https://custom.example/v1/models?limit=100"
        );
        assert_eq!(
            derive_models_url(Provider::OpenAiChat, Some("https://custom.example/v1/")).unwrap(),
            "https://custom.example/v1/models"
        );
        assert_eq!(
            derive_models_url(Provider::OpenAiChat, Some("https://custom.example")).unwrap(),
            "https://custom.example/models"
        );
        assert_eq!(
            derive_models_url(
                Provider::OpenAiChat,
                Some("https://custom.example/v1/models")
            )
            .unwrap(),
            "https://custom.example/v1/models"
        );

        for invalid in [
            "ftp://example.com/v1",
            "file:///etc/passwd",
            "https://user:pass@example.com/v1",
            "https://user@example.com/v1",
            "https://:pass@example.com/v1",
            "https://@example.com/v1",
            "https://example.com/v1 with space",
            "https://example.com/v1\nbar",
            "https://example.com/v1\x00bar",
            "not-a-url",
        ] {
            assert_eq!(
                derive_models_url(Provider::OpenAiChat, Some(invalid)),
                Err(ModelDiscoveryError::InvalidEndpoint),
                "expected InvalidEndpoint for {invalid:?}"
            );
        }
    }

    #[tokio::test]
    async fn fetch_available_models_openai_openrouter_dedup_and_filtering() {
        let base = spawn_http_server(|req| async move {
            assert!(req.starts_with("GET /v1/models HTTP/1.1\r\n"));
            let lower = req.to_ascii_lowercase();
            assert!(lower.contains("authorization: bearer sk-test-openai\r\n"));
            let body = r#"{
              "data": [
                {"id": "gpt-5.4", "owned_by": "openai"},
                {"id": "  gpt-5.4  ", "owned_by": "duplicate"},
                {"id": "   "},
                {"id": "bad\u0007model"},
                {"id": "anthropic/claude-sonnet-4", "name": "Anthropic: Claude Sonnet 4", "owned_by": "openrouter"}
              ]
            }"#;
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .into_bytes()
        })
        .await;

        let models = fetch_available_models(
            Provider::OpenAiResponses,
            Some(&format!("{base}/v1/responses")),
            Some("sk-test-openai"),
        )
        .await
        .unwrap();

        assert_eq!(
            models,
            vec![
                DiscoveredModel {
                    id: "gpt-5.4".into(),
                    display_name: None,
                    owned_by: Some("openai".into()),
                },
                DiscoveredModel {
                    id: "anthropic/claude-sonnet-4".into(),
                    display_name: Some("Anthropic: Claude Sonnet 4".into()),
                    owned_by: Some("openrouter".into()),
                },
            ]
        );
    }

    #[tokio::test]
    async fn fetch_available_models_anthropic_headers_and_display_name() {
        let base = spawn_http_server(|req| async move {
            assert!(req.starts_with("GET /v1/models?limit=100 HTTP/1.1\r\n"));
            let lower = req.to_ascii_lowercase();
            assert!(lower.contains("x-api-key: sk-ant-secret\r\n"));
            assert!(lower.contains("anthropic-version: 2023-06-01\r\n"));
            let body = r#"{"data":[{"id":"claude-sonnet-4-6","display_name":"Claude Sonnet 4.6"}]}"#;
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .into_bytes()
        })
        .await;

        let models = fetch_available_models(
            Provider::Anthropic,
            Some(&format!("{base}/v1/messages")),
            Some("sk-ant-secret"),
        )
        .await
        .unwrap();

        assert_eq!(
            models,
            vec![DiscoveredModel {
                id: "claude-sonnet-4-6".into(),
                display_name: Some("Claude Sonnet 4.6".into()),
                owned_by: None,
            }]
        );
    }

    #[tokio::test]
    async fn fetch_available_models_supports_ollama_models_key_and_top_level_array() {
        let ollama_base = spawn_http_server(|_| async move {
            let body = r#"{"models":[{"name":"qwen3:8b","model":"qwen3:8b"},{"name":"llama3.3:70b"}]}"#;
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .into_bytes()
        })
        .await;

        let ollama_models = fetch_available_models(
            Provider::OpenAiChat,
            Some(&format!("{ollama_base}/v1/chat/completions")),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            ollama_models,
            vec![
                DiscoveredModel {
                    id: "qwen3:8b".into(),
                    display_name: None,
                    owned_by: None,
                },
                DiscoveredModel {
                    id: "llama3.3:70b".into(),
                    display_name: None,
                    owned_by: None,
                },
            ]
        );

        let array_base = spawn_http_server(|_| async move {
            let body = r#"[{"id":"custom-a"},{"id":"custom-b","display_name":"Custom B"}]"#;
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .into_bytes()
        })
        .await;

        let array_models = fetch_available_models(
            Provider::OpenAiChat,
            Some(&format!("{array_base}/v1")),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            array_models,
            vec![
                DiscoveredModel {
                    id: "custom-a".into(),
                    display_name: None,
                    owned_by: None,
                },
                DiscoveredModel {
                    id: "custom-b".into(),
                    display_name: Some("Custom B".into()),
                    owned_by: None,
                },
            ]
        );
    }

    #[tokio::test]
    async fn fetch_available_models_redacts_keys_and_urls_on_http_and_network_errors() {
        let secret = "sk-super-secret-token-999";
        let base = spawn_http_server(|_| async move {
            let body = r#"{"error":{"message":"invalid token sk-super-secret-token-999"}}"#;
            format!(
                "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .into_bytes()
        })
        .await;

        let err = fetch_available_models(
            Provider::OpenAiChat,
            Some(&format!("{base}/v1/chat/completions")),
            Some(secret),
        )
        .await
        .unwrap_err();
        let display = err.to_string();
        let debug = format!("{err:?}");
        assert!(matches!(
            err,
            ModelDiscoveryError::HttpStatus { status: 401, .. }
        ));
        assert!(!display.contains(secret));
        assert!(!debug.contains(secret));

        // Unreachable port produces Network error without leaking URL/secret
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_addr = listener.local_addr().unwrap();
        drop(listener);
        let dead_url = format!("http://{dead_addr}/v1/chat/completions?secret={secret}");
        let net_err = fetch_available_models(Provider::OpenAiChat, Some(&dead_url), Some(secret))
            .await
            .unwrap_err();
        assert!(matches!(net_err, ModelDiscoveryError::Network(_)));
        let net_display = net_err.to_string();
        let net_debug = format!("{net_err:?}");
        assert!(!net_display.contains(secret));
        assert!(!net_debug.contains(secret));
        assert!(!net_display.contains(&dead_addr.to_string()));
        assert!(!net_debug.contains(&dead_addr.to_string()));
    }

    #[tokio::test]
    async fn fetch_available_models_rejects_redirect_oversize_and_malformed_json() {
        let redirect_base = spawn_http_server(|_| async move {
            b"HTTP/1.1 302 Found\r\nLocation: https://evil.example/steal\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
        })
        .await;
        let redirect_err = fetch_available_models(
            Provider::OpenAiChat,
            Some(&format!("{redirect_base}/v1")),
            Some("sk-must-not-follow"),
        )
        .await
        .unwrap_err();
        assert!(matches!(
            redirect_err,
            ModelDiscoveryError::HttpStatus { status: 302, .. }
        ));
        assert!(!redirect_err.to_string().contains("evil.example"));

        let oversized_base = spawn_http_server(|_| async move {
            let payload_len = MAX_DISCOVERY_BODY_BYTES + 1024;
            let mut resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {payload_len}\r\nConnection: close\r\n\r\n"
            )
            .into_bytes();
            resp.extend(std::iter::repeat_n(b' ', payload_len));
            resp
        })
        .await;
        let oversize_err = fetch_available_models(
            Provider::OpenAiChat,
            Some(&format!("{oversized_base}/v1")),
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(oversize_err, ModelDiscoveryError::ResponseTooLarge);

        // Streamed oversize without Content-Length header
        let chunked_oversize_base = spawn_http_server(|_| async move {
            let mut resp =
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n"
                    .to_vec();
            resp.extend(std::iter::repeat_n(b' ', MAX_DISCOVERY_BODY_BYTES + 128));
            resp
        })
        .await;
        let chunked_err = fetch_available_models(
            Provider::OpenAiChat,
            Some(&format!("{chunked_oversize_base}/v1")),
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(chunked_err, ModelDiscoveryError::ResponseTooLarge);

        let malformed_base = spawn_http_server(|_| async move {
            let body = "{not-json";
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .into_bytes()
        })
        .await;
        let malformed_err = fetch_available_models(
            Provider::OpenAiChat,
            Some(&format!("{malformed_base}/v1")),
            None,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            malformed_err,
            ModelDiscoveryError::InvalidResponse(_)
        ));
    }
}
