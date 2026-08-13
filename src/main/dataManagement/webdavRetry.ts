const RETRYABLE_METHODS = new Set(["DELETE", "GET", "HEAD", "MKCOL", "PROPFIND", "PUT"]);

type RetryableError = {
  retryable?: unknown;
};

export const isRetryableWebDavStatus = (status: number): boolean =>
  status === 408 || status === 423 || status === 429 || status >= 500;

export const webDavRetryDelayMs = (
  failedAttempt: number,
  retryAfterMs: number | null = null
): number => {
  if (retryAfterMs !== null && Number.isFinite(retryAfterMs)) {
    return Math.max(0, Math.min(retryAfterMs, 30_000));
  }
  return Math.min(250 * (2 ** Math.max(0, failedAttempt - 1)), 2_000);
};

export const isRetryableWebDavRequest = (
  method: string,
  error: unknown
): boolean => {
  if (!RETRYABLE_METHODS.has(method.toUpperCase())) return false;
  if (error instanceof TypeError || error instanceof DOMException) return true;
  return typeof error === "object" && error !== null &&
    (error as RetryableError).retryable === true;
};
