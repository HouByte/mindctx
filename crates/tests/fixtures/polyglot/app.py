"""fixture: python symbols for outline tests.

Theme: HTTP client retry policy (shared across languages for search relevance assertions).
Each language has >=20 symbols with known nesting.
"""

import random
import time
from typing import Optional


class RetryPolicy:
    """Retry policy: max attempts + backoff base + whether to add jitter."""

    def __init__(self, max_attempts: int = 3, base_delay_ms: int = 100, jitter: bool = True) -> None:
        self.max_attempts = max_attempts
        self.base_delay_ms = base_delay_ms
        self.jitter = jitter

    def with_jitter(self, jitter: bool) -> "RetryPolicy":
        self.jitter = jitter
        return self

    def delay_ms(self, attempt: int) -> int:
        return self.base_delay_ms * (2 ** (attempt - 1))

    def max_attempts_hint(self) -> int:
        return self.max_attempts


class BackoffKind:
    FIXED = "fixed"
    LINEAR = "linear"
    EXPONENTIAL = "exponential"

    @staticmethod
    def default() -> str:
        return BackoffKind.EXPONENTIAL


class HttpClient:
    """HTTP client skeleton with retry."""

    def __init__(self, base_url: str, policy: Optional[RetryPolicy] = None) -> None:
        self.base_url = base_url
        self.policy = policy or RetryPolicy()
        self.headers: dict[str, str] = {}

    def get(self, path: str) -> str:
        return self._send("GET", path, body=None)

    def post(self, path: str, body: str) -> str:
        return self._send("POST", path, body=body)

    def set_header(self, key: str, value: str) -> None:
        self.headers[key] = value

    def _send(self, method: str, path: str, body: Optional[str]) -> str:
        for attempt in range(1, self.policy.max_attempts + 1):
            if attempt > 1:
                time.sleep(self.policy.delay_ms(attempt) / 1000)
            return f"{method} {self.base_url}{path} {body or ''}"
        return ""


class RetryError(Exception):
    """Error after retries exhausted."""

    def __init__(self, attempts: int, last_status: Optional[int] = None) -> None:
        super().__init__(f"retries exhausted after {attempts}")
        self.attempts = attempts
        self.last_status = last_status

    def attempts_used(self) -> int:
        return self.attempts


def retry_with_backoff(attempt, policy: RetryPolicy):
    """Core: retry loop with exponential backoff and jitter."""
    for round_no in range(1, policy.max_attempts + 1):
        try:
            return attempt(round_no)
        except Exception:
            if round_no == policy.max_attempts:
                raise
    raise RetryError(policy.max_attempts)


def parse_retry_after(header: Optional[str]) -> Optional[int]:
    if header is None:
        return None
    try:
        return int(header.strip())
    except ValueError:
        return None


def full_jitter(delay_ms: int) -> int:
    return random.randint(0, delay_ms)


def equal_jitter(delay_ms: int) -> int:
    return delay_ms // 2 + full_jitter(delay_ms // 2)
