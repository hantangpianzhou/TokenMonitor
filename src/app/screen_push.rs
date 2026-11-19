//! 图灵智显 3.5" 数显推送：管理一个常驻 TokenScreen.exe（--partial 局部刷新）。
//!
//! 设计要点：
//! - 单文件分发：exe 通过 `include_bytes!` 编进主程序，首次运行时释放到
//!   `%TEMP%` 并复用（大小不符则重写），目标机无需 Python 或第二个文件。
//! - 常驻进程：拉起一次 `TokenScreen.exe --partial`，首次 RESET + 全帧建基准，
//!   之后串口保持常开；数据变化 / 时钟每分钟变化只推脏矩形（几 KB、无 RESET、
//!   无黑屏），并每 10 分钟强制全帧校准一次重建基准，自愈屏幕与基准的失步。
//!   短命进程做不到——进程退出关串口后屏幕复位回待机，下一帧局部
//!   数据会打在空屏上（2026-11-19 实机验证）。
//! - 触发通知：扫描完成写一个空触发文件，常驻进程下一轮询（≤15s）推一帧局部。
//! - 生命周期：开关打开 / 程序启动时拉起；开关关闭 / 程序退出时 kill 释放 COM3。
//! - 无黑框：Windows 下用 `CREATE_NO_WINDOW` 抑制子进程的控制台窗口。
//!
//! 本模块只在 `ui` feature（桌面主程序）下编译；TUI 构建不含它。

use std::io;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

/// 内嵌的小屏推送程序（doc/prototype-turing-screen 的 PyInstaller 产物）。
const SCREEN_EXE: &[u8] = include_bytes!("../../assets/TokenScreen.exe");
const EXE_NAME: &str = "TokenScreen.exe";
/// 触发文件名，必须与 Python screen_loop.py 的 DEFAULT_TRIGGER 一致。
const TRIGGER_NAME: &str = "tm-screen-refresh.trigger";
/// 常驻进程轮询间隔：15s 让时钟变化最迟 15s 内上屏（用户要求每分钟即时刷新）。
const POLL_INTERVAL_SECS: u32 = 15;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub struct ScreenPush {
    child: Option<Child>,
    exe_path: Option<PathBuf>,
    trigger_path: Option<PathBuf>,
}

impl ScreenPush {
    pub fn new() -> Self {
        Self {
            child: None,
            exe_path: None,
            trigger_path: None,
        }
    }

    /// 确保 exe 已释放到临时目录；已存在且大小匹配则直接复用。
    fn ensure_exe(&mut self) -> io::Result<PathBuf> {
        if let Some(path) = &self.exe_path {
            let size_ok = std::fs::metadata(path)
                .map(|m| m.len() == SCREEN_EXE.len() as u64)
                .unwrap_or(false);
            if size_ok {
                return Ok(path.clone());
            }
        }
        let dir = std::env::temp_dir().join("tokenmonitor-screen");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(EXE_NAME);
        // 新实例（app 重启、测试）没有缓存路径，但 exe 可能已在磁盘上——且
        // 上一轮的常驻进程可能还在运行它：重写运行中的 exe 会撞共享冲突，
        // ensure_running 就永远起不来。载荷只在长度变化时才算更新，因此
        // 大小匹配就复用磁盘文件，绝不覆盖活进程。
        if let Ok(meta) = std::fs::metadata(&path) {
            if meta.len() == SCREEN_EXE.len() as u64 {
                self.exe_path = Some(path.clone());
                return Ok(path);
            }
        }
        std::fs::write(&path, SCREEN_EXE)?;
        self.exe_path = Some(path.clone());
        Ok(path)
    }

    /// 触发文件路径（惰性缓存），与 Python 侧默认触发文件一致。
    fn trigger_file(&mut self) -> io::Result<PathBuf> {
        if let Some(path) = &self.trigger_path {
            return Ok(path.clone());
        }
        let path = std::env::temp_dir().join(TRIGGER_NAME);
        self.trigger_path = Some(path.clone());
        Ok(path)
    }

    /// 常驻进程是否仍存活；若已退出则清掉失效的 Child 句柄。
    fn alive(&mut self) -> bool {
        let still = match self.child.as_mut() {
            Some(child) => matches!(child.try_wait(), Ok(None)),
            None => false,
        };
        if !still {
            self.child = None;
        }
        still
    }

