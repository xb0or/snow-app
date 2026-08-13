import type {
  ChatConversationMessage,
  ConversationContextValue,
  ToolAuthorizationDecision,
  ToolCallInfo,
} from "../utils/conversationTypes";
import {
  createMessageId,
  directoryIdToPath,
  formatMessageTime,
  formatMcpToolResultForModel,
  formatToolResultsContent,
  getErrorMessage,
  parseToolCalls,
  updateFirstMatchingToolCall,
} from "../utils/conversationHelpers";
import { resolveResponseDisposition } from "../utils/responseDisposition";
import { appendHookExecutionToMessage, runHook } from "./hookOutcome";
import { extractFileChangeFromTool } from "./fileChangeTracking";
import { injectSessionIdIntoToolArgs } from "../utils/toolSessionMetadata";
import {
  parseSubAgentTools,
  resolveSubAgentRuntimeConfig,
} from "./subAgentRuntimeConfig";
import {
  PARENT_PLAN_APPROVAL_REQUIRED,
  beginStreamMetricsIteration,
  createStreamChunkHandler,
  createStreamIdHandler,
  resetRunStreamMetrics,
} from "./agentLoopHelpers";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/** 子代理队友通信工具：所有子代理默认携带（Rust collect_allowed_mcp_tools
 *  无条件追加定义；执行在渲染进程完成）。 */
export const SUB_AGENT_COMMS_TOOL_NAMES = new Set([
  "sub-agents-listTeammates",
  "sub-agents-sendMessage",
]);

export type SubAgentActivationDeps = {
  ctx: ConversationContextValue;
  requestToolAuthorizations: (
    toolCalls: ToolCallInfo[],
    conversationId: string,
    projectId?: string
  ) => Promise<ToolAuthorizationDecision[]>;
  parentApiProfile: string | undefined;
  parentModel: string | undefined;
  parentThinkingStrength: string | undefined;
  planApprovedSessionKeysRef: { current: Set<string> };
};

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

