import { memo, useState } from "react";
import { ChevronDown, Loader2, Minimize2, Undo2 } from "lucide-react";
import { useI18n } from "../../../../i18n";
import { MarkdownBlock } from "./markdownRenderer";

type CompactionMessageProps = {
  content: string;
  isStreaming: boolean;
  isRollbackPreparing?: boolean;
  onRollback: () => void;
};

export const CompactionMessage = memo(
  ({
    content,
    isStreaming,
    isRollbackPreparing,
    onRollback,
  }: CompactionMessageProps): React.JSX.Element => {
    const { t } = useI18n();
    const [isExpanded, setIsExpanded] = useState(false);
    const toggleLabel = isExpanded
      ? t("chat.compactionCollapse")
      : t("chat.compactionExpand");

    return (
      <article
        className={`context-compaction-message${
          isExpanded ? " is-expanded" : ""
        }`}
      >
        <div className="context-compaction-message-header">
          <button
            className="context-compaction-message-toggle"
            type="button"
            aria-expanded={isExpanded}
            aria-label={toggleLabel}
            title={toggleLabel}
            onClick={() => setIsExpanded((current) => !current)}
          >
            <span
              className="context-compaction-message-icon"
              aria-hidden="true"
            >
              <Minimize2 size={15} strokeWidth={1.9} />
            </span>
            <span className="context-compaction-message-copy">
              <strong>{t("chat.compactionSummary")}</strong>
              <span className="context-compaction-message-description">
                {t("chat.contextCompacted")}
              </span>
            </span>
            <span
              className="context-compaction-message-action"
              aria-hidden="true"
            >
              {toggleLabel}
              <ChevronDown
                className="context-compaction-message-chevron"
                size={15}
                strokeWidth={1.8}
              />
            </span>
          </button>
          {!isStreaming ? (
            <button
              className="context-compaction-message-rollback"
              type="button"
              aria-label={t("chat.rollbackMessage")}
              title={t("chat.rollbackMessage")}
              disabled={isRollbackPreparing}
              onClick={onRollback}
            >
              {isRollbackPreparing ? (
                <Loader2 size={15} strokeWidth={1.8} className="spin" />
              ) : (
                <Undo2 size={15} strokeWidth={1.8} />
              )}
            </button>
          ) : null}
        </div>
        {isExpanded ? (
          <div className="context-compaction-message-body">
            <MarkdownBlock
              className="context-compaction-markdown"
              content={content}
              streaming={isStreaming}
            />
          </div>
        ) : null}
      </article>
    );
  }
);

CompactionMessage.displayName = "CompactionMessage";