    /// 确保常驻进程在运行。
    /// 返回 `Ok(true)` = 本次新拉起（首次会 RESET + 全帧建基准），
    /// `Ok(false)` = 已在运行。
    pub fn ensure_running(&mut self) -> io::Result<bool> {
        if self.alive() {
            return Ok(false);
        }
        let exe = self.ensure_exe()?;
        // 清掉上一轮残留的触发文件，避免刚启动就连推两帧。
        let trigger = self.trigger_file()?;
        let _ = std::fs::remove_file(&trigger);

        // 日志落盘：stdout/stderr 写 %TEMP%/tokenmonitor-screen/push.log，
        // 屏幕不亮时可查原因。追加且超限截断。
        let log_path = exe.parent().map(|d| d.join("push.log"));
        let log_file = log_path.as_ref().and_then(|p| {
            if let Ok(m) = std::fs::metadata(p) {
                if m.len() > 512 * 1024 {
                    let _ = std::fs::remove_file(p);
                }
            }
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
                .ok()
        });

        let mut cmd = Command::new(&exe);
        cmd.arg("--partial")
            .arg("--interval")
            .arg(POLL_INTERVAL_SECS.to_string())
            .arg("--orient")
            .arg("landscape")
            .arg("--trigger")
            .arg(&trigger)
            .stdin(Stdio::null());
        match log_file {
            Some(f) => {
                use std::io::Write as _;
                let mut stamp = f.try_clone();
                if let Ok(ref mut s) = stamp {
                    let _ = writeln!(s, "===== screen session start =====");
                }
                if let Ok(out) = f.try_clone() {
                    cmd.stdout(Stdio::from(out));
                } else {
                    cmd.stdout(Stdio::null());
                }
                if let Ok(err) = f.try_clone() {
                    cmd.stderr(Stdio::from(err));
                } else {
                    cmd.stderr(Stdio::null());
                }
            }
            None => {
                cmd.stdout(Stdio::null());
                cmd.stderr(Stdio::null());
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        self.child = Some(cmd.spawn()?);
        Ok(true)
    }

    /// 扫描完成回调：通知常驻进程推一帧。
    /// 进程若意外退出则先重新拉起（首次全帧自带一帧，无需再写触发文件）。
    pub fn notify(&mut self) -> io::Result<()> {
        if !self.alive() {
            self.ensure_running()?;
            return Ok(());
        }
        let trigger = self.trigger_file()?;
        // Python 侧只检查文件是否存在，空文件即可触发。
        std::fs::write(&trigger, b"")?;
        Ok(())
    }

    /// 停止常驻进程并清理触发文件，释放 COM3。
    pub fn stop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.child = None;
        if let Some(path) = &self.trigger_path {
            let _ = std::fs::remove_file(path);
        }
    }
}

impl Drop for ScreenPush {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Single-file distribution hinges on the embedded exe being released to
    /// a usable path with the exact embedded byte count. Pin that invariant.
    #[test]
    fn embedded_exe_releases_with_matching_size() {
        let mut push = ScreenPush::new();
        let path = push.ensure_exe().expect("release embedded exe");
        assert_eq!(path.file_name().unwrap().to_string_lossy(), EXE_NAME);
        let size = std::fs::metadata(&path).expect("stat released exe").len();
        assert_eq!(size, SCREEN_EXE.len() as u64);
        assert!(size > 0, "embedded exe must not be empty");
    }

    /// A second release reuses the same file (idempotent), so repeated scans
    /// never rewrite the 17 MB payload.
    #[test]
    fn repeated_release_reuses_cached_path() {
        let mut push = ScreenPush::new();
        let first = push.ensure_exe().expect("first release");
        let second = push.ensure_exe().expect("second release");
        assert_eq!(first, second);
    }

    /// Trigger file path must match the Python default so notify() is observed.
    #[test]
    fn trigger_file_matches_python_default() {
        let mut push = ScreenPush::new();
        let path = push.trigger_file().expect("trigger path");
        assert_eq!(path.file_name().unwrap().to_string_lossy(), TRIGGER_NAME);
        assert_eq!(path, std::env::temp_dir().join("tm-screen-refresh.trigger"));
    }
}