export const createSubAgentActivation = (deps: SubAgentActivationDeps) => {
  const {
    ctx,
    requestToolAuthorizations,
    parentApiProfile,
    parentModel,
    parentThinkingStrength,
    planApprovedSessionKeysRef,
  } = deps;

  return async (
    argsJson: string,
    parentConversationId: string,
    dirId: string,
    toolCallInteractionId?: string
  ): Promise<string> => {
    // Inherit checkpoint ids from the parent session so that file changes
    // made by sub-agent tools are recorded into the same checkpoint
    // manifest.  This allows the main conversation rollback to detect and
    // restore sub-agent modifications alongside the parent's own changes.
    const parentCheckpointIds =
      ctx.sessionsRefData.current.get(parentConversationId)?.checkpointIds ??
      [];
    const subCheckpointWorkDir =
      parentCheckpointIds.length > 0
        ? directoryIdToPath(dirId) ?? ctx.directoryPath
        : undefined;

    const parsedArgs = JSON.parse(argsJson) as Record<string, unknown>;
    const agentId =
      typeof parsedArgs.agentId === "string" ? parsedArgs.agentId : "";
    const prompt =
      typeof parsedArgs.prompt === "string" ? parsedArgs.prompt : "";

    if (!agentId || !prompt) {
      return JSON.stringify({
        success: false,
        error: "agentId and prompt are required",
      });
    }
    let subConversationId: string | undefined;
    let subAgentName: string | undefined;
    let config: Awaited<ReturnType<typeof window.snow.getSubAgentConfig>> =
      null;

    // 子代理回合循环。try 块内赋值（依赖运行时解析的配置），catch
    // 路径（第一轮失败/被中断）也需要它来继续处理用户强行发送的消息。
    let subAgentRunLoop: (
      subMessages: {
        role: "user" | "assistant" | "system" | "developer" | "tool";
        content: string;
      }[],
      resumeAfterCompaction?: boolean
    ) => Promise<string> = async () => "";

    // 用户强行发送的循环处理（sendPendingMessageNow 已 handleAbort
    // 中断当前回合并把消息暂存到会话的 forceSendMessages）：复用自动
    // 发送路径 —— 暂存消息作为新回合继续在本子代理会话处理，保持
    // 子代理的模型/API 配置/工具集/系统提示，绝不转交父会话、也绝不
    // 走主流程 handleSendMessage（那会把子代理当成主会话发送，还会
    // 被侧边栏 upsert 成"新主会话"）。直到没有新的强行发送为止。
    const runForceSendLoop = async (): Promise<void> => {
      const activeSubConvId = subConversationId;
      if (!activeSubConvId) {
        return;
      }
      while (true) {
        const ref = ctx.sessionsRefData.current.get(activeSubConvId);
        const forceSends = ref?.forceSendMessages;
        if (!forceSends || forceSends.length === 0) {
          return;
        }
        ref!.forceSendMessages = undefined;
        const forceSendText = forceSends
          .map((item) => item.text)
          .join("\n\n");
        const forceUserMsg: ChatConversationMessage = {
          id: createMessageId("user"),
          role: "user",
          content: forceSendText,
          timestamp: formatMessageTime(),
          status: "sent",
        };
        ctx.updateSessionMessages(activeSubConvId, (currentMessages) => [
          ...currentMessages,
          forceUserMsg,
        ]);
        // handleAbort 已复位运行状态：新一轮回合必须恢复
        // isSending/isAbortRequested/流式标记（与子代理激活时的
        // 初始化一致），handleAbort 与暂停检查才能正常工作。
        const runRef = ctx.sessionsRefData.current.get(activeSubConvId);
        if (runRef) {
          runRef.isSending = true;
          runRef.isAbortRequested = false;
        }
        ctx.updateSessionField(activeSubConvId, "isStreaming", true);
        resetRunStreamMetrics(ctx, activeSubConvId);
        ctx.updateSessionField(activeSubConvId, "streamStartedAt", Date.now());
        ctx.addStreamingId(activeSubConvId);
        await subAgentRunLoop([{ role: "user", content: forceSendText }]);
      }
    };

    // Carry queued user insertions from a sub-agent conversation over to the
    // parent conversation's pending queue. Called when a sub-agent run ends
    // (normally or with a failure): the sub conversation becomes read-only,
    // so messages the user inserted mid-run must not be lost — the parent
    // loop is still mid-execution and picks them up at its next iteration
    // boundary (or on the user's next send).
    const forwardSubPendingQueue = (finishedSubConvId: string): void => {
      const subQueue = ctx.pendingQueueRef.current.get(finishedSubConvId) ?? [];
      if (subQueue.length === 0) {
        return;
      }
      ctx.pendingQueueRef.current.delete(finishedSubConvId);
      const parentQueue =
        ctx.pendingQueueRef.current.get(parentConversationId) ?? [];
      parentQueue.push(...subQueue);
      ctx.pendingQueueRef.current.set(parentConversationId, parentQueue);
      ctx.setActivePendingMessages(
        ctx.activeConversationIdRef.current === parentConversationId
          ? parentQueue.map((item) => item.text)
          : []
      );
    };

    try {
      // 项目级子代理优先：先查当前项目（dirId）下的配置，未命中再回退全局。
      config = dirId
        ? (await window.snow.getSubAgentConfig(agentId, dirId)) ??
          (await window.snow.getSubAgentConfig(agentId))
        : await window.snow.getSubAgentConfig(agentId);
      if (!config) {
        return JSON.stringify({
          success: false,
          error: `Sub-agent configuration not found: ${agentId}`,
        });
      }

      const runtimeConfig = resolveSubAgentRuntimeConfig({
        config,
        apiConfigs: await window.snow.listApiConfigs(),
        parentApiProfile,
        parentModel,
        parentThinkingStrength,
      });
      const allowedTools = parseSubAgentTools(runtimeConfig.toolsJson);

      subConversationId = `sub-${Date.now()}-${Math.random()
        .toString(36)
        .slice(2, 10)}`;
      const title = prompt.length > 80 ? `${prompt.slice(0, 80)}...` : prompt;

      // Execute beforeSubAgentStart hooks. If blocked, abort the
      // sub-agent activation immediately with the hook's message.
      try {
        const beforeSubAgentContext = JSON.stringify({
          agentId,
          agentName: runtimeConfig.agentName,
          prompt,
          parentConversationId,
          cwd: directoryIdToPath(dirId) ?? ctx.directoryPath ?? "",
        });
        const subHookResult = await runHook(
          "beforeSubAgentStart",
          dirId || undefined,
          beforeSubAgentContext
        );
        if (subHookResult) {
          ctx.updateSessionMessages(parentConversationId, (currentMessages) =>
            appendHookExecutionToMessage(currentMessages, {
              ...subHookResult.record,
              // Bind to the sub-agent tool call so the hook renders attached
              // to the sub-agent card ("启动前" step), not the message footer.
              toolCallInteractionId,
            })
          );
          if (subHookResult.outcome.kind === "abort") {
            return JSON.stringify({
              success: false,
              error: subHookResult.outcome.message,
            });
          }
        }
      } catch {
        // Hook execution failed -- continue with sub-agent activation
      }

      await window.snow.createSubAgentSession(
        subConversationId,
        parentConversationId,
        runtimeConfig.agentId,
        runtimeConfig.agentName,
        dirId,
        runtimeConfig.apiProfile,
        runtimeConfig.model,
        title
      );

      await window.snow.updateSubAgentSessionStatus(
        subConversationId,
        "running",
        ""
      );

      ctx.setSubAgentSessionEvent({
        parentConversationId,
        conversationId: subConversationId,
        agentId,
        agentName: runtimeConfig.agentName,
        status: "running",
        timestamp: Date.now(),
        toolCallInteractionId,
      });

      const subAgentToolsJson = runtimeConfig.toolsJson;
      subAgentName = runtimeConfig.agentName;

      // ---------------------------------------------------------------------
      // 子代理队友通信（sub-agents-listTeammates / sub-agents-sendMessage）
      // 在渲染进程直接执行：在线队友状态（subAgentSessionEventsRef）与
      // Pending 消息队列（pendingQueueRef）都只存在于渲染进程，Rust 无法
      // 感知。会话隔离：只允许同一 parentConversationId 下的子代理互相
      // 可见、互相通信，跨会话查询/发送一律拒绝。
      // ---------------------------------------------------------------------
      const executeSubAgentCommsTool = async (
        toolName: string,
        argsJson: string,
        senderConvId: string,
        senderAgentName: string
      ): Promise<string> => {
        let args: Record<string, unknown>;
        try {
          args = JSON.parse(argsJson) as Record<string, unknown>;
        } catch {
          return JSON.stringify({
            success: false,
            error: "Invalid JSON arguments",
          });
        }

        const eventMap = ctx.subAgentSessionEventsRef.current;

        if (toolName === "sub-agents-listTeammates") {
          const teammates = Object.values(eventMap)
            .filter(
              (event) =>
                event.parentConversationId === parentConversationId &&
                event.conversationId !== senderConvId &&
                event.status === "running" &&
                !ctx.sessionsRefData.current.get(event.conversationId)
                  ?.subAgentTerminated
            )
            .map((event) => ({
              conversationId: event.conversationId,
              agentId: event.agentId,
              agentName: event.agentName,
            }));
          return JSON.stringify({ success: true, teammates });
        }

        if (toolName === "sub-agents-sendMessage") {
          const targetConvId =
            typeof args.conversationId === "string"
              ? args.conversationId.trim()
              : "";
          const message =
            typeof args.message === "string" ? args.message.trim() : "";
          if (!targetConvId || !message) {
            return JSON.stringify({
              success: false,
              error: "conversationId and message are required",
            });
          }
          if (targetConvId === senderConvId) {
            return JSON.stringify({
              success: false,
              error: "Cannot send a message to yourself",
            });
          }

          // 会话隔离：目标必须是同一父会话下的子代理。
          const targetEvent = eventMap[targetConvId];
          if (
            !targetEvent ||
            targetEvent.parentConversationId !== parentConversationId
          ) {
            return JSON.stringify({
              success: false,
              error:
                "Target sub-agent does not exist or belongs to another conversation (session isolation: teammates must be in the same session)",
            });
          }
          // 只允许发给仍在运行的子代理：消息以 Pending 形式进入目标会话的
          // 队列，由目标循环在下一回合边界自动切入。
          if (
            targetEvent.status !== "running" ||
            ctx.sessionsRefData.current.get(targetConvId)?.subAgentTerminated
          ) {
            return JSON.stringify({
              success: false,
              error:
                "Target sub-agent is no longer running; messages can only be sent to teammates that are still active",
            });
          }

          // 消息自带发送方标识，目标子代理收到后可知来源。
          const queuedText = `[来自子代理 ${senderAgentName} (${senderConvId})]\n${message}`;
          const queue = ctx.pendingQueueRef.current.get(targetConvId) ?? [];
          queue.push({ text: queuedText, options: {} });
          ctx.pendingQueueRef.current.set(targetConvId, queue);
          // 目标会话若正处于激活状态，让 Pending 消息气泡实时可见。
          if (ctx.activeConversationIdRef.current === targetConvId) {
            ctx.setActivePendingMessages(queue.map((item) => item.text));
          }

          return JSON.stringify({
            success: true,
            queued: true,
            recipient: {
              conversationId: targetConvId,
              agentName: targetEvent.agentName,
            },
            note: "The message was queued as a Pending message and will be delivered to the target at the end of its current round.",
          });
        }

        return JSON.stringify({
          success: false,
          error: `Unknown sub-agent communication tool: ${toolName}`,
        });
      };

      const subConvId = subConversationId!;
      ctx.ensureSession(subConvId, dirId);
      const subSessionRef = ctx.sessionsRefData.current.get(subConvId);
      if (subSessionRef) {
        subSessionRef.isSending = true;
        subSessionRef.isAbortRequested = false;
        // Sub-agents never run Plan/Goal Mode (Rust forces both off on the
        // sub-agent request path). Zero the inherited defaults so the ref
        // stays truthful for any future reader.
        subSessionRef.planMode = false;
        subSessionRef.goalMode = false;
      }
      // Register this sub-agent on the parent session so aborting the main
      // flow can cascade the cancellation down to it (and its children).
      const parentSessionRef =
        ctx.sessionsRefData.current.get(parentConversationId);
      if (parentSessionRef) {
        parentSessionRef.childSubAgentIds.add(subConvId);
      }
      ctx.updateSessionField(subConvId, "isStreaming", true);
      resetRunStreamMetrics(ctx, subConvId);
      // Anchor the accumulating timer start for the sub-agent session so
      // its StreamMetrics timer is independent of the parent session.
      ctx.updateSessionField(subConvId, "streamStartedAt", Date.now());
      ctx.addStreamingId(subConvId);

      const subUserMessage: ChatConversationMessage = {
        id: createMessageId("user"),
        role: "user",
        content: prompt,
        timestamp: formatMessageTime(),
        status: "sent",
      };

      ctx.updateSessionMessages(subConvId, (currentMessages) => [
        ...currentMessages,
        subUserMessage,
      ]);

      const isSubCancelled = (): boolean =>
        !!ctx.sessionsRefData.current.get(subConvId)?.isAbortRequested;

      subAgentRunLoop = async (
        subMessages: {
          role: "user" | "assistant" | "system" | "developer" | "tool";
          content: string;
        }[],
        resumeAfterCompaction = false
      ): Promise<string> => {
        if (ctx.sessionsRefData.current.get(subConvId)?.isAbortRequested) {
          return "Sub-agent interrupted by user";
        }

        const subAssistantMessageId = createMessageId("assistant");
        const subAssistantMessage: ChatConversationMessage = {
          id: subAssistantMessageId,
          role: "assistant",
          content: "",
          timestamp: formatMessageTime(),
          status: "sending",
        };

        ctx.updateSessionMessages(subConvId, (currentMessages) => [
          ...currentMessages,
          subAssistantMessage,
        ]);

        beginStreamMetricsIteration(ctx, subConvId);
        const subStreamPromise = window.snow.createResponseStream(
          {
            messages: subMessages,
            conversationId: subConvId,
            directoryId: dirId,
            apiProfile: runtimeConfig.apiProfile,
            model: runtimeConfig.model,
            thinkingStrength: runtimeConfig.thinkingStrength,
            resumeAfterCompaction,
            subAgentToolsJson,
            subAgentSystemPrompt: runtimeConfig.systemPrompt || undefined,
            // Sub-agents always use their own normal-mode prompt and tool set.
            // Parent Plan Mode is enforced separately at Rust tool execution.
            planMode: false,
          },
          createStreamChunkHandler(
            ctx,
            subConvId,
            subAssistantMessageId,
            isSubCancelled
          ),
          createStreamIdHandler(ctx, subConvId, isSubCancelled)
        );
        const subStreamRefBefore = ctx.sessionsRefData.current.get(subConvId);
        if (subStreamRefBefore) {
          subStreamRefBefore.streamPromise = subStreamPromise;
        }

        const subResponse = await subStreamPromise;
        const subResponseDisposition = resolveResponseDisposition(subResponse);
        const subResponseFailed = subResponseDisposition.kind === "error";

        const subRef = ctx.sessionsRefData.current.get(subConvId);
        if (subRef) {
          subRef.streamId = null;
          subRef.streamPromise = null;
        }

        if (ctx.sessionsRefData.current.get(subConvId)?.isAbortRequested) {
          ctx.updateSessionMessages(subConvId, (currentMessages) =>
            currentMessages.map((currentMessage) =>
              currentMessage.id === subAssistantMessageId
                ? {
                    ...currentMessage,
                    status: "sent" as const,
                    content:
                      currentMessage.content || "Sub-agent interrupted by user",
                    isRetrying: false,
                  }
                : currentMessage
            )
          );
          return "Sub-agent interrupted by user";
        }

        if (subResponse.tokenUsage && !subResponseFailed) {
          ctx.updateSessionField(
            subConvId,
            "tokenUsage",
            subResponse.tokenUsage
          );
        }

        // A final incomplete-like result terminates the sub-agent's internal
        // loop. Keep display-safe content and metadata, but never parse,
        // authorize, execute, or return incomplete tool arguments.
        if (subResponseDisposition.kind === "incomplete") {
          const currentSubAssistant = ctx.sessionsRef.current?.[
            subConvId
          ]?.messages.find((message) => message.id === subAssistantMessageId);
          const incompleteContent =
            subResponse.content || currentSubAssistant?.content || "";
          const safeIncompleteResult = incompleteContent.trim()
            ? incompleteContent
            : "Sub-agent response ended before completion. Any incomplete tool call was discarded and was not executed.";

          ctx.updateSessionMessages(subConvId, (currentMessages) =>
            currentMessages.map((currentMessage) =>
              currentMessage.id === subAssistantMessageId
                ? {
                    ...currentMessage,
                    content:
                      subResponse.content || currentMessage.content || "",
                    thinking:
                      subResponse.thinking ||
                      currentMessage.thinking ||
                      undefined,
                    timestamp: formatMessageTime(),
                    status: "incomplete" as const,
                    incompleteVariant: subResponseDisposition.variant,
                    interruptionReason: subResponseDisposition.reason,
                    recoveryOutcome: subResponseDisposition.recoveryOutcome,
                    responseId: subResponse.id || undefined,
                    model: subResponse.model || undefined,
                    toolCalls: undefined,
                    isRetrying: false,
                    retryAttempt: undefined,
                    retryError: undefined,
                  }
                : currentMessage
            )
          );
          return safeIncompleteResult;
        }

        const subToolCalls =
          subResponseDisposition.kind === "complete"
            ? parseToolCalls(subResponse.toolCallsJson)
            : [];

        if (subResponseFailed) {
          const failureContent =
            subResponse.content || "Sub-agent request failed. Please retry.";
          ctx.updateSessionMessages(subConvId, (currentMessages) =>
            currentMessages.map((currentMessage) =>
              currentMessage.id === subAssistantMessageId
                ? {
                    ...currentMessage,
                    content: failureContent,
                    thinking: subResponse.thinking || undefined,
                    timestamp: formatMessageTime(),
                    status: "error" as const,
                    responseId: subResponse.id || undefined,
                    model: subResponse.model || undefined,
                    isRetrying: false,
                  }
                : currentMessage
            )
          );
          return failureContent;
        }

        // Auto-compaction for sub-agents: mirrors the main agent loop. When the
        // resolved API profile has enableAutoCompress=true and the total token
        // usage crosses its configured threshold, finalize the assistant
        // message, compact the sub-conversation, and continue the sub-agent loop
        // from the compacted context. The fixed profile name is reused for every
        // check, while its latest non-secret settings are read so threshold edits
        // apply without a restart.
        //
        // Only runs while the sub-agent loop is still alive (tool calls to
        // process). When the sub-agent is finishing naturally (no tool calls),
        // compaction must NOT fire even over the threshold — it would force
        // another subAgentRunLoop iteration and wake the sub-agent back up
        // after it completed.
        if (
          subToolCalls.length > 0 &&
          subResponse.tokenUsage &&
          !subResponseFailed
        ) {
          const subApiConfig = (await window.snow.listApiConfigs()).find(
            (item) => item.profileName.trim() === runtimeConfig.apiProfile
          );
          if (subApiConfig?.enableAutoCompress) {
            // autoCompressThreshold is stored in TOKENS — compare directly (see
            // the main loop check for why calculateAutoCompressThresholdTokens
            // is intentionally not used here).
            const subThresholdTokens = subApiConfig.autoCompressThreshold;
            if (subThresholdTokens != null && subThresholdTokens > 0) {
              const subTotalTokens =
                subResponse.tokenUsage.inputTokens +
                subResponse.tokenUsage.outputTokens;
              if (subTotalTokens >= subThresholdTokens) {
                // Finalize the assistant message that crossed the threshold so
                // it does not linger in "sending" state. Any tool calls it
                // emitted are abandoned by the handoff; the Rust compaction
                // boundary plus ensure_tool_pairing keep the post-compaction
                // context free of orphan tool entries.
                ctx.updateSessionMessages(subConvId, (currentMessages) =>
                  currentMessages.map((currentMessage) =>
                    currentMessage.id === subAssistantMessageId
                      ? {
                          ...currentMessage,
                          content:
                            subResponse.content || currentMessage.content || "",
                          thinking:
                            subResponse.thinking ||
                            currentMessage.thinking ||
                            undefined,
                          timestamp: formatMessageTime(),
                          status: "sent",
                          responseId: subResponse.id || undefined,
                          model: subResponse.model || undefined,
                          isRetrying: false,
                        }
                      : currentMessage
                  )
                );

                const subCompactionSummary =
                  await ctx.performCompactionRef.current(
                    subConvId,
                    runtimeConfig.model,
                    true,
                    undefined,
                    runtimeConfig.apiProfile,
                    runtimeConfig.toolsJson,
                    runtimeConfig.systemPrompt || undefined
                  );

                if (subCompactionSummary) {
                  if (isSubCancelled()) {
                    return "Sub-agent interrupted by user";
                  }

                  // performCompaction's finally resets isSending=false, but the
                  // sub-agent loop is still mid-send. Restore it so abort keeps
                  // working and the session stays locked until the loop ends.
                  const subRefAfterCompaction =
                    ctx.sessionsRefData.current.get(subConvId);
                  if (subRefAfterCompaction) {
                    subRefAfterCompaction.isSending = true;
                    subRefAfterCompaction.isAbortRequested = false;
                  }

                  // Continue the sub-agent loop from the compacted context. The
                  // Rust backend rebuilds context from the compaction boundary
                  // stored in the database for this sub-conversation.
                  return subAgentRunLoop(
                    [{ role: "user", content: subCompactionSummary }],
                    true
                  );
                }
              }
            }
          }
        }

        if (subToolCalls.length === 0) {
          ctx.updateSessionMessages(subConvId, (currentMessages) =>
            currentMessages.map((currentMessage) =>
              currentMessage.id === subAssistantMessageId
                ? {
                    ...currentMessage,
                    content:
                      subResponse.content ||
                      currentMessage.content ||
                      "Sub-agent completed with no output.",
                    status: "sent" as const,
                    responseId: subResponse.id || undefined,
                    model: subResponse.model || undefined,
                    isRetrying: false,
                  }
                : currentMessage
            )
          );

          return subResponse.content || "Sub-agent completed with no output.";
        }

        ctx.updateSessionMessages(subConvId, (currentMessages) =>
          currentMessages.map((currentMessage) =>
            currentMessage.id === subAssistantMessageId
              ? {
                  ...currentMessage,
                  content: subResponse.content || "",
                  thinking: subResponse.thinking || undefined,
                  toolCalls: subToolCalls.map((tc) => ({
                    ...tc,
                    status: "pending" as const,
                  })),
                  status: "sent" as const,
                  responseId: subResponse.id || undefined,
                  model: subResponse.model || undefined,
                  isRetrying: false,
                }
              : currentMessage
          )
        );

        const subAuthorizationDecisions = await requestToolAuthorizations(
          subToolCalls,
          subConvId,
          dirId
        );

        const subAllToolsRejected = subAuthorizationDecisions.every(
          (decision) => decision.status === "rejected"
        );
        // 用户填写了拒绝理由时，拒绝理由作为工具结果回传子代理 AI，
        // 子代理 Loop 继续；仅当全部拒绝且没有用户理由时才终止。
        const subHasUserProvidedRejectionReason =
          subAuthorizationDecisions.some(
            (decision) =>
              decision.status === "rejected" &&
              decision.userProvidedReason === true
          );

        const subToolResults: string[] = [];
        const subStructuredResults: {
          name: string;
          callId: string;
          result: string;
        }[] = [];
        let parentPlanApprovalRequired = false;

        for (
          let subToolIndex = 0;
          subToolIndex < subToolCalls.length;
          subToolIndex++
        ) {
          const subToolCall = subToolCalls[subToolIndex];
          const subAuthorizationDecision =
            subAuthorizationDecisions[subToolIndex];

          if (ctx.sessionsRefData.current.get(subConvId)?.isAbortRequested) {
            return "Sub-agent interrupted by user";
          }

          if (subAuthorizationDecision.status === "rejected") {
            const subRejectResult = JSON.stringify({
              success: false,
              error: "TOOL_EXECUTION_DENIED_BY_USER",
              reason:
                subAuthorizationDecision.reason ||
                "User declined tool execution",
            });
            subToolResults.push(formatMcpToolResultForModel(subRejectResult));
            subStructuredResults.push({
              name: subToolCall.name,
              callId: subToolCall.callId || "",
              result: formatMcpToolResultForModel(subRejectResult),
            });

            ctx.updateSessionMessages(subConvId, (currentMessages) =>
              currentMessages.map((currentMessage) => {
                if (currentMessage.id !== subAssistantMessageId) {
                  return currentMessage;
                }
                return {
                  ...currentMessage,
                  toolCalls: updateFirstMatchingToolCall(
                    currentMessage.toolCalls,
                    subToolCall,
                    ["pending"],
                    (currentToolCall) => ({
                      ...currentToolCall,
                      status: "completed" as const,
                      result: subRejectResult,
                    })
                  ),
                };
              })
            );
            continue;
          }

          const subToolArgs = injectSessionIdIntoToolArgs(
            subToolCall.name,
            subToolCall.arguments,
            subConvId
          );
          let subSensitiveAuthorizationToken: string | undefined;
          if (
            subToolCall.name === "bash-terminal-execute" &&
            subAuthorizationDecision.status === "approved" &&
            subAuthorizationDecision.sensitiveCommandConfirmed === true
          ) {
            try {
              const subParsedArgs = JSON.parse(subToolArgs) as Record<
                string,
                unknown
              >;
              if (typeof subParsedArgs.command !== "string") {
                throw new Error("Sensitive command argument is missing");
              }
              subSensitiveAuthorizationToken =
                await window.snow.issueSensitiveCommandAuthorization(
                  subParsedArgs.command
                );
            } catch {
              // If authorization fails, let the tool fail naturally.
            }
          }

          ctx.updateSessionMessages(subConvId, (currentMessages) =>
            currentMessages.map((currentMessage) => {
              if (currentMessage.id !== subAssistantMessageId) {
                return currentMessage;
              }
              return {
                ...currentMessage,
                toolCalls: updateFirstMatchingToolCall(
                  currentMessage.toolCalls,
                  subToolCall,
                  ["pending"],
                  (currentToolCall) => ({
                    ...currentToolCall,
                    status: "running" as const,
                    startedAt: Date.now(),
                  })
                ),
              };
            })
          );

          let subResult: string;
          let subToolErrored = false;
          try {
            if (SUB_AGENT_COMMS_TOOL_NAMES.has(subToolCall.name)) {
              // 队友通信工具由渲染进程直接执行（会话隔离与 Pending 队列
              // 都在渲染进程），不走 Rust callMcpTool。
              subResult = await executeSubAgentCommsTool(
                subToolCall.name,
                subToolArgs,
                subConvId,
                subAgentName ?? agentId
              );
            } else {
              subResult = await window.snow.callMcpTool(
                subToolCall.name,
                subToolArgs,
                dirId,
                parentCheckpointIds,
                subCheckpointWorkDir,
                subSensitiveAuthorizationToken,
                (chunk) => {
                  if (!chunk.data) {
                    return;
                  }
                  if (
                    chunk.stream === "interactive_session" ||
                    chunk.stream === "tool_execution"
                  ) {
                    ctx.updateSessionMessages(subConvId, (currentMessages) =>
                      currentMessages.map((currentMessage) => {
                        if (currentMessage.id !== subAssistantMessageId) {
                          return currentMessage;
                        }
                        return {
                          ...currentMessage,
                          toolCalls: updateFirstMatchingToolCall(
                            currentMessage.toolCalls,
                            subToolCall,
                            ["pending", "running"],
                            (currentToolCall) => ({
                              ...currentToolCall,
                              interactiveSessionId:
                                chunk.stream === "interactive_session"
                                  ? chunk.data
                                  : currentToolCall.interactiveSessionId,
                              toolExecutionId:
                                chunk.stream === "tool_execution"
                                  ? chunk.data
                                  : currentToolCall.toolExecutionId,
                            })
                          ),
                        };
                      })
                    );
                    return;
                  }
                  ctx.updateSessionMessages(subConvId, (currentMessages) =>
                    currentMessages.map((currentMessage) => {
                      if (currentMessage.id !== subAssistantMessageId) {
                        return currentMessage;
                      }
                      return {
                        ...currentMessage,
                        toolCalls: updateFirstMatchingToolCall(
                          currentMessage.toolCalls,
                          subToolCall,
                          ["pending", "running"],
                          (currentToolCall) => ({
                            ...currentToolCall,
                            streamingStdout:
                              chunk.stream === "stdout"
                                ? `${currentToolCall.streamingStdout ?? ""}${
                                    chunk.data
                                  }`
                                : currentToolCall.streamingStdout,
                            streamingStderr:
                              chunk.stream === "stderr"
                                ? `${currentToolCall.streamingStderr ?? ""}${
                                    chunk.data
                                  }`
                                : currentToolCall.streamingStderr,
                          })
                        ),
                      };
                    })
                  );
                },
                subToolCall.interactionId,
                allowedTools,
                // These booleans carry only the parent conversation's Rust
                // write-gate state; they do not enable Plan Mode for the sub-agent.
                // Read from the parent session's own ref so a background parent
                // keeps its gate even after the user switches conversations.
                ctx.sessionsRefData.current.get(parentConversationId)
                  ?.planMode ?? ctx.planModeRef.current,
                planApprovedSessionKeysRef.current.has(parentConversationId)
              );
            }
          } catch (err) {
            subToolErrored = true;
            const errorMessage = getErrorMessage(err);

            if (errorMessage.includes(PARENT_PLAN_APPROVAL_REQUIRED)) {
              parentPlanApprovalRequired = true;
            }
            // Recover partial streaming output for terminal-execute
            // so the sub-agent (and ultimately the parent AI loop)
            // receives the partial output together with the error.
            if (subToolCall.name === "bash-terminal-execute") {
              const subSessionMessages =
                ctx.sessionsRef.current?.[subConvId]?.messages ?? [];
              const subAssistantMsg = subSessionMessages.find(
                (m) => m.id === subAssistantMessageId
              );
              const liveSubToolCall = subAssistantMsg?.toolCalls?.find(
                (tc) =>
                  tc.interactionId === subToolCall.interactionId &&
                  tc.name === subToolCall.name
              );
              const partialStdout = liveSubToolCall?.streamingStdout ?? "";
              const partialStderr = liveSubToolCall?.streamingStderr ?? "";
              const partialOutput = [partialStdout, partialStderr]
                .filter(Boolean)
                .join("\n");
              subResult = JSON.stringify({
                error: errorMessage,
                stdout: partialStdout,
                stderr: partialStderr,
                partialOutput:
                  partialOutput.length > 0 ? partialOutput : undefined,
              });
            } else {
              subResult = JSON.stringify({ error: errorMessage });
            }
          }

          ctx.updateSessionMessages(subConvId, (currentMessages) =>
            currentMessages.map((currentMessage) => {
              if (currentMessage.id !== subAssistantMessageId) {
                return currentMessage;
              }
              return {
                ...currentMessage,
                toolCalls: updateFirstMatchingToolCall(
                  currentMessage.toolCalls,
                  subToolCall,
                  ["pending", "running"],
                  (currentToolCall) => ({
                    ...currentToolCall,
                    status: subToolErrored
                      ? ("error" as const)
                      : ("completed" as const),
                    result: subResult,
                  })
                ),
              };
            })
          );

          // Record successful file modifications made by this sub-agent under
          // its own conversationId AND the parent conversationId. Storing
          // under the parent key lets the file-change stats panel show the
          // full picture (main agent + sub-agents) without extra lookups;
          // the sub-agent's own key keeps its per-session view accurate.
          if (!subToolErrored && subResult !== undefined) {
            const subFileChange = extractFileChangeFromTool(
              subToolCall.name,
              subToolArgs,
              subResult
            );
            if (subFileChange) {
              const subChangeRecord = {
                ...subFileChange,
                agent: "sub" as const,
                subAgentName: subAgentName ?? config?.name ?? agentId,
                timestamp: Date.now(),
              };
              ctx.recordFileChange(subConvId, subChangeRecord);
              ctx.recordFileChange(parentConversationId, subChangeRecord);
            }
          }

          const subModelResult = formatMcpToolResultForModel(subResult);
          subToolResults.push(subModelResult);
          subStructuredResults.push({
            name: subToolCall.name,
            callId: subToolCall.callId || "",
            result: subModelResult,
          });

          if (parentPlanApprovalRequired) {
            break;
          }
        }

        const subToolResultMessage: ChatConversationMessage = {
          id: createMessageId("tool"),
          role: "tool",
          content: formatToolResultsContent(subStructuredResults),
          timestamp: formatMessageTime(),
          status: "sent",
          toolName: subToolCalls.map((tc) => tc.name).join(", "),
        };

        ctx.updateSessionMessages(subConvId, (currentMessages) => [
          ...currentMessages,
          subToolResultMessage,
        ]);

        // A sub-agent cannot obtain Plan approval. Stop immediately and return
        // control to the main loop instead of feeding the denial back into a
        // recursive sub-agent iteration that could repeatedly retry the write.
        // Queued user insertions are left in place: the post-loop flush below
        // carries them over to the parent conversation.
        if (parentPlanApprovalRequired) {
          return "Sub-agent stopped because the main conversation must approve the Plan Mode plan before delegated writes can run.";
        }

        if (subAllToolsRejected && !subHasUserProvidedRejectionReason) {
          return subToolResults.join("\n\n");
        }

        const subPendingForTools =
          ctx.pendingQueueRef.current.get(subConvId) ?? [];
        const subToolResultsJson = JSON.stringify(subStructuredResults);
        const subNextMessages: {
          role: "user" | "assistant" | "system" | "developer" | "tool";
          content: string;
          toolResultsJson?: string;
        }[] = [
          {
            role: "tool",
            content: formatToolResultsContent(subStructuredResults),
            toolResultsJson: subToolResultsJson,
          },
        ];
        if (subPendingForTools.length > 0) {
          ctx.pendingQueueRef.current.delete(subConvId);
          const subPendingText = subPendingForTools
            .map((item) => item.text)
            .join("\n\n");
          ctx.setActivePendingMessages([]);
          const subPendingUserMsg: ChatConversationMessage = {
            id: createMessageId("user"),
            role: "user",
            content: subPendingText,
            timestamp: formatMessageTime(),
            status: "sent",
          };
          ctx.updateSessionMessages(subConvId, (currentMessages) => [
            ...currentMessages,
            subPendingUserMsg,
          ]);
          subNextMessages.push({ role: "user", content: subPendingText });
        }

        return subAgentRunLoop(subNextMessages);
      };

      const summary = await subAgentRunLoop([
        { role: "user", content: prompt },
      ]);

      const subFinalRef = ctx.sessionsRefData.current.get(subConvId);
      // 用户在子代理运行中点击"立即发送"：先消费暂存消息继续回合，
      // 再统一走终止收尾（terminated + cancelled 广播 + 队列转交
      // 父会话）。
      if (subFinalRef?.forceSendMessages?.length) {
        try {
          await runForceSendLoop();
        } catch {
          // 强行发送回合异常：继续统一收尾（失败路径）
        }
      }
      if (subFinalRef) {
        // Mark the sub-agent conversation read-only before clearing isSending:
        // once the run ends no new agent loop may start in it, and this
        // synchronous flag closes the race window before the UI hides the
        // input box.
        subFinalRef.subAgentTerminated = true;
        subFinalRef.isSending = false;
      }
      ctx.updateSessionField(subConvId, "isStreaming", false);
      ctx.updateSessionField(subConvId, "streamStartedAt", 0);
      ctx.updateSessionField(subConvId, "isAborting", false);
      ctx.removeStreamingId(subConvId);

      // Broadcast the terminal status FIRST — immediately after the flag, so
      // the UI hides the input box as soon as possible. Persisting to the DB
      // and running the (possibly slow) completion hook happen afterwards.
      ctx.setSubAgentSessionEvent({
        parentConversationId,
        conversationId: subConversationId,
        agentId,
        agentName: subAgentName,
        status: "completed",
        timestamp: Date.now(),
        toolCallInteractionId,
      });

      // Persist the terminal status so it survives an app restart.
      await window.snow.updateSubAgentSessionStatus(subConvId, "completed", "");

      // Flush user messages queued while the sub-agent was busy (inserting
      // messages mid-run is allowed). A finished sub-agent conversation no
      // longer accepts messages, so carry them to the parent conversation.
      // This also covers aborted runs: with the sub-conversation input
      // hidden, the queue would otherwise be orphaned and silently lost.
      forwardSubPendingQueue(subConvId);

      // Execute onSubAgentComplete hooks. The hook context includes the
      // sub-agent's summary so prompt-type hooks can inspect the result.
      // If blocked, the error message replaces the summary returned to
      // the parent AI loop.
      let effectiveSummary = summary;
      try {
        const onCompleteContext = JSON.stringify({
          agentId,
          agentName: subAgentName ?? agentId,
          prompt,
          summary,
          parentConversationId,
          cwd: directoryIdToPath(dirId) ?? ctx.directoryPath ?? "",
        });
        const onCompleteResult = await runHook(
          "onSubAgentComplete",
          dirId || undefined,
          onCompleteContext
        );
        if (onCompleteResult) {
          ctx.updateSessionMessages(parentConversationId, (currentMessages) =>
            appendHookExecutionToMessage(currentMessages, {
              ...onCompleteResult.record,
              // Bind to the sub-agent tool call so the hook renders attached
              // to the sub-agent card ("完成" step), not the message footer.
              toolCallInteractionId,
            })
          );
          if (onCompleteResult.outcome.kind === "abort") {
            effectiveSummary = onCompleteResult.outcome.message;
          } else if (
            onCompleteResult.outcome.kind === "pass" &&
            onCompleteResult.outcome.context
          ) {
            effectiveSummary = `${summary}\n\n[Hook Context]\n${onCompleteResult.outcome.context}`;
          } else if (onCompleteResult.outcome.kind === "warn") {
            effectiveSummary = `${summary}\n\n[Hook Warning]\n${onCompleteResult.outcome.message}`;
          }
        }
      } catch {
        // Hook execution failed -- use original summary
      }

      return JSON.stringify({
        success: true,
        conversationId: subConversationId,
        agentName: subAgentName,
        summary: effectiveSummary,
      });
    } catch (err) {
      if (subConversationId) {
        const subCatchRef = ctx.sessionsRefData.current.get(subConversationId);
        // 与正常路径相同的强行发送处理：先消费暂存消息继续回合，
        // 再统一走失败收尾。
        if (subCatchRef?.forceSendMessages?.length) {
          try {
            await runForceSendLoop();
          } catch {
            // 强行发送回合也失败：照常走失败收尾
          }
        }
        if (subCatchRef) {
          // A failed sub-agent conversation is read-only as well.
          subCatchRef.subAgentTerminated = true;
          subCatchRef.isSending = false;
        }
        ctx.updateSessionField(subConversationId, "isStreaming", false);
        ctx.updateSessionField(subConversationId, "streamStartedAt", 0);
        ctx.updateSessionField(subConversationId, "isAborting", false);
        ctx.removeStreamingId(subConversationId);

        // Same ordering as the success path: broadcast the failed status
        // first so the UI hides the input box immediately.
        ctx.setSubAgentSessionEvent({
          parentConversationId,
          conversationId: subConversationId,
          agentId,
          agentName: subAgentName ?? agentId,
          status: "failed",
          timestamp: Date.now(),
          toolCallInteractionId,
        });

        await window.snow
          .updateSubAgentSessionStatus(subConversationId, "failed", "")
          .catch(() => {});

        // Same rationale as the success path: queued insertions must not be
        // lost when the failed sub-agent conversation becomes read-only.
        forwardSubPendingQueue(subConversationId);
      }

      return JSON.stringify({
        success: false,
        error: getErrorMessage(err),
      });
    }
  };
};
