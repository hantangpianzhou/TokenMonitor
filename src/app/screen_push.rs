//! 图灵智显 3.5" 数显推送：扫描完成后拉起内嵌的 TokenScreen.exe 推一帧。
//!
//! 设计要点：
//! - 单文件分发：exe 通过 `include_bytes!` 编进主程序，首次运行时释放到
//!   `%TEMP%` 并复用（大小不符则重写），目标机无需 Python 或第二个文件。
//! - 短命进程：每次 `--once` 推一帧后自行退出，不常驻；因此无生命周期轮询，
//!   仅在 `Drop` 里 kill 残留进程作为退出兜底。
//! - 防重入：上次进程未结束时跳过本次，避免两个进程抢同一串口。
//! - 无黑框：Windows 下用 `CREATE_NO_WINDOW` 抑制子进程的控制台窗口。
//!
//! 本模块只在 `ui` feature（桌面主程序）下编译；TUI 构建不含它。

use std::io;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

/// 内嵌的小屏推送程序（doc/prototype-turing-screen 的 PyInstaller 产物）。
const SCREEN_EXE: &[u8] = include_bytes!("../../assets/TokenScreen.exe");
const EXE_NAME: &str = "TokenScreen.exe";
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub struct ScreenPush {
    child: Option<Child>,
    exe_path: Option<PathBuf>,
}

impl ScreenPush {
    pub fn new() -> Self {
        Self {
            child: None,
            exe_path: None,
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
        std::fs::write(&path, SCREEN_EXE)?;
        self.exe_path = Some(path.clone());
        Ok(path)
    }

    /// 扫描完成回调：拉起一帧推送。
    ///
    /// 返回 `Ok(true)` = 已拉起，`Ok(false)` = 上次仍在运行故跳过，`Err` = 失败。
    pub fn push_frame(&mut self) -> io::Result<bool> {
        // 防重入：上一次还没推完（屏刷一帧要 1.5–2s）就不重复拉起。
        if let Some(child) = self.child.as_mut() {
            match child.try_wait()? {
                None => return Ok(false),
                Some(_) => self.child = None,
            }
        }
        let exe = self.ensure_exe()?;
        let mut cmd = Command::new(exe);
        cmd.arg("--once")
            .arg("--trigger")
            .arg("") // 短命进程用不到触发文件，禁用
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        self.child = Some(cmd.spawn()?);
        Ok(true)
    }
}

impl Drop for ScreenPush {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
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
}
