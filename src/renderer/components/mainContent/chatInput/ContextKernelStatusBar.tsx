import { useCallback, useEffect, useState } from "react";

import type { ContextKernelStatus } from "../../../../preload";
import { useI18n } from "../../../i18n";

type ContextKernelStatusBarProps = {
  conversationId: string | null;
  /** API 档案的 `contextKernelEnabled`；关闭时不渲染。 */
  enabled: boolean;
  /** 用量变化时触发刷新（例如 tokenUsage 快照）。 */
  refreshKey?: string;
};

const formatTokens = (value: number | null | undefined): string => {
  if (value == null || value <= 0) return "0";
  if (value >= 1_000_000) return `${(value / 1_000_000).toFixed(1)}M`;
  if (value >= 1000) return `${(value / 1000).toFixed(1)}k`;
  return String(Math.round(value));
};

/**
 * 输入区上沿的上下文内核状态条：块数量、出站/阈值 token、占位条目数，
 * 展开后列出每个块。只读展示，数据来自 `context-kernel:get-status`。
 */
export const ContextKernelStatusBar = ({
  conversationId,
  enabled,
  refreshKey = "",
}: ContextKernelStatusBarProps): React.JSX.Element | null => {
  const { t } = useI18n();
  const [status, setStatus] = useState<ContextKernelStatus | null>(null);
  const [expanded, setExpanded] = useState(false);

  const load = useCallback(async () => {
    if (!enabled || !conversationId) {
      setStatus(null);
      return;
    }
    try {
      const next = await window.snow.getContextKernelStatus(conversationId);
      setStatus(next);
    } catch {
      setStatus(null);
    }
  }, [conversationId, enabled]);

  useEffect(() => {
    void load();
  }, [load, refreshKey]);

  if (!enabled || !status) return null;

  const used = Math.max(status.estimatedTokens, status.observedTokens ?? 0);
  const threshold = status.thresholdTokens ?? 0;
  const ratio = threshold > 0 ? Math.min(1, used / threshold) : 0;
  // 主题语义色：正常 / 接近阈值 / 已超阈值。
  const tone =
    ratio >= 1
      ? "var(--danger)"
      : ratio >= 0.8
        ? "var(--warning)"
        : "var(--color-success)";

  return (
    <div
      className="context-kernel-status-bar"
      style={{
        display: "flex",
        flexDirection: "column",
        gap: 4,
        padding: "2px 4px 6px",
        fontSize: 11,
        color: "var(--text-secondary)",
      }}
    >
      <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
        <span
          aria-hidden="true"
          style={{
            width: 6,
            height: 6,
            borderRadius: 999,
            background: tone,
            flexShrink: 0,
          }}
        />
        <span style={{ color: "var(--text-primary)", fontWeight: 600 }}>
          {t("chat.contextKernel", { defaultValue: "上下文内核" })}
        </span>
        <span>
          {t("chat.contextKernelBlocks", {
            defaultValue: "{count} 块",
          }).replace("{count}", String(status.activeBlocks))}
        </span>
        <span aria-hidden="true">·</span>
        <span
          title={t("chat.contextKernelTokensHint", {
            defaultValue: "本地估算 / 最近一次真实用量",
          })}
        >
          {t("chat.contextKernelTokens", {
            defaultValue: "{used} / {threshold} tokens",
          })
            .replace("{used}", formatTokens(used))
            .replace("{threshold}", formatTokens(threshold))}
        </span>
        {status.highestTier > 0 ? (
          <>
            <span aria-hidden="true">·</span>
            <span>
              {t("chat.contextKernelTier", {
                defaultValue: "层级 {tier}/{max}",
              })
                .replace("{tier}", String(status.highestTier))
                .replace("{max}", String(status.maxTier))}
            </span>
          </>
        ) : null}
        {status.storedEntries > 0 ? (
          <>
            <span aria-hidden="true">·</span>
            <span>
              {t("chat.contextKernelStored", {
                defaultValue: "{count} 条占位",
              }).replace("{count}", String(status.storedEntries))}
            </span>
          </>
        ) : null}
        <span style={{ marginLeft: "auto", display: "flex", gap: 6 }}>
          <button
            type="button"
            onClick={() => void load()}
            style={linkButtonStyle}
          >
            {t("chat.contextKernelRefresh", { defaultValue: "刷新" })}
          </button>
          {status.blocks.length > 0 ? (
            <button
              type="button"
              onClick={() => setExpanded((current) => !current)}
              style={linkButtonStyle}
            >
              {expanded
                ? t("chat.contextKernelCollapse", { defaultValue: "收起" })
                : t("chat.contextKernelExpand", { defaultValue: "详情" })}
            </button>
          ) : null}
        </span>
      </div>
      {expanded && status.blocks.length > 0 ? (
        <div
          style={{
            display: "flex",
            flexDirection: "column",
            gap: 2,
            paddingLeft: 14,
            color: "var(--text-muted)",
          }}
        >
          {status.blocks.map((block) => (
            <div
              key={block.blockId}
              style={{
                display: "flex",
                gap: 6,
                alignItems: "baseline",
                minWidth: 0,
              }}
            >
              <span style={{ color: "var(--text-link)", flexShrink: 0 }}>
                {block.blockId}
                {block.tier > 1 ? ` T${block.tier}` : ""}
              </span>
              <span
                style={{
                  color: "var(--text-primary)",
                  overflow: "hidden",
                  textOverflow: "ellipsis",
                  whiteSpace: "nowrap",
                }}
                title={block.title}
              >
                {block.title}
              </span>
              <span style={{ flexShrink: 0 }}>
                {t("chat.contextKernelBlockMeta", {
                  defaultValue: "{messages} 条 · {tokens} tokens",
                })
                  .replace("{messages}", String(block.coveredMessages))
                  .replace("{tokens}", formatTokens(block.summaryTokens))}
                {block.children.length > 0
                  ? ` · ${t("chat.contextKernelMerged", {
                      defaultValue: "合并 {count} 块",
                    }).replace("{count}", String(block.children.length))}`
                  : ""}
              </span>
            </div>
          ))}
        </div>
      ) : null}
    </div>
  );
};

const linkButtonStyle = {
  border: "none",
  background: "transparent",
  color: "var(--text-link)",
  cursor: "pointer",
  fontSize: 11,
  padding: 0,
} as const;
