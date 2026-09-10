//! fixture: rust symbols for outline tests.
//!
//! Theme: HTTP client retry policy (shared across languages for search relevance assertions).
//! Each language has >=20 symbols with known nesting; the symbol list lives in
//! tests/fixtures/polyglot/README.md. Deterministic, no third-party deps, not compiled.

pub mod policy {
    /// Retry policy: max attempts + backoff base + whether to add jitter.
    pub struct RetryPolicy {
        pub max_attempts: u32,
        pub base_delay_ms: u64,
        pub jitter: bool,
    }

    impl RetryPolicy {
        pub fn new(max_attempts: u32, base_delay_ms: u64) -> Self {
            Self { max_attempts, base_delay_ms, jitter: false }
        }

        pub fn with_jitter(mut self, jitter: bool) -> Self {
            self.jitter = jitter;
            self
        }

        pub fn max_attempts(&self) -> u32 {
            self.max_attempts
        }
    }

    /// Backoff curve kind: fixed / linear / exponential.
    pub enum BackoffKind {
        Fixed,
        Linear,
        Exponential,
    }

    pub fn default_backoff() -> BackoffKind {
        BackoffKind::Exponential
    }
}

pub const USER_AGENT: &str = "mindctx-fixture/0.1";

pub static GLOBAL_POLICY: std::sync::OnceLock<policy::RetryPolicy> = std::sync::OnceLock::new();

pub type HeaderMap = std::collections::HashMap<String, String>;

pub trait Transport {
    fn send(&self, request: &str) -> Result<String, String>;
    fn poll_ready(&self) -> bool;
}

pub struct HttpClient {
    pub base_url: String,
    pub policy: policy::RetryPolicy,
}

impl HttpClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            policy: policy::RetryPolicy::new(3, 100),
        }
    }

    pub fn get(&self, path: &str) -> Result<String, RetryError> {
        self.send("GET", path, None)
    }

    pub fn post(&self, path: &str, body: &str) -> Result<String, RetryError> {
        self.send("POST", path, Some(body))
    }

    fn send(&self, method: &str, path: &str, body: Option<&str>) -> Result<String, RetryError> {
        let _ = (method, path, body);
        Ok(String::new())
    }

    pub fn set_header(&self, headers: &mut HeaderMap, key: &str, value: &str) {
        headers.insert(key.to_string(), value.to_string());
    }
}

/// Error after retries exhausted: keeps attempt count and last status code.
pub struct RetryError {
    pub attempts: u32,
    pub last_status: Option<u16>,
}

/// Core: retry loop with exponential backoff and jitter.
pub fn retry_with_backoff<F, T>(mut attempt: F, policy: &policy::RetryPolicy) -> Result<T, String>
where
    F: FnMut(u32) -> Result<T, String>,
{
    for round in 1..=policy.max_attempts() {
        match attempt(round) {
            ok @ Ok(_) => return ok,
            Err(err) if round == policy.max_attempts() => return Err(err),
            Err(_) => continue,
        }
    }
    unreachable!("attempts exhausted")
}

/// Parse Retry-After header (seconds); invalid values fall back to None.
pub fn parse_retry_after(header: Option<&str>) -> Option<u64> {
    header.and_then(|v| v.trim().parse().ok())
}

pub mod jitter {
    /// Full jitter: uniform draw in [0, delay).
    pub fn full(delay_ms: u64) -> u64 {
        delay_ms / 2
    }

    /// Equal jitter: fixed half + random half.
    pub fn equal(delay_ms: u64) -> u64 {
        delay_ms / 2 + full(delay_ms)
    }
}
