import { describe, it } from "node:test";
import assert from "node:assert/strict";
import { createZipArchive, readZipArchive } from "./zipArchive";

describe("data-management ZIP containers", () => {
  it("round-trips entries and validates extraction limits", () => {
    const archive = createZipArchive([
      { name: "manifest.json", data: Buffer.from('{"formatVersion":1}') },
      { name: "database/snowapp.db", data: Buffer.from("sqlite-bytes") },
    ]);
    const entries = readZipArchive(archive);
    assert.equal(entries.get("manifest.json")?.toString("utf8"), '{"formatVersion":1}');
    assert.equal(entries.get("database/snowapp.db")?.toString("utf8"), "sqlite-bytes");
    assert.throws(
      () => readZipArchive(archive, { maxEntryBytes: 4 }),
      /extraction limits/
    );
  });

  it("rejects path traversal and duplicate entries", () => {
    assert.throws(
      () => createZipArchive([{ name: "../outside", data: Buffer.from("x") }]),
      /Unsafe ZIP entry path/
    );
    assert.throws(
      () => createZipArchive([
        { name: "same", data: Buffer.from("one") },
        { name: "same", data: Buffer.from("two") },
      ]),
      /Duplicate ZIP entry/
    );
  });
});
