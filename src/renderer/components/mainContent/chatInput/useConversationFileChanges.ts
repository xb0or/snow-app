import { useEffect, useMemo, useState } from "react";
import type {
  ChatConversationMessage,
  FileChangeRecord,
} from "../chatMessages/utils/conversationTypes";

type UseConversationFileChangesParams = {
  baselineCheckpointId?: string;
  workDir?: string;
  messages: ChatConversationMessage[];
  conversationVersion: number;
  fallbackChanges: FileChangeRecord[];
};

type CheckpointDiffs = Awaited<
  ReturnType<typeof window.snow.listCheckpointDiffs>
>;

type CheckpointDiffState = {
  requestKey: string;
  diffs: CheckpointDiffs | null;
};

const normalizePath = (filePath: string): string =>
  filePath.replaceAll("\\", "/").replace(/^\.\/+/, "").toLowerCase();

const findFallbackChange = (
  checkpointPath: string,
  fallbackChanges: FileChangeRecord[]
): FileChangeRecord | undefined => {
  const normalizedCheckpointPath = normalizePath(checkpointPath);
  return fallbackChanges.find((change) => {
    const normalizedToolPath = normalizePath(change.filePath);
    return (
      normalizedToolPath === normalizedCheckpointPath ||
      normalizedToolPath.endsWith(`/${normalizedCheckpointPath}`)
    );
  });
};

const toFileChangeKind = (
  changeType: string
): FileChangeRecord["kind"] => {
  if (changeType === "added") {
    return "create";
  }
  if (changeType === "deleted") {
    return "delete";
  }
  return "edit";
};

/**
 * Returns the conversation's file modifications: the net workspace diff from
 * its first local checkpoint, merged with the tool-recorded statistics.
 *
 * The checkpoint view reports every captured file (includeAll=true), so later
 * runs drifting the shared working tree cannot erase an earlier
 * conversation's modifications; tool-recorded changes that the checkpoint
 * does not cover (deleted checkpoints, capture failures, SSH workspaces) are
 * appended from the fallback statistics. Remote workspaces and unavailable
 * checkpoint APIs retain the tool-recorded approximation so SSH statistics
 * continue to work.
 */
export const useConversationFileChanges = ({
  baselineCheckpointId,
  workDir,
  messages,
  conversationVersion,
  fallbackChanges,
}: UseConversationFileChangesParams): FileChangeRecord[] => {
  const completedToolSignature = useMemo(
    () =>
      messages
        .flatMap((message) => message.toolCalls ?? [])
        .filter((toolCall) => toolCall.status === "completed")
        .map(
          (toolCall) =>
            `${toolCall.interactionId}:${toolCall.status}:${toolCall.result?.length ?? 0}`
        )
        .join("|"),
    [messages]
  );

  const canUseCheckpoint = Boolean(baselineCheckpointId && workDir);
  const requestKey = JSON.stringify([
    baselineCheckpointId ?? "",
    workDir ?? "",
    completedToolSignature,
    conversationVersion,
  ]);
  const [checkpointState, setCheckpointState] =
    useState<CheckpointDiffState>({ requestKey: "", diffs: null });

  useEffect(() => {
    if (!canUseCheckpoint || !baselineCheckpointId || !workDir) {
      return;
    }

    let cancelled = false;
    // includeAll=true: report every captured entry, including files whose
    // current state drifted from the checkpoint's post-change state (later
    // runs in a shared working tree). Without it, listCheckpointDiffs drops
    // those files and the panel loses this conversation's modifications.
    void window.snow
      .listCheckpointDiffs(baselineCheckpointId, workDir, true)
      .then((diffs) => {
        if (!cancelled) {
          setCheckpointState({ requestKey, diffs });
        }
      })
      .catch(() => {
        if (!cancelled) {
          setCheckpointState({ requestKey, diffs: null });
        }
      });

    return () => {
      cancelled = true;
    };
  }, [
    baselineCheckpointId,
    canUseCheckpoint,
    completedToolSignature,
    conversationVersion,
    requestKey,
    workDir,
  ]);

  return useMemo(() => {
    if (
      !canUseCheckpoint ||
      checkpointState.requestKey !== requestKey ||
      checkpointState.diffs === null
    ) {
      return fallbackChanges;
    }

    const checkpointChanges: FileChangeRecord[] = checkpointState.diffs.map(
      (diff, index) => {
        const fallback = findFallbackChange(diff.path, fallbackChanges);
        return {
          filePath: diff.path,
          kind: toFileChangeKind(diff.changeType),
          agent: fallback?.agent ?? "main",
          subAgentName: fallback?.subAgentName,
          timestamp: fallback?.timestamp ?? index,
          diff: {
            patch: diff.content,
            isBinary: diff.isBinary,
          },
        };
      }
    );

    // A successful but empty checkpoint result is ambiguous: the baseline
    // checkpoint may have been deleted (rollback, compaction cleanup,
    // new-chat pruning) — listCheckpointDiffs returns an empty list for
    // missing manifests instead of an error. Also, tool-recorded changes for
    // files the checkpoint never captured (capture failures, SSH fallbacks)
    // are absent from the checkpoint view. Append those fallback records so
    // the panel never loses tool-recorded modifications; checkpoint diffs
    // keep precedence for files covered by both sources.
    const checkpointPaths = new Set(
      checkpointChanges.map((change) => normalizePath(change.filePath))
    );
    const missingFallback = fallbackChanges.filter(
      (change) => !checkpointPaths.has(normalizePath(change.filePath))
    );
    return [...checkpointChanges, ...missingFallback].sort(
      (left, right) => left.timestamp - right.timestamp
    );
  }, [
    canUseCheckpoint,
    checkpointState,
    fallbackChanges,
    requestKey,
  ]);
};
