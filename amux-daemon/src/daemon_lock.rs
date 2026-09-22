//! Daemon 单实例锁：按机器名锁定，锁由操作系统在进程退出时自动释放。

use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

use amux_common::paths;

/// 持有锁文件句柄；进程退出或崩溃后由操作系统释放排他锁。
#[derive(Debug)]
#[must_use = "dropping the lock releases the daemon lock"]
pub struct DaemonLock {
    _file: File,
}

/// 获取当前机器名的 Daemon 排他锁。
pub fn acquire(machine: &str) -> Result<DaemonLock, String> {
    acquire_in(&paths::daemon_dir(), machine)
}

/// 锁文件路径：`<amux_home>/daemon/<机器名>.lock`。
pub fn lock_path(machine: &str) -> PathBuf {
    lock_path_in(&paths::daemon_dir(), machine)
}

fn acquire_in(dir: &Path, machine: &str) -> Result<DaemonLock, String> {
    std::fs::create_dir_all(dir)
        .map_err(|error| format!("创建 Daemon 锁目录失败（{}）: {error}", dir.display()))?;
    let path = lock_path_in(dir, machine);
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|error| format!("打开 Daemon 锁文件失败（{}）: {error}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(DaemonLock { _file: file }),
        Err(TryLockError::WouldBlock) => Err(format!(
            "机器「{machine}」已有 Daemon 正在运行，锁文件: {}",
            path.display()
        )),
        Err(TryLockError::Error(error)) => Err(format!(
            "获取 Daemon 排他锁失败（{}）: {error}",
            path.display()
        )),
    }
}

fn lock_path_in(dir: &Path, machine: &str) -> PathBuf {
    dir.join(format!("{machine}.lock"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_path_uses_raw_machine_name() {
        let path = lock_path_in(Path::new("/tmp/amux"), "开发机 A");
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("开发机 A.lock")
        );
    }

    #[test]
    fn exclusive_lock_rejects_duplicate_machine_and_releases_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let first = acquire_in(dir.path(), "pc").unwrap();
        let error = acquire_in(dir.path(), "pc").unwrap_err();
        assert!(error.contains("已有 Daemon 正在运行"), "{error}");

        drop(first);
        let _lock = acquire_in(dir.path(), "pc").unwrap();
    }
}
