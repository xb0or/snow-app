import assert from "node:assert/strict";
import test from "node:test";
import {
  isRetryableWebDavRequest,
  isRetryableWebDavStatus,
  webDavRetryDelayMs,
} from "./webdavRetry";

test("WebDAV retry policy retries transient idempotent requests", () => {
  assert.equal(isRetryableWebDavRequest("GET", { retryable: true }), true);
  assert.equal(isRetryableWebDavRequest("PUT", { retryable: true }), true);
  assert.equal(isRetryableWebDavRequest("PROPFIND", new TypeError("offline")), true);
});

test("WebDAV retry policy rejects conflicts, auth failures and unsafe methods", () => {
  assert.equal(isRetryableWebDavStatus(409), false);
  assert.equal(isRetryableWebDavStatus(401), false);
  assert.equal(isRetryableWebDavStatus(503), true);
  assert.equal(isRetryableWebDavRequest("GET", { retryable: false }), false);
  assert.equal(isRetryableWebDavRequest("POST", { retryable: true }), false);
});

test("WebDAV retry delay backs off and caps Retry-After", () => {
  assert.deepEqual([1, 2, 3, 4].map((attempt) => webDavRetryDelayMs(attempt)), [250, 500, 1_000, 2_000]);
  assert.equal(webDavRetryDelayMs(1, 45_000), 30_000);
  assert.equal(webDavRetryDelayMs(1, -10), 0);
});
