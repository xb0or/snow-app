/**
 * 独立浏览器窗口入口（右侧面板浏览器 tab「在新窗口中打开」）。
 *
 * 复用 BrowserPanelContent 完整浏览器 UI；instanceId 经 query 参数从
 * 主窗口继承，因此 browser.rs 的 MCP 浏览器工具按 instanceId 路由命令时
 * 仍可操作本窗口内的实例（主进程 browserCommandBroker 按实例归属转发）。
 */
import { createRoot } from "react-dom/client";
import { useEffect, useMemo, useState } from "react";
// 主题设计令牌与预设变量必须与主窗口保持一致地引入（styles.css 内的
// var(--surface-island-strong) / --accent-color 等依赖 tokens 与预设文件，
// 缺失会导致弹窗背景透明、呼吸灯动画失效）。
import "./styles.css";
import "./themes/tokens.css";
import "./themes/preset-cream.css";
import "./themes/preset-google.css";
import { I18nProvider } from "./i18n";
import { useTheme } from "./hooks/useTheme";
import { applyThemeCacheToDocument } from "./components/sidebar/themeSettings/themeSettingsUtils";
import { INSERT_ELEMENT_TAG_EVENT } from "./components/mainContent/chatInput/fileTagUtils";
import type { ElementTag } from "./components/mainContent/chatInput/fileTagUtils";
import { APP_CONTROL_OPEN_SETTINGS_EVENT } from "./hooks/useAppControl";
import { BrowserPanelContent } from "./components/rightPanel/BrowserPanelContent";
import {
  useBrowserMcpCommandBridge,
  type BrowserMcpTabCallbacks,
} from "./components/rightPanel/browser/useBrowserMcpCommandBridge";

const DEFAULT_TITLE = "Snow Browser";

// 与主窗口 main.tsx 一致：React 渲染前同步应用 localStorage 缓存的
// 主题快照，使首屏即呈现用户上次选择的主题（避免白闪与 CSS 变量缺失）。
applyThemeCacheToDocument();

function DetachedBrowserWindowApp(): React.JSX.Element {
  // 主题 CSS 变量由主窗口启动时注入，独立窗口必须自行应用
  // （读 Rust 后端持久化的 ThemeSettings → 注入 document 变量）。
  useTheme();

  const params = new URLSearchParams(window.location.search);
  const instanceId = params.get("instanceId") ?? "";
  const initialUrl = params.get("url") ?? "";
  // 主窗口「在新窗口中打开」时携带的实例内部标签页快照（激活页置首），
  // 解析失败或缺失时退化为单标签页（initialUrl）。
  const initialTabs = useMemo<{ url: string; title: string }[] | undefined>(
    () => {
      const raw = params.get("tabs");
      if (!raw) {
        return undefined;
      }
      try {
        const parsed: unknown = JSON.parse(raw);
        if (!Array.isArray(parsed)) {
          return undefined;
        }
        const tabs = parsed.filter(
          (tab): tab is { url: string; title: string } =>
            !!tab &&
            typeof tab === "object" &&
            typeof (tab as Record<string, unknown>).url === "string" &&
            typeof (tab as Record<string, unknown>).title === "string"
        );
        return tabs.length > 0 ? tabs : undefined;
      } catch {
        return undefined;
      }
    },
    // eslint-disable-next-line react-hooks/exhaustive-deps
    []
  );
  const [title, setTitle] = useState("");

  // 页面标题（onTitleChange 来自 webview）同步到窗口标题栏。
  useEffect(() => {
    document.title = title || DEFAULT_TITLE;
  }, [title]);

  // 独立窗口固定承载一个实例：create 语义不符直接报错（命令不带
  // instanceId 时由主窗口处理，不会路由到这里）；close/focus/list 均
  // 作用于本窗口自身。
  const browserMcpCallbacks = useMemo<BrowserMcpTabCallbacks>(
    () => ({
      openTab: () => {
        throw new Error(
          "Cannot create a new browser instance inside a detached browser window"
        );
      },
      closeTab: (targetInstanceId: string): boolean => {
        if (targetInstanceId !== instanceId) {
          return false;
        }
        window.close();
        return true;
      },
      focusTab: (targetInstanceId: string): boolean => {
        if (targetInstanceId !== instanceId) {
          return false;
        }
        window.focus();
        return true;
      },
      listTabs: () => [
        { instanceId, title: title || DEFAULT_TITLE, isActive: true },
      ],
    }),
    [instanceId, title]
  );

  // 独立浏览器窗口无右侧面板折叠概念（窗口自身即浏览器），isCollapsed 固定 false。
  useBrowserMcpCommandBridge(browserMcpCallbacks, false);

  // 元素选择确认后，本窗口派发 INSERT_ELEMENT_TAG_EVENT（useWebviewElementPicker
  // 的 confirmPicker）。本窗口没有 ChatInputView，事件无法跨渲染进程到达主窗口，
  // 因此拦截并转发给主窗口，由其聊天输入框插入 element chip。
  useEffect(() => {
    const handleElementTag = (event: Event): void => {
      const tag = (event as CustomEvent<ElementTag>).detail;
      if (!tag) {
        return;
      }
      window.snow.forwardElementTagToChat({
        url: tag.url,
        tag: tag.tag,
        label: tag.label,
        text: tag.text,
        note: tag.note,
      });
    };
    window.addEventListener(INSERT_ELEMENT_TAG_EVENT, handleElementTag);
    return () => {
      window.removeEventListener(INSERT_ELEMENT_TAG_EVENT, handleElementTag);
    };
  }, []);

  // 「浏览器设置」菜单项：BrowserPanelContent 在窗口内派发
  // APP_CONTROL_OPEN_SETTINGS_EVENT，但本窗口没有 Sidebar 监听该事件，
  // 因此拦截并转发给主窗口（主进程聚焦主窗口后由 Sidebar 打开设置）。
  useEffect(() => {
    const handleOpenSettings = (event: Event): void => {
      const detail = (event as CustomEvent<{ view?: string }>).detail;
      window.snow.forwardOpenSettingsToMain(
        detail?.view ?? "browser-settings"
      );
    };
    window.addEventListener(APP_CONTROL_OPEN_SETTINGS_EVENT, handleOpenSettings);
    return () => {
      window.removeEventListener(
        APP_CONTROL_OPEN_SETTINGS_EVENT,
        handleOpenSettings
      );
    };
  }, []);

  return (
    // 浏览器 UI 内部大量使用 useI18n()，必须提供 I18nProvider，
    // 否则组件树整体抛错（主窗口由 App 统一包裹，独立窗口需自行包裹）。
    <I18nProvider>
      <BrowserPanelContent
        instanceId={instanceId}
        initialUrl={initialUrl}
        initialTabs={initialTabs}
        isActive
        detached
        onTitleChange={setTitle}
      />
    </I18nProvider>
  );
}

const container = document.getElementById("browser-window-root");
if (container) {
  createRoot(container).render(<DetachedBrowserWindowApp />);
}
