import { useCallback, useState } from "react";
import type {
  ConversationContextValue,
  CheckpointFileChange,
  RollbackMode,
  RollbackTodoItem,
} from "../utils/conversationTypes";
import { PENDING_SESSION_KEY } from "../utils/conversationTypes";
import {
  deleteCheckpoints,
  directoryIdToPath,
  getErrorMessage,
  killRunningToolExecutions,
} from "../utils/conversationHelpers";

/**
 * 回滚逻辑：中止流、预览文件变更、确认/取消回滚。
 * context_compaction 回滚必须调用 truncateConversation，以其自身 responseId
 * 为起点删除边界及后续消息；不得调用 deleteConversation。
 */
export const useRollback = (ctx: ConversationContextValue) => {
  const clearDraftToRestore = useCallback((): void => {
    ctx.setDraftToRestore(null);
  }, [ctx.setDraftToRestore]);

  /** 正在计算变更（弹窗弹出前）的消息 id：SSH 下 listCheckpointChanges
   *  经 SFTP 遍历可能较慢，入口按钮在此期间显示 loading。 */
  const [preparingMessageId, setPreparingMessageId] = useState<string | null>(
    null
  );

  const handleRollback = useCallback(
    (messageId: string): void => {
      const key = ctx.activeConversationIdRef.current ?? PENDING_SESSION_KEY;

      // Abort any in-flight stream before rolling back.
      const ref = ctx.sessionsRefData.current.get(key);
      if (ref?.streamId) {
        void window.snow.abortResponseStream(ref.streamId);
        ref.streamId = null;
      }
      if (ref) {
        ref.isSending = false;
        ref.runId += 1;
      }
      // 回滚作用于会话自己的目录(而非运行时全局目录),确保 checkpoint
      // manifest.work_dir 与恢复目录一致,切换项目后仍可回滚旧会话。
      const sessionWorkDir =
        directoryIdToPath(ref?.directoryId) ?? ctx.directoryPath;
      ctx.updateSessionField(key, "isStreaming", false);
      ctx.updateSessionField(key, "streamStartedAt", 0);
      ctx.updateSessionField(key, "isAborting", false);
      // Clear the vision textify status card: the abort above may interrupt
      // the backend while it describes user images, and a stuck intermediate
      // card must not survive the rollback.
      ctx.updateSessionField(key, "visionAnalysis", undefined);
      ctx.removeStreamingId(key);

      // Cancel any in-flight summary generation so the
      // update_conversation_summary write transaction is skipped before the
      // rollback's delete/truncate runs. Without this, the summary promise may
      // still hold a database write lock and cause "database is locked".
      if (key !== PENDING_SESSION_KEY) {
        void window.snow.cancelConversationSummary(key);
      }

      const session = ctx.sessionsRef.current[key];
      if (!session) {
        return;
      }

      // Kill every in-flight bash subprocess before truncating the
      // conversation, so no orphaned OS process keeps running afterwards.
      killRunningToolExecutions(session.messages);

      const messages = session.messages;
      const targetIndex = messages.findIndex((m) => m.id === messageId);
      if (targetIndex === -1) {
        return;
      }
      // 变更计算（SSH 下经 SFTP）期间入口按钮显示 loading，弹窗弹出或
      // 失败后清除。
      setPreparingMessageId(messageId);

      const targetMessage = messages[targetIndex];
      const messageContent = targetMessage.content;
      const checkpointId = targetMessage.checkpointId;
      const convId = key !== PENDING_SESSION_KEY ? key : undefined;

      // Delete the entire conversation only when this is the true first user
      // message in the complete history. A compaction boundary and the first item
      // in a paginated window must always use range truncation instead.
      const hasUserMessageBefore = messages
        .slice(0, targetIndex)
        .some((m) => m.role === "user");
      const isFirstMessage =
        !targetMessage.isContextCompaction &&
        !session.hasMoreMessages &&
        !hasUserMessageBefore;

      // Normal user messages roll back from their following assistant response.
      // A compaction boundary is persisted as a user message with its own response id,
      // so rolling back that boundary must target the boundary row itself.
      //
      // 失败/中断轮次的 assistant 消息没有 provider responseId（持久化时
      // response_id 为空），无法用 responseId 定位截断边界。因此优先使用
      // 用户消息自身的持久化 DB ID（snowflake 数字 id，消息持久化后前端
      // id 会被替换为数据库 id）作为边界：truncateConversationFromMessage
      // 从该行开始删除该轮及之后的所有消息。找不到持久化 id（消息尚未
      // 落库）时才回退到向后寻找非空 responseId 的旧逻辑。
      let responseId = targetMessage.isContextCompaction
        ? targetMessage.responseId
        : undefined;
      let persistedMessageId: string | undefined;
      if (!targetMessage.isContextCompaction && targetMessage.id) {
        // snowflake id 是 64 位大整数（可能超过 Number.MAX_SAFE_INTEGER），
        // 这里只区分"数字 id"与前端临时 id（"user-{ts}-{rand}"），不参与
        // 数值运算，用 isInteger 即可。
        if (Number.isInteger(Number(targetMessage.id))) {
          persistedMessageId = targetMessage.id;
        }
      }
      if (!responseId && !persistedMessageId) {
        for (let i = targetIndex + 1; i < messages.length; i++) {
          if (messages[i].role === "assistant" && messages[i].responseId) {
            responseId = messages[i].responseId;
            break;
          }
        }
      }

      // Compute file changes for the confirmation dialog. This is async but
      // we set the preview state once the diff is ready.
      const computeAndPreview = async (): Promise<void> => {
        try {
          let changes: CheckpointFileChange[] = [];
          if (checkpointId && sessionWorkDir) {
            try {
              changes = await window.snow.listCheckpointChanges(
                checkpointId,
                sessionWorkDir
              );
            } catch {
              // Best effort — show dialog without changes on error
            }
          }

        // Fetch TODO items that will be deleted alongside the rollback.
        let todoItems: RollbackTodoItem[] = [];
        if (convId && responseId) {
          try {
            const todoJson = await window.snow.listTodosForRollback(
              convId,
              responseId
            );
            const parsed = JSON.parse(todoJson) as unknown;
            if (Array.isArray(parsed)) {
              todoItems = parsed
                .filter(
                  (item): item is Record<string, unknown> =>
                    typeof item === "object" && item !== null
                )
                .map((item) => ({
                  id: typeof item.id === "string" ? item.id : "",
                  content: typeof item.content === "string" ? item.content : "",
                  status:
                    typeof item.status === "string" ? item.status : "pending",
                }))
                .filter((item) => item.id);
            }
          } catch {
            // Best effort — show empty on error
          }
        }

        ctx.setRollbackPreview({
          messageId,
          messageContent,
          changes,
          checkpointId,
          workDir: sessionWorkDir,
          convId,
          responseId,
          persistedMessageId,
          isFirstMessage,
          isContextCompaction: targetMessage.isContextCompaction === true,
          todoItems,
          streamPromise:
            ctx.sessionsRefData.current.get(key)?.streamPromise ?? null,
          summaryPromise:
            ctx.sessionsRefData.current.get(key)?.summaryPromise ?? null,
        });
        } finally {
          setPreparingMessageId(null);
        }
      };

      void computeAndPreview();
    },
    [
      ctx.directoryPath,
      ctx.updateSessionField,
      ctx.removeStreamingId,
      ctx.activeConversationIdRef,
      ctx.sessionsRefData,
      ctx.sessionsRef,
      ctx.setRollbackPreview,
    ]
  );

  const confirmRollback = useCallback(
    async (mode: RollbackMode): Promise<void> => {
      const preview = ctx.rollbackPreview;
      if (!preview) {
        return;
      }

      const key = ctx.activeConversationIdRef.current ?? PENDING_SESSION_KEY;
      const {
        messageId,
        messageContent,
        checkpointId,
        convId,
        responseId,
        persistedMessageId,
        isFirstMessage,
        isContextCompaction,
      } = preview;

      // Wait for any in-flight stream AND summary generation to fully settle
      // (including the Rust store_chat_exchange / update_conversation_summary
      // write transactions) before issuing delete/truncate. Without this, the
      // write transactions race and can exceed the busy_timeout, producing a
      // "database is locked" error. The promises are captured at
      // handleRollback time (before the agent loop clears them from the ref).
      const pending: Promise<unknown>[] = [];
      if (preview.streamPromise) {
        pending.push(preview.streamPromise);
      }
      if (preview.summaryPromise) {
        pending.push(preview.summaryPromise);
      }
      if (pending.length > 0) {
        await Promise.allSettled(pending);
      }

      // 回退是事务性的：必须先成功删除/截断持久化会话，再更新界面。
      // 持久化失败时界面消息保持原样，预览重新打开并显示错误，用户可
      // 以重试或取消，不会出现"界面已撤销但重启后消息复活"的不一致。
      try {
        if (isFirstMessage && !isContextCompaction && convId) {
          await window.snow.deleteConversation(convId);
        } else if (convId && persistedMessageId) {
          // 失败/中断轮次没有 responseId，用持久化用户消息 ID 作为边界，
          // 从该行开始删除该轮及之后的所有消息。
          ctx.updateSessionField(key, "tokenUsage", null);
          await window.snow.truncateConversationFromMessage(
            convId,
            persistedMessageId
          );
        } else if (convId && responseId) {
          ctx.updateSessionField(key, "tokenUsage", null);
          await window.snow.truncateConversation(convId, responseId);
        }
      } catch (error) {
        ctx.setRollbackPreview({
          ...preview,
          error: getErrorMessage(error),
        });
        return;
      }

      // Persistence succeeded — now update the UI. The message list update is
      // intentionally deferred until AFTER the file restore: SSH rollback
      // restores files over SFTP and can take a while, and the confirm dialog
      // stays open with a loading button during that time. Updating the list
      // here would show the rolled-back conversation while the dialog is still
      // waiting, which feels broken. DB persistence already happened above, so
      // the UI state below cannot diverge from disk.
      if (checkpointId) {
        const sessionRef = ctx.sessionsRefData.current.get(key);
        const checkpointIndex =
          sessionRef?.checkpointIds.indexOf(checkpointId) ?? -1;
        const discardedCheckpointIds =
          sessionRef && checkpointIndex >= 0
            ? sessionRef.checkpointIds.slice(checkpointIndex)
            : [checkpointId];
        if (sessionRef && checkpointIndex >= 0) {
          sessionRef.checkpointIds = sessionRef.checkpointIds.slice(
            0,
            checkpointIndex
          );
        }

        const shouldRestoreFiles =
          mode === "conversation-and-files" && Boolean(preview.workDir);
        if (shouldRestoreFiles && preview.workDir) {
          // 等待文件恢复完成再关闭对话框：SSH 回滚经 SFTP 逐文件写回，
          // 可能较慢，对话框确认按钮在此期间显示 loading。恢复失败不
          // 阻塞消息清理（best effort，与旧行为一致）。
          try {
            await window.snow.restoreCheckpoint(checkpointId, preview.workDir);
          } catch {
            // Best effort — file restore failure must not block rollback cleanup.
          } finally {
            ctx.setConversationVersion((version) => version + 1);
            deleteCheckpoints(discardedCheckpointIds);
          }
        } else {
          deleteCheckpoints(discardedCheckpointIds);
        }
      }

      // 文件恢复完成后再更新消息列表：弹窗此时仍打开（确认按钮 loading），
      // 列表变化与弹窗关闭同步发生，避免"消息已回滚但弹窗还停着"的割裂。
      ctx.updateSessionMessages(key, (currentMessages) => {
        const targetIndex = currentMessages.findIndex(
          (message) => message.id === messageId
        );
        return targetIndex === -1
          ? currentMessages
          : currentMessages.slice(0, targetIndex);
      });

      if (isFirstMessage && !isContextCompaction && convId) {
        // 会话已被删除：刷新侧边栏列表，移除该会话
        ctx.setConversationListVersion((version) => version + 1);
        ctx.sessionsRefData.current.delete(key);
        ctx.setSessions((prev) => {
          const next = { ...prev };
          delete next[key];
          return next;
        });
        ctx.setActiveId(undefined);
      } else {
        // Bump version so dependent components (user-message rail) re-fetch
        // the updated message list after truncation.
        ctx.setConversationVersion((version) => version + 1);
        // 截断会改变会话记录（消息数/预览/更新时间）：同步侧边栏列表
        ctx.setConversationListVersion((version) => version + 1);
      }

      if (!isContextCompaction) {
        ctx.setDraftToRestore(messageContent);
      }
      ctx.setRollbackPreview(null);
    },
    [
      ctx.rollbackPreview,
      ctx.directoryPath,
      ctx.updateSessionField,
      ctx.updateSessionMessages,
      ctx.setConversationVersion,
      ctx.setConversationListVersion,
      ctx.setActiveId,
      ctx.setDraftToRestore,
      ctx.setRollbackPreview,
      ctx.sessionsRefData,
      ctx.setSessions,
      ctx.activeConversationIdRef,
    ]
  );

  const cancelRollback = useCallback((): void => {
    ctx.setRollbackPreview(null);
  }, [ctx.setRollbackPreview]);

  return {
    clearDraftToRestore,
    handleRollback,
    confirmRollback,
    cancelRollback,
    preparingMessageId,
  };
};
