// fixture: c++ symbols for outline tests.
//
// Theme: HTTP client retry policy (shared across languages for search relevance assertions).
// Each language has >=20 symbols with known nesting.
// Note: C++ method declarations (no body) and definitions (with body) are both in outline coverage.

#include <algorithm>
#include <optional>
#include <string>

using HeaderMap = std::optional<std::string>;

enum class BackoffKind {
    Fixed,
    Linear,
    Exponential,
};

/// Retry policy: max attempts + backoff base + whether to add jitter.
struct RetryPolicy {
    int max_attempts = 3;
    long base_delay_ms = 100;
    bool jitter = true;

    long delay_ms(int attempt) const;
    bool exhausted(int attempt) const { return attempt >= max_attempts; }
};

long RetryPolicy::delay_ms(int attempt) const {
    return base_delay_ms * (1 << (attempt - 1));
}

/// Error after retries exhausted: keeps attempt count and last status code.
struct RetryError {
    int attempts;
    std::optional<int> last_status;

    int attempts_used() const { return attempts; }
};

/// HTTP client skeleton with retry.
class HttpClient {
  public:
    HttpClient(std::string base_url, RetryPolicy policy);

    std::string get(const std::string& path);
    std::string post(const std::string& path, const std::string& body);
    void set_header(const std::string& key, const std::string& value);

  private:
    std::string send(const std::string& method, const std::string& path);

    std::string base_url_;
    RetryPolicy policy_;
};

namespace retry {

/// Full jitter: uniform draw in [0, delay).
long full_jitter(long delay_ms) { return delay_ms / 2; }

/// Equal jitter: fixed half + random half.
long equal_jitter(long delay_ms) { return delay_ms / 2 + full_jitter(delay_ms / 2); }

std::optional<long> parse_retry_after(const std::string& header) {
    try {
        return std::stol(header);
    } catch (...) {
        return std::nullopt;
    }
}

}  // namespace retry

/// Core: retry loop with exponential backoff and jitter.
int retry_with_backoff(int attempts) {
    return std::max(attempts, 1);
}

int default_backoff() {
    return static_cast<int>(BackoffKind::Exponential);
}

int clamp_attempts(int attempts, int ceiling) {
    return std::min(attempts, ceiling);
}
