// fixture: typescript symbols for outline tests.
//
// Theme: HTTP client retry policy (shared across languages for search relevance assertions).
// Each language has >=20 symbols with known nesting.

export interface RetryOptions {
  maxAttempts: number;
  baseDelayMs: number;
  jitter: boolean;
}

export interface Transport {
  send(request: string): Promise<string>;
}

export type HeaderMap = Record<string, string>;

export type FetchLike = (url: string, init?: HeaderMap) => Promise<string>;

export enum BackoffKind {
  Fixed = "fixed",
  Linear = "linear",
  Exponential = "exponential",
}

export class RetryPolicy {
  constructor(
    public maxAttempts: number = 3,
    public baseDelayMs: number = 100,
    public jitter: boolean = true,
  ) {}

  withJitter(jitter: boolean): this {
    this.jitter = jitter;
    return this;
  }

  delayMs(attempt: number): number {
    return this.baseDelayMs * 2 ** (attempt - 1);
  }
}

export class HttpClient {
  private headers: HeaderMap = {};

  constructor(
    public baseUrl: string,
    public policy: RetryPolicy = new RetryPolicy(),
  ) {}

  get(path: string): Promise<string> {
    return this.send("GET", path);
  }

  post(path: string, body: string): Promise<string> {
    return this.send("POST", path, body);
  }

  setHeader(key: string, value: string): void {
    this.headers[key] = value;
  }

  private async send(method: string, path: string, body?: string): Promise<string> {
    for (let attempt = 1; attempt <= this.policy.maxAttempts; attempt++) {
      if (attempt > 1) {
        await delay(this.policy.delayMs(attempt));
      }
      return `${method} ${this.baseUrl}${path}`;
    }
    throw new RetryError(this.policy.maxAttempts);
  }
}

export class RetryError extends Error {
  constructor(
    public attempts: number,
    public lastStatus?: number,
  ) {
    super(`retries exhausted after ${attempts}`);
  }
}

export namespace RetryUtils {
  export function parseRetryAfter(header?: string): number | null {
    const value = header?.trim();
    return value ? Number(value) : null;
  }

  export function fullJitter(delayMs: number): number {
    return Math.floor(Math.random() * delayMs);
  }
}

export function retryWithBackoff<T>(
  attempt: (round: number) => Promise<T>,
  options: RetryOptions,
): Promise<T> {
  return (async () => {
    for (let round = 1; round <= options.maxAttempts; round++) {
      try {
        return await attempt(round);
      } catch (err) {
        if (round === options.maxAttempts) throw err;
      }
    }
    throw new RetryError(options.maxAttempts);
  })();
}

function delay(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
