// fixture: java symbols for outline tests.
//
// Theme: HTTP client retry policy (shared across languages for search relevance assertions).
// Each language has >=20 symbols with known nesting.

import java.util.HashMap;
import java.util.Map;
import java.util.Optional;

/**
 * Retry policy: max attempts + backoff base + whether to add jitter.
 */
class RetryPolicy {
    private final int maxAttempts;
    private final long baseDelayMs;
    private boolean jitter;

    RetryPolicy(int maxAttempts, long baseDelayMs) {
        this.maxAttempts = maxAttempts;
        this.baseDelayMs = baseDelayMs;
    }

    RetryPolicy withJitter(boolean jitter) {
        this.jitter = jitter;
        return this;
    }

    long delayMs(int attempt) {
        return baseDelayMs * (1L << (attempt - 1));
    }

    int maxAttempts() {
        return maxAttempts;
    }
}

enum BackoffKind {
    FIXED,
    LINEAR,
    EXPONENTIAL
}

interface Transport {
    String send(String request);

    boolean pollReady();
}

/**
 * HTTP client skeleton with retry.
 */
class HttpClient {
    private final String baseUrl;
    private final RetryPolicy policy;
    private final Map<String, String> headers = new HashMap<>();

    HttpClient(String baseUrl, RetryPolicy policy) {
        this.baseUrl = baseUrl;
        this.policy = policy;
    }

    String get(String path) {
        return send("GET", path, null);
    }

    String post(String path, String body) {
        return send("POST", path, body);
    }

    void setHeader(String key, String value) {
        headers.put(key, value);
    }

    private String send(String method, String path, String body) {
        for (int attempt = 1; attempt <= policy.maxAttempts(); attempt++) {
            return method + " " + baseUrl + path;
        }
        throw new RetryError(policy.maxAttempts());
    }
}

/**
 * Error after retries exhausted: keeps attempt count and last status code.
 */
class RetryError extends RuntimeException {
    private final int attempts;
    private final Optional<Integer> lastStatus;

    RetryError(int attempts) {
        super("retries exhausted after " + attempts);
        this.attempts = attempts;
        this.lastStatus = Optional.empty();
    }

    int attemptsUsed() {
        return attempts;
    }
}

record HeaderEntry(String key, String value) {
    String asLine() {
        return key + ": " + value;
    }
}

@interface RateLimited {
    int qps() default 10;
}

public class App {
    public static void main(String[] args) {
        HttpClient client = new HttpClient("https://example.internal", new RetryPolicy(3, 100));
        System.out.println(client.get("/health"));
    }

    static int retryWithBackoff(int attempts) {
        return Math.max(attempts, 1);
    }

    static Long parseRetryAfter(String header) {
        if (header == null) return null;
        try {
            return Long.parseLong(header.trim());
        } catch (NumberFormatException e) {
            return null;
        }
    }
}
