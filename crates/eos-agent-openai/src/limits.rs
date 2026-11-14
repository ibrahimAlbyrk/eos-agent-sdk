//! Plan-limit headers, when the server sends them (Codex's `x-codex-*`
//! family), as [`RateLimitInfo`]: a window up to six hours long reads as the
//! five-hour window, a longer one as the weekly window.

use eos_agent_core::RateLimitInfo;
use reqwest::header::HeaderMap;

/// Usage at or above this share of a window is reported as a warning.
const WARNING_AT: f64 = 0.8;

pub(crate) fn from_headers(h: &HeaderMap) -> Option<RateLimitInfo> {
    let get = |name: &str| h.get(name).and_then(|v| v.to_str().ok());
    let window = |which: &str| {
        let used = get(&format!("x-codex-{which}-used-percent"))?.parse::<f64>().ok()?;
        let minutes = get(&format!("x-codex-{which}-window-minutes")).and_then(|v| v.parse::<u64>().ok());
        let resets = get(&format!("x-codex-{which}-reset-at")).and_then(|v| v.parse::<u64>().ok());
        Some((used / 100.0, minutes, resets))
    };
    let windows: Vec<_> = ["primary", "secondary"].into_iter().filter_map(window).collect();
    if windows.is_empty() {
        return None;
    }
    let mut info = RateLimitInfo::default();
    let mut binding: Option<(f64, &str, Option<u64>)> = None;
    for (used, minutes, resets) in windows {
        let name = if minutes.is_some_and(|m| m <= 6 * 60) { "five_hour" } else { "seven_day" };
        let slot = if name == "five_hour" { &mut info.five_hour_utilization } else { &mut info.seven_day_utilization };
        *slot = Some(slot.map_or(used, |u: f64| u.max(used)));
        if binding.is_none_or(|(u, _, _)| used > u) {
            binding = Some((used, name, resets));
        }
    }
    let (used, claim, resets) = binding?;
    info.claim = Some(claim.into());
    info.resets_at = resets;
    let reached = get("x-codex-rate-limit-reached-type").is_some_and(|v| !v.is_empty()) || used >= 1.0;
    info.status = Some(
        if reached {
            "rejected"
        } else if used >= WARNING_AT {
            "allowed_warning"
        } else {
            "allowed"
        }
        .into(),
    );
    Some(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_by_length() {
        let mut h = HeaderMap::new();
        for (k, v) in [
            ("x-codex-primary-used-percent", "12.5"),
            ("x-codex-primary-window-minutes", "300"),
            ("x-codex-secondary-used-percent", "85"),
            ("x-codex-secondary-window-minutes", "10080"),
            ("x-codex-secondary-reset-at", "1794416400"),
        ] {
            h.insert(k, v.parse().expect("header"));
        }
        let info = from_headers(&h).expect("info");
        assert_eq!(info.five_hour_utilization, Some(0.125));
        assert_eq!(info.seven_day_utilization, Some(0.85));
        assert_eq!(info.claim.as_deref(), Some("seven_day"));
        assert_eq!(info.resets_at, Some(1_794_416_400));
        assert_eq!(info.status.as_deref(), Some("allowed_warning"));
        assert!(from_headers(&HeaderMap::new()).is_none());
    }
}
