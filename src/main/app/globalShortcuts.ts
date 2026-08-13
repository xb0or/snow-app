import { app, globalShortcut } from "electron";
import type { NativeBridge } from "../native/types";
import { getMainWindow } from "./mainWindow";
import { refreshTrayStats, showMainWindow } from "./tray";
import { snowLog } from "../../utils/snowLogger";

/**
 * 显示/隐藏对话窗口全局快捷键（toggleWindow，默认 mod+shift+h）。
 *
 * 与其余 7 个快捷键不同，该快捷键由主进程 globalShortcut 注册：
 * 渲染进程的 keydown 监听只在窗口聚焦时生效（见 useKeyboardShortcuts
 * 的注释），窗口隐藏到托盘后收不到按键事件，无法实现"呼出"。
 * 全局注册后任意状态下都能 toggle，因此该快捷键默认不设"仅台前"。
 *
 * 键格式转换：渲染层规范化格式（mod+shift+h / mod+f / escape）
 * → Electron accelerator（CommandOrControl+Shift+H / ... / Esc）。
 */

/** 当前已注册的 accelerator，重注册前先注销旧绑定。 */
let registeredAccelerator: string | null = null;

/**
 * 规范化键 → Electron accelerator。
 * - mod → CommandOrControl（macOS=Cmd，其他=Ctrl）
 * - ctrl → Control，alt → Alt，shift → Shift
 * - backtick → `，escape → Esc，字母/数字大写
 * 无修饰键的单键组合（如 escape）返回 null：全局注册会抢占整个系统
 * 的该按键（包括其他应用的输入场景），风险过高，拒绝注册。
 */
export const keyToAccelerator = (key: string): string | null => {
  const parts = key.split("+").filter((part) => part.length > 0);
  if (parts.length < 2) {
    return null; // 必须带至少一个修饰键
  }
  const modifiers = parts.slice(0, -1);
  const main = parts[parts.length - 1];

  const acceleratorParts: string[] = [];
  for (const modifier of modifiers) {
    if (modifier === "mod") {
      acceleratorParts.push("CommandOrControl");
    } else if (modifier === "ctrl") {
      acceleratorParts.push("Control");
    } else if (modifier === "alt") {
      acceleratorParts.push("Alt");
    } else if (modifier === "shift") {
      acceleratorParts.push("Shift");
    } else {
      return null; // 未知修饰键，不注册
    }
  }

  let mainPart: string;
  if (main === "backtick") {
    mainPart = "`";
  } else if (main === "escape") {
    mainPart = "Esc";
  } else if (main.length === 1 && /^[a-z0-9]$/i.test(main)) {
    mainPart = main.toUpperCase();
  } else {
    return null;
  }

  return [...acceleratorParts, mainPart].join("+");
};

/**
 * toggle 主窗口：
 * - 可见且聚焦 → 隐藏到托盘（macOS 同时隐藏 Dock 图标，与
 *   window:hide-to-tray 行为一致，并刷新托盘悬停信息）
 * - 其余状态（隐藏 / 最小化 / 失焦）→ 呼出并聚焦（复用托盘恢复逻辑）
 */
const toggleMainWindow = (): void => {
  const win = getMainWindow();
  if (win && win.isVisible() && win.isFocused()) {
    win.hide();
    if (process.platform === "darwin") {
      app.dock?.hide();
    }
    refreshTrayStats();
    return;
  }
  showMainWindow();
};

/**
 * 根据数据库中的 toggleWindow 配置注册/注销全局快捷键。
 * - enabled=false 或键不含修饰键：注销并跳过
 * - 注册失败（组合键被其他应用占用）：记录告警，不打扰用户
 * native 代理已做 storageReady 门控，storage 未就绪时该调用会自动等待。
 */
export const registerToggleWindowShortcut = async (
  native: NativeBridge
): Promise<void> => {
  if (registeredAccelerator) {
    globalShortcut.unregister(registeredAccelerator);
    registeredAccelerator = null;
  }

  const settings = await native.getKeyboardShortcutsSettings();
  const config = settings.toggleWindow;
  if (!config?.enabled) {
    return;
  }

  const accelerator = keyToAccelerator(config.key);
  if (!accelerator) {
    snowLog.warn({
      module: "app/globalShortcuts",
      func: "registerToggleWindowShortcut",
      message: "Toggle window shortcut requires a modifier key, skipped",
      context: `key=${config.key}`,
    });
    return;
  }

  const ok = globalShortcut.register(accelerator, toggleMainWindow);
  if (ok) {
    registeredAccelerator = accelerator;
    snowLog.info({
      module: "app/globalShortcuts",
      func: "registerToggleWindowShortcut",
      message: "Toggle window global shortcut registered",
      context: `accelerator=${accelerator}`,
    });
  } else {
    snowLog.warn({
      module: "app/globalShortcuts",
      func: "registerToggleWindowShortcut",
      message:
        "Failed to register global shortcut, likely taken by another app",
      context: `accelerator=${accelerator}`,
    });
  }
};
