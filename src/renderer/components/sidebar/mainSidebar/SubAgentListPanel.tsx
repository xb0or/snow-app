import {
  AlertCircle,
  Bot,
  CheckCircle2,
  CircleAlert,
  Loader2,
} from "lucide-react";

import { useI18n } from "../../../i18n";
import type { ChatConversationRecord } from "../../../../preload";

type SubAgentListPanelProps = {
  conversations: ChatConversationRecord[];
  activeConversationId?: string;
  /** 待用户确认的子代理会话 id 集合（提问或工具授权） */
  attentionRequiredConversationIds?: Set<string>;
  onSelect?: (conversationId: string) => void;
};

function renderStatusIcon(
  status: string,
  isAttentionRequired: boolean
): React.ReactNode {
  if (isAttentionRequired) {
    return <CircleAlert size={11} className="sub-agent-attention" />;
  }
  if (status === "running") {
    return <Loader2 size={11} className="spin" />;
  }
  if (status === "failed") {
    return <AlertCircle size={11} className="sub-agent-failed" />;
  }
  if (status === "completed") {
    return <CheckCircle2 size={11} className="sub-agent-completed" />;
  }
  return <Bot size={11} />;
}

/**
 * 子代理列表面板：独立的自包含面板，拥有自己的表面背景，
 * 不依赖父级会话项的选中/悬停状态，避免嵌套背景互相冲突。
 *
 * 注意：子代理会话不允许单独删除（随父会话级联删除），
 * 因此不参与多选批量删除，列表项仅支持点击打开会话。
 */
export function SubAgentListPanel({
  conversations,
  activeConversationId,
  attentionRequiredConversationIds,
  onSelect,
}: SubAgentListPanelProps): React.JSX.Element {
  const { t } = useI18n();

  const handleItemClick = (
    event: React.MouseEvent,
    conversationId: string
  ): void => {
    // 面板是独立交互区域，阻止点击事件继续冒泡
    event.stopPropagation();
    onSelect?.(conversationId);
  };

  const attentionDescription = t("sidebar.chatStatusWaitingForReviewOrInput", {
    defaultValue: "Waiting for review or input",
  });

  return (
    <div className="sub-agent-list-panel">
      {conversations.map((subAgent) => {
        const isAttentionRequired =
          attentionRequiredConversationIds?.has(subAgent.conversationId) ??
          false;
        return (
          <div
            key={subAgent.conversationId}
            className={`sub-agent-list-item${
              subAgent.conversationId === activeConversationId ? " active" : ""
            }`}
            onClick={(event) =>
              handleItemClick(event, subAgent.conversationId)
            }
            role="button"
            tabIndex={0}
            onKeyDown={(event) => {
              if (event.key === "Enter" || event.key === " ") {
                event.preventDefault();
                event.stopPropagation();
                onSelect?.(subAgent.conversationId);
              }
            }}
          >
            <span className="sub-agent-list-icon">
              {renderStatusIcon(subAgent.subAgentStatus, isAttentionRequired)}
            </span>
            <span className="sub-agent-list-name-row">
              <span className="sub-agent-list-name">
                {subAgent.subAgentName ||
                  subAgent.title ||
                  t("sidebar.subAgent", { defaultValue: "Sub-agent" })}
              </span>
              {isAttentionRequired && (
                <span
                  className="chat-item-status-label attention-required"
                  title={attentionDescription}
                  aria-label={attentionDescription}
                >
                  {t("sidebar.chatStatusNeedsAction", {
                    defaultValue: "Needs action",
                  })}
                </span>
              )}
            </span>
          </div>
        );
      })}
    </div>
  );
}
