import assert from "node:assert/strict";
import { describe, it } from "node:test";
import { decideSyncAction, type SyncDecisionInput } from "./syncDecision";

const baseline = (patch: Partial<SyncDecisionInput> = {}): SyncDecisionInput => ({
  localContentHash: "base",
  localMode: "config",
  baseRevision: 4,
  baseLocalHash: "base",
  baseRemoteHash: "base",
  remoteRevision: 4,
  remoteContentHash: "base",
  remoteMode: "config",
  ...patch,
});

describe("WebDAV sync decisions", () => {
  it("requires a choice on first contact when local and remote content differ", () => {
    assert.equal(decideSyncAction(baseline({
      baseRevision: 0,
      baseLocalHash: "",
      baseRemoteHash: "",
      localContentHash: "local",
      remoteContentHash: "remote",
    })), "conflict");
    assert.equal(decideSyncAction(baseline({
      baseRevision: 0,
      baseLocalHash: "",
      baseRemoteHash: "",
    })), "none");
  });

  it("detects local-only and remote-only changes", () => {
    assert.equal(decideSyncAction(baseline({ localContentHash: "local" })), "upload");
    assert.equal(decideSyncAction(baseline({
      remoteRevision: 5,
      remoteContentHash: "remote",
    })), "download");
  });

  it("pauses for divergent edits and sync-mode changes", () => {
    assert.equal(decideSyncAction(baseline({
      localContentHash: "local",
      remoteRevision: 5,
      remoteContentHash: "remote",
    })), "conflict");
    assert.equal(decideSyncAction(baseline({ remoteMode: "mirror" })), "conflict");
  });

  it("recognizes converged content even when both revisions moved", () => {
    assert.equal(decideSyncAction(baseline({
      localContentHash: "same-new",
      remoteRevision: 5,
      remoteContentHash: "same-new",
    })), "none");
  });
});
