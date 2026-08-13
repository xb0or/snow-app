import { describe, it } from "node:test";
import assert from "node:assert/strict";
import {
  decryptBundlePayload,
  encryptBundlePayload,
} from "./cryptoBundle";

describe("data-management encrypted payloads", () => {
  it("uses authenticated Argon2id + AES-GCM envelopes", async () => {
    const payload = Buffer.from('{"tables":{"api_configs":[]}}', "utf8");
    const encrypted = await encryptBundlePayload(payload, "correct horse battery staple");
    const envelope = JSON.parse(encrypted.toString("utf8")) as Record<string, unknown>;
    assert.equal(envelope.algorithm, "aes-256-gcm");
    assert.equal(envelope.kdf, "argon2id");
    assert.deepEqual(await decryptBundlePayload(encrypted, "correct horse battery staple"), payload);
    await assert.rejects(
      decryptBundlePayload(encrypted, "wrong password"),
      /Unable to decrypt package/
    );
  });
});
