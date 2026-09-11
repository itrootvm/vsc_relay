#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    Transient,
    RateLimit,
    Terminal,
}

pub fn classify_error(text: &str) -> ErrorClass {
    let t = text.to_lowercase();

    let terminal = [
        "401",
        "403",
        "forbidden",
        "insufficient permissions",
        "not logged in",
        "invalid api key",
        "oauth token",
        "login expired",
        "authentication_error",
        "credit balance is too low",
        "insufficient_quota",
        "exceeded your current quota",
        "prompt is too long",
        "request too large",
        "conversation too long",
        "context_length_exceeded",
        "maximum context length",
        "usage policy",
        "safety measures",
        "is not available with",
        "400 bad request",
        "certificate",
    ];
    if terminal.iter().any(|k| t.contains(k)) {
        return ErrorClass::Terminal;
    }

    let rate_limit = [
        "you've hit your",
        "usage limit",
        "rate limit",
        "429",
        "temporarily limiting requests",
        "too many requests",
    ];
    if rate_limit.iter().any(|k| t.contains(k)) {
        return ErrorClass::RateLimit;
    }

    let transient = [
        "the response above may be incomplete",
        "mid-response",
        "mid-stream",
        "500 internal server",
        "internal server error",
        "529",
        "overloaded",
        "502",
        "503",
        "504",
        "timed out",
        "econnreset",
        "econnrefused",
        "etimedout",
        "fetch failed",
        "unable to connect",
        "stream disconnected",
        "stream closed",
        "stream error",
        "terminated early due to an api error",
    ];
    if transient.iter().any(|k| t.contains(k)) {
        return ErrorClass::Transient;
    }

    ErrorClass::Terminal
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_mid_response_and_network() {
        for s in [
            "API Error: Server error mid-response. The response above may be incomplete.",
            "API Error: Connection closed mid-response. The response above may be incomplete.",
            "API Error: Response stalled mid-stream. The response above may be incomplete.",
            "API Error: 500 Internal server error. This is a server-side issue, usually temporary",
            "API Error: Repeated 529 Overloaded errors. The API is at capacity",
            "Unable to connect to API (ECONNRESET)",
            "Request timed out",
            "stream disconnected before completion: stream closed before response.complete",
            "Agent terminated early due to an API error",
        ] {
            assert_eq!(classify_error(s), ErrorClass::Transient, "{s}");
        }
    }

    #[test]
    fn rate_limit_class() {
        for s in [
            "You've hit your session limit · resets 3:45pm",
            "You've hit your weekly limit · resets Mon 12:00am",
            "API Error: Request rejected (429)",
            "API Error: Server is temporarily limiting requests (not your usage limit)",
            "🖐 You've hit your usage limit. Upgrade to Pro",
            "Rate limit reached for o4-mini",
        ] {
            assert_eq!(classify_error(s), ErrorClass::RateLimit, "{s}");
        }
    }

    #[test]
    fn terminal_never_retry() {
        for s in [
            "API Error 403: Forbidden. Insufficient permissions.",
            "API Error: 401 authentication_error",
            "Not logged in · Please run /login",
            "Invalid API key · Fix external API key",
            "Credit balance is too low",
            "Prompt is too long",
            "Request too large (max 30 MB).",
            "API Error: Claude Code is unable to respond to this request, which appears to violate our Usage Policy",
            "Claude Opus is not available with the Claude Pro plan",
            "stream error: exceeded retry limit, last status: 401 Unauthorized",
        ] {
            assert_eq!(classify_error(s), ErrorClass::Terminal, "{s}");
        }
    }

    #[test]
    fn transient_500_with_try_again_is_not_rate_limit() {
        let s = "API Error: 500 Internal server error. This is a server-side issue, usually temporary — try again in a moment.";
        assert_eq!(classify_error(s), ErrorClass::Transient);
    }

    #[test]
    fn unknown_defaults_terminal() {
        assert_eq!(
            classify_error("something totally unexpected"),
            ErrorClass::Terminal
        );
    }
}
