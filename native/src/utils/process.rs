//! 统一的子进程创建工具。
//!
//! Windows 下 GUI 宿主（Electron 主进程）spawn 控制台程序（reg.exe、
//! git.exe、rg.exe、shell 等）时，若不带 `CREATE_NO_WINDOW` 标志，
//! 系统会为子进程新建一个控制台窗口，表现为"cmd 窗口一闪而过"。
//! 该标志是 `CreateProcess` 的 per-process 标志，不存在系统级全局
//! 开关，因此统一收敛到本模块：内部子进程创建一律使用 [`cmd`] /
//! [`cmd_async`]，禁止直接 `Command::new`，避免遗漏。
//!
//! 例外：需要用户可见窗口的 spawn（启动 IDE、终端会话等）不在此列。

use std::ffi::OsStr;

/// Windows 下隐藏子进程控制台窗口的创建标志。
#[cfg(target_os = "windows")]
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 创建隐藏控制台窗口的同步子进程命令。
///
/// Windows 下自动携带 `CREATE_NO_WINDOW`；其它平台该标志无意义，
/// 行为与 `Command::new` 一致。
pub fn cmd(program: impl AsRef<OsStr>) -> std::process::Command {
    #[cfg_attr(not(target_os = "windows"), allow(unused_mut))]
    let mut command = std::process::Command::new(program);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

/// 创建隐藏控制台窗口的异步子进程命令（tokio 版）。
pub fn cmd_async(program: impl AsRef<OsStr>) -> tokio::process::Command {
    #[cfg_attr(not(target_os = "windows"), allow(unused_mut))]
    let mut command = tokio::process::Command::new(program);
    #[cfg(target_os = "windows")]
    {
        // tokio::process::Command 的 creation_flags 是 inherent method，
        // 无需引入 CommandExt trait。
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}
