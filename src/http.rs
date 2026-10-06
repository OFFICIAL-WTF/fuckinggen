use std::time::Duration;

/// Agent tuned for long, streamed image generations: connect/write are quick,
/// reads may idle up to 90s between SSE events before we give up.
pub fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(20))
        .timeout_read(Duration::from_secs(90))
        .timeout_write(Duration::from_secs(30))
        .build()
}

/// Human-readable one-liner for any ureq failure, including the response body
/// for HTTP status errors (the interesting part for this backend).
pub fn describe_error(err: ureq::Error) -> String {
    match err {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            let snippet: String = body.trim().chars().take(400).collect();
            if snippet.is_empty() {
                format!("HTTP {code}")
            } else {
                format!("HTTP {code}: {snippet}")
            }
        }
        other => format!("network error: {other}"),
    }
}
