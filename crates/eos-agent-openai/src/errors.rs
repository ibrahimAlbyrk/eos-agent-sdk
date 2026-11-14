//! Responses errors (HTTP answers, `response.failed`, `error` events) as
//! [`ModelError`]s, by the codes Sign in with ChatGPT and the API document.

use std::time::Duration;

use eos_agent_core::{ModelError, RateLimitInfo};
use reqwest::header::HeaderMap;
use serde_json::Value;

/// A refused HTTP request.
pub(crate) fn from_status(status: u16, headers: &HeaderMap, body: &str, limits: Option<RateLimitInfo>) -> ModelError {
    let value: Value = serde_json::from_str(body).unwrap_or_default();
    // Admission errors may come as `{"detail": …}` instead of `{"error": …}`.
    let error = match (&value["error"], &value["detail"]) {
        (e @ Value::Object(_), _) => e.clone(),
        (_, d @ Value::Object(_)) => d.clone(),
        (_, Value::String(text)) => serde_json::json!({ "message": text }),
        _ => serde_json::json!({ "message": body.chars().take(500).collect::<String>() }),
    };
    let retry_after = header(headers, "retry-after")
        .and_then(|v| v.parse::<f64>().ok())
        .map(Duration::from_secs_f64)
        .or_else(|| retry_hint(error["message"].as_str().unwrap_or_default()));
    if let Some(mapped) = by_code(&error, retry_after, limits.clone()) {
        return mapped;
    }
    let message = format!("{status}: {}", message(&error));
    match status {
        429 => ModelError::RateLimited { retry_after },
        401 | 403 => ModelError::Auth(message),
        408 | 409 | 500.. => ModelError::Transient(message),
        _ => ModelError::Invalid(message),
    }
}

/// The `error` of a failed response or an `error` event.
pub(crate) fn from_error_object(error: &Value) -> ModelError {
    let retry_after = retry_hint(error["message"].as_str().unwrap_or_default());
    by_code(error, retry_after, None).unwrap_or_else(|| {
        let code = error["code"].as_str().or_else(|| error["type"].as_str()).unwrap_or("response_failed");
        ModelError::Api { kind: code.to_owned(), message: message(error) }
    })
}

fn message(error: &Value) -> String {
    let text = error["message"].as_str().filter(|m| !m.trim().is_empty()).unwrap_or("no message");
    match (error["code"].as_str(), error["param"].as_str()) {
        (Some(code), Some(param)) => format!("{code}: {text} (param: {param})"),
        (Some(code), None) => format!("{code}: {text}"),
        _ => text.to_owned(),
    }
}

fn by_code(error: &Value, retry_after: Option<Duration>, limits: Option<RateLimitInfo>) -> Option<ModelError> {
    let code = error["code"].as_str().or_else(|| error["type"].as_str())?;
    let limit = |resets_at: Option<u64>| {
        let mut info = limits.clone().unwrap_or_default();
        info.status = Some("rejected".into());
        // "Don't infer a reset time" unless the answer gives one.
        info.resets_at = resets_at.or(info.resets_at);
        ModelError::LimitReached { info }
    };
    Some(match code {
        "subscription_sharing_usage_limit_exceeded" | "insufficient_quota" | "usage_not_included" => limit(None),
        "usage_limit_reached" => limit(error["resets_at"].as_u64()),
        "context_length_exceeded" => ModelError::ContextWindowExceeded(message(error)),
        "rate_limit_exceeded" | "slow_down" => ModelError::RateLimited { retry_after },
        "server_is_overloaded" | "overloaded" => ModelError::Overloaded { retry_after },
        "server_error" | "internal_error" => ModelError::Transient(message(error)),
        c if c.starts_with("subscription_sharing_") && c.ends_with("_unavailable") => {
            ModelError::Transient(message(error))
        }
        c if c.ends_with("_user_not_eligible") || c.starts_with("chatpass_v2_") || c.ends_with("_invalid_user") => {
            ModelError::Auth(format!(
                "{}. ChatGPT plan use is not available for this sign-in; sign in again with `eos-agent login chatgpt`",
                message(error)
            ))
        }
        c if c.ends_with("_unsupported_capability") || c.ends_with("_route_not_supported") || c == "invalid_prompt" => {
            ModelError::Invalid(message(error))
        }
        _ => return None,
    })
}

fn header<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name).and_then(|v| v.to_str().ok())
}

/// "…Please try again in 11.054s." / "…in 250ms."
fn retry_hint(message: &str) -> Option<Duration> {
    let rest = &message[message.find("try again in ")? + "try again in ".len()..];
    let number: String = rest.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    let value: f64 = number.parse().ok()?;
    let unit = &rest[number.len()..];
    Some(if unit.starts_with("ms") { Duration::from_secs_f64(value / 1000.0) } else { Duration::from_secs_f64(value) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(code: u16, body: &str) -> ModelError {
        from_status(code, &HeaderMap::new(), body, None)
    }

    #[test]
    fn documented_codes_map_as_recovery_needs() {
        let e = |code: &str| format!(r#"{{"error":{{"code":"{code}","message":"m","param":"tools"}}}}"#);
        assert!(matches!(
            status(429, &e("subscription_sharing_usage_limit_exceeded")),
            ModelError::LimitReached { info } if info.status.as_deref() == Some("rejected") && info.resets_at.is_none()
        ));
        assert!(matches!(status(503, &e("subscription_sharing_usage_unavailable")), ModelError::Transient(_)));
        assert!(matches!(status(403, &e("subscription_sharing_user_not_eligible")), ModelError::Auth(_)));
        assert!(matches!(status(403, &e("chatpass_v2_region")), ModelError::Auth(_)));
        assert!(matches!(status(401, &e("subscription_sharing_invalid_user")), ModelError::Auth(_)));
        assert!(
            matches!(status(400, &e("subscription_sharing_unsupported_capability")), ModelError::Invalid(m) if m.contains("param: tools"))
        );
        assert!(matches!(status(400, &e("context_length_exceeded")), ModelError::ContextWindowExceeded(_)));
        assert!(matches!(status(503, r#"{"detail":"Service unavailable"}"#), ModelError::Transient(_)));
        assert!(matches!(status(401, r#"{"detail":{"code":"token_expired","message":"x"}}"#), ModelError::Auth(_)));
        assert!(matches!(status(400, "not json"), ModelError::Invalid(_)));
    }

    #[test]
    fn retry_hints_come_from_the_header_or_the_message() {
        let mut h = HeaderMap::new();
        h.insert("retry-after", "3".parse().expect("header"));
        assert!(matches!(
            from_status(429, &h, "{}", None),
            ModelError::RateLimited { retry_after: Some(d) } if d == Duration::from_secs(3)
        ));
        let failed = serde_json::json!({"code":"rate_limit_exceeded","message":"Please try again in 11.054s. Visit…"});
        assert!(matches!(
            from_error_object(&failed),
            ModelError::RateLimited { retry_after: Some(d) } if d == Duration::from_secs_f64(11.054)
        ));
        let other = serde_json::json!({"code":"cyber_policy","message":"flagged"});
        assert!(matches!(from_error_object(&other), ModelError::Api { kind, .. } if kind == "cyber_policy"));
        let reached = serde_json::json!({"type":"usage_limit_reached","resets_at":1794416400u64});
        assert!(
            matches!(from_error_object(&reached), ModelError::LimitReached { info } if info.resets_at == Some(1_794_416_400))
        );
    }
}
