import assert from "node:assert/strict";
import test from "node:test";
import {
  DataManagementTaskBusyError,
  DataManagementTaskCancelledError,
  DataManagementTaskCoordinator,
} from "./taskCoordinator";

test("data management tasks are mutually exclusive", async () => {
  const coordinator = new DataManagementTaskCoordinator();
  let release!: () => void;
  const started = new Promise<void>((resolve) => {
    release = resolve;
  });

  const first = coordinator.run("backup-create", async () => {
    await started;
    return "first";
  });

  const active = coordinator.getActiveTask();
  assert.ok(active);
  assert.equal(active.operation, "backup-create");
  await assert.rejects(
    coordinator.run("sync", async () => "second"),
    DataManagementTaskBusyError
  );

  release();
  assert.equal(await first, "first");
  assert.equal(coordinator.getActiveTask(), null);
});

test("cancelling a task aborts its context and reports cancellation", async () => {
  const coordinator = new DataManagementTaskCoordinator();
  let resolveAbort!: () => void;
  const abortObserved = new Promise<void>((resolve) => {
    resolveAbort = resolve;
  });

  const task = coordinator.run("sync", async ({ signal }) => {
    await new Promise<void>((resolve) => {
      signal.addEventListener(
        "abort",
        () => {
          resolveAbort();
          resolve();
        },
        { once: true }
      );
    });
  });

  const active = coordinator.getActiveTask();
  assert.ok(active);
  assert.equal(coordinator.cancel(active.taskId), true);
  await abortObserved;
  await assert.rejects(task, DataManagementTaskCancelledError);
  assert.equal(coordinator.getActiveTask(), null);
});

test("progress listeners receive task lifecycle updates", async () => {
  const coordinator = new DataManagementTaskCoordinator();
  const statuses: string[] = [];
  const unsubscribe = coordinator.subscribe((progress) => {
    statuses.push(progress.status);
  });

  await coordinator.run("config-export", async ({ report }) => {
    report({ phase: "hashing", completed: 1, total: 2 });
  });

  unsubscribe();
  assert.deepEqual(statuses, ["running", "running", "completed"]);
});
