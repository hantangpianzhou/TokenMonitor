//! Trae 自动采集：扫描完成后拉起内嵌的 TraeCollector.exe（`--auto`）写库。
//!
//! 数据流（与内置 TraeSource 完全独立）：
//!   Trae 本地加密库 database.db（SQLCipher 4）
//!     --TraeCollector.exe --auto--> tokenmonitor.db.usage_records (provider='trae')
//!
//! 设计要点：
//! - `--auto` 优先用缓存 key（HMAC 校验当前库），命中即解密+幂等写库；缓存失效
//!   且不在 30 分钟冷却期时会扫内存提 key（≤180s，需 Trae 活跃且近期有 agent
//!   任务），让新电脑/换 key 后能自愈。提 key 失败写冷却并静默跳过。全程日志
//!   落盘到 `%APPDATA%\TokenMonitor\trae_ingest.log`（子进程 stdout 被丢弃）。
//! - 单文件分发：exe 通过 `include_bytes!` 编进主程序，首次释放到 `%TEMP%` 并复用。
//! - 短命进程 + 防重入：每次 --auto 写完自退；上一次还在跑就跳过本轮。
//! - 无黑框：Windows 用 `CREATE_NO_WINDOW`。
//! - 完成通知：起一个后台线程 `wait()`，退出后通过 `done_tx` 通知 app 刷新视图，
//!   让本轮采集的数据无需等下一个 5 分钟周期即可显示（本轮 refresh 可能早于写库）。
//!
//! 本模块只在 `ui` feature（桌面主程序）下编译。

use std::io;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_channel::Sender;

/// 内嵌的 Trae 采集程序（scripts/trae_collector.py 的 PyInstaller 产物）。
const INGEST_EXE: &[u8] = include_bytes!("../../assets/TokenCollector.exe");
const EXE_NAME: &str = "TokenCollector.exe";
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 采集完成通知（bool 保留给将来区分"有/无新数据"，当前 app 收到即刷新）。
pub type IngestDone = Sender<bool>;

#[derive(Clone)]
pub struct TraeIngest {
    exe_path: Option<Arc<PathBuf>>,
    /// 防重入：进程在跑时为 true；等待线程负责清零。
    busy: Arc<AtomicBool>,
    done_tx: IngestDone,
}

impl TraeIngest {
    pub fn new(done_tx: IngestDone) -> Self {
        Self {
            exe_path: None,
            busy: Arc::new(AtomicBool::new(false)),
            done_tx,
        }
    }

    fn ensure_exe(&mut self) -> io::Result<PathBuf> {
        if let Some(path) = &self.exe_path {
            let size_ok = std::fs::metadata(path.as_ref())
                .map(|m| m.len() == INGEST_EXE.len() as u64)
                .unwrap_or(false);
            if size_ok {
                return Ok((**path).clone());
            }
        }
        let dir = std::env::temp_dir().join("tokenmonitor-trae");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(EXE_NAME);
        // 新实例没有缓存路径，但 exe 可能已在磁盘上——且上一轮采集进程可能
        // 还在运行它：重写运行中的 exe 会撞共享冲突。载荷只在长度变化时才算
        // 更新，大小匹配就复用磁盘文件。
        if let Ok(meta) = std::fs::metadata(&path) {
            if meta.len() == INGEST_EXE.len() as u64 {
                let path = Arc::new(path);
                self.exe_path = Some(path.clone());
                return Ok((*path).clone());
            }
        }
        std::fs::write(&path, INGEST_EXE)?;
        let path = Arc::new(path);
        self.exe_path = Some(path.clone());
        Ok((*path).clone())
    }

    /// 拉起一次自动采集；返回 Ok(true) = 已拉起，Ok(false) = 上次仍在运行故跳过。
    pub fn ingest_once(&mut self) -> io::Result<bool> {
        // 防重入：compare_exchange 保证并发调用也只有一个能起进程。
        if self
            .busy
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Ok(false);
        }

        let result = (|| -> io::Result<()> {
            let exe = self.ensure_exe()?;
            let mut cmd = Command::new(exe);
            cmd.arg("--auto")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                cmd.creation_flags(CREATE_NO_WINDOW);
            }
            let mut child = cmd.spawn()?;

            // 等待线程：退出后清 busy 并通知 app 刷新。用 clone 的 Sender / Arc，
            // 不持有 self，任务生命周期独立于 app 的这一帧。
            let busy = self.busy.clone();
            let done = self.done_tx.clone();
            std::thread::Builder::new()
                .name("tokenmonitor-trae-ingest".into())
                .spawn(move || {
                    let _ = child.wait();
                    busy.store(false, Ordering::SeqCst);
                    // 后台 std 线程不用 async：send_blocking 同步送达。
                    let _ = done.send_blocking(true);
                })?;
            Ok(())
        })();

        if let Err(e) = result {
            self.busy.store(false, Ordering::SeqCst);
            return Err(e);
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_ingest_exe_releases_with_matching_size() {
        let (tx, _rx) = async_channel::unbounded();
        let mut ingest = TraeIngest::new(tx);
        let path = ingest.ensure_exe().expect("release embedded exe");
        assert_eq!(path.file_name().unwrap().to_string_lossy(), EXE_NAME);
        let size = std::fs::metadata(&path).expect("stat").len();
        assert_eq!(size, INGEST_EXE.len() as u64);
        assert!(size > 0, "embedded exe must not be empty");
    }

    #[test]
    fn repeated_release_reuses_cached_path() {
        let (tx, _rx) = async_channel::unbounded();
        let mut ingest = TraeIngest::new(tx);
        let first = ingest.ensure_exe().unwrap();
        let second = ingest.ensure_exe().unwrap();
        assert_eq!(first, second);
    }
}
