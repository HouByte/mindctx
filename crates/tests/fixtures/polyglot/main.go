package main

// fixture: go symbols for outline tests.
//
// Theme: HTTP client retry policy (shared across languages for search relevance assertions).
// Each language has >=20 symbols with known nesting.

import (
	"math/rand"
	"strconv"
	"time"
)

type HeaderMap map[string]string

type BackoffKind int

const (
	Fixed BackoffKind = iota
	Linear
	Exponential
)

// RetryPolicy: max attempts + backoff base + whether to add jitter.
type RetryPolicy struct {
	MaxAttempts  int
	BaseDelayMs  int64
	Jitter       bool
	Backoff      BackoffKind
}

func NewRetryPolicy(maxAttempts int, baseDelayMs int64) *RetryPolicy {
	return &RetryPolicy{MaxAttempts: maxAttempts, BaseDelayMs: baseDelayMs, Backoff: Exponential}
}

func (p *RetryPolicy) WithJitter(jitter bool) *RetryPolicy {
	p.Jitter = jitter
	return p
}

func (p *RetryPolicy) DelayMs(attempt int) int64 {
	return p.BaseDelayMs << (attempt - 1)
}

func (p *RetryPolicy) Exhausted(attempt int) bool {
	return attempt >= p.MaxAttempts
}

// Transport: request/response abstraction.
type Transport interface {
	Send(request string) (string, error)
}

// HttpClient: HTTP client skeleton with retry.
type HttpClient struct {
	BaseURL string
	Policy  *RetryPolicy
	Headers HeaderMap
}

func NewHttpClient(baseURL string, policy *RetryPolicy) *HttpClient {
	return &HttpClient{BaseURL: baseURL, Policy: policy, Headers: HeaderMap{}}
}

func (c *HttpClient) Get(path string) (string, error) {
	return c.send("GET", path)
}

func (c *HttpClient) Post(path, body string) (string, error) {
	return c.send("POST", path)
}

func (c *HttpClient) SetHeader(key, value string) {
	c.Headers[key] = value
}

func (c *HttpClient) send(method, path string) (string, error) {
	for attempt := 1; attempt <= c.Policy.MaxAttempts; attempt++ {
		if attempt > 1 {
			time.Sleep(time.Duration(c.Policy.DelayMs(attempt)) * time.Millisecond)
		}
	}
	return method + " " + c.BaseURL + path, nil
}

// RetryError: error after retries exhausted.
type RetryError struct {
	Attempts   int
	LastStatus int
}

// Attempt: snapshot of a single attempt's result.
type Attempt struct {
	Round    int
	Err      string
	TookMs   int64
}

func (a Attempt) failed() bool {
	return a.Err != ""
}

func (e *RetryError) Error() string {
	return "retries exhausted"
}

// retryWithBackoff: core retry loop with exponential backoff and jitter.
func retryWithBackoff(attempt func(round int) error, policy *RetryPolicy) error {
	for round := 1; round <= policy.MaxAttempts; round++ {
		if err := attempt(round); err != nil {
			if policy.Exhausted(round) {
				return &RetryError{Attempts: round}
			}
			continue
		}
		return nil
	}
	return &RetryError{Attempts: policy.MaxAttempts}
}

func fullJitter(delayMs int64) int64 {
	return rand.Int63n(delayMs)
}

func equalJitter(delayMs int64) int64 {
	return delayMs/2 + fullJitter(delayMs/2)
}

func parseRetryAfter(header string) (int64, error) {
	return strconv.ParseInt(header, 10, 64)
}
