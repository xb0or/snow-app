import { randomUUID } from "node:crypto";
import type {
  DataManagementProgress,
  DataManagementTaskOperation,
} from "../../preload/types/dataManagement";

export class DataManagementTaskBusyError extends Error {
  constructor() {
    super("Another data management task is already running");
    this.name = "DataManagementTaskBusyError";
  }
}

export class DataManagementTaskCancelledError extends Error {
  constructor() {
    super("Data management task was cancelled");
    this.name = "DataManagementTaskCancelledError";
  }
}

export type DataManagementTaskContext = {
  taskId: string;
  signal: AbortSignal;
  report: (progress: Partial<Omit<DataManagementProgress, "taskId" | "operation" | "status">>) => void;
};

type TaskListener = (progress: DataManagementProgress) => void;
type TaskWork<T> = (context: DataManagementTaskContext) => Promise<T>;

export class DataManagementTaskCoordinator {
  private activeTask: {
    progress: DataManagementProgress;
    controller: AbortController;
  } | null = null;

  private readonly listeners = new Set<TaskListener>();

  subscribe(listener: TaskListener): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  getActiveTask(): DataManagementProgress | null {
    return this.activeTask ? { ...this.activeTask.progress } : null;
  }

  cancel(taskId?: string): boolean {
    if (!this.activeTask || (taskId && this.activeTask.progress.taskId !== taskId)) {
      return false;
    }
    this.activeTask.controller.abort();
    return true;
  }

  async run<T>(
    operation: DataManagementTaskOperation,
    work: TaskWork<T>
  ): Promise<T> {
    if (this.activeTask) {
      throw new DataManagementTaskBusyError();
    }

    const controller = new AbortController();
    const progress: DataManagementProgress = {
      taskId: randomUUID(),
      operation,
      status: "running",
      phase: "starting",
      completed: 0,
      total: 0,
      currentItem: "",
      cancellable: true,
    };
    this.activeTask = { progress, controller };
    this.emit();

    const report = (
      update: Partial<
        Omit<DataManagementProgress, "taskId" | "operation" | "status">
      >
    ): void => {
      if (!this.activeTask || this.activeTask.progress.taskId !== progress.taskId) {
        return;
      }
      this.activeTask.progress = { ...this.activeTask.progress, ...update };
      this.emit();
    };

    try {
      const result = await work({
        taskId: progress.taskId,
        signal: controller.signal,
        report,
      });
      if (controller.signal.aborted) {
        throw new DataManagementTaskCancelledError();
      }
      this.setTerminalProgress(progress.taskId, {
        status: "completed",
        phase: "completed",
        cancellable: false,
      });
      this.emit();
      return result;
    } catch (error) {
      if (controller.signal.aborted || error instanceof DataManagementTaskCancelledError) {
        this.setTerminalProgress(progress.taskId, {
          status: "cancelled",
          phase: "cancelled",
          cancellable: false,
        });
        this.emit();
        throw error instanceof DataManagementTaskCancelledError
          ? error
          : new DataManagementTaskCancelledError();
      }
      this.setTerminalProgress(progress.taskId, {
        status: "failed",
        phase: "failed",
        error: error instanceof Error ? error.message : String(error),
        cancellable: false,
      });
      this.emit();
      throw error;
    } finally {
      this.activeTask = null;
    }
  }

  private emit(): void {
    const progress = this.getActiveTask();
    if (!progress) {
      return;
    }
    for (const listener of this.listeners) {
      listener(progress);
    }
  }

  private setTerminalProgress(
    taskId: string,
    update: Pick<DataManagementProgress, "status" | "phase" | "cancellable"> &
      Partial<Pick<DataManagementProgress, "error">>
  ): void {
    if (!this.activeTask || this.activeTask.progress.taskId !== taskId) {
      return;
    }
    this.activeTask.progress = { ...this.activeTask.progress, ...update };
  }
}
