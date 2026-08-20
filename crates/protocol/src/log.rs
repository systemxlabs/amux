//! 极简日志（docs/DESIGN.md §8 可观测性）。
//!
//! 统一格式与级别策略，GUI 与 server 共用（跨进程串起同一链路）：
//! - 输出：stderr + 可选文件（server 启动时初始化为 `~/.amux/logs/server.log`，
//!   GUI 启动时初始化为 `~/.amux/logs/desktop.log`）
//! - 格式：`HH:MM:SS.mmm LEVEL [组件] 消息`
//! - 级别：`AMUX_LOG` 环境变量控制（error / warn / info / debug / trace），默认 info
//! - 文件日志按天切片、保留最近 7 天（docs/DESIGN.md §8）
//! - 不引入外部日期库；UTC 日期计算自实现
//! - trace 级用于 ACP 线上原始帧等高频日志（默认关闭）

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// 日志级别（数值越大越详细）。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Level {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
    Trace = 4,
}

impl Level {
    fn tag(self) -> &'static str {
        match self {
            Level::Error => "ERROR",
            Level::Warn => "WARN",
            Level::Info => "INFO",
            Level::Debug => "DEBUG",
            Level::Trace => "TRACE",
        }
    }
}

static LEVEL: OnceLock<Level> = OnceLock::new();
static APPENDER: OnceLock<Mutex<FileAppender>> = OnceLock::new();

/// 文件追加器：维护当前日志文件，支持按天切片与 7 天保留。
struct FileAppender {
    /// 基路径，如 `~/.amux/logs/server.log`
    base: PathBuf,
    /// 当前文件句柄（打开失败时为 None，日志落入 stderr 兜底）
    file: Option<File>,
    /// 当前打开文件时的 UTC 日期（`yyyy-mm-dd`）
    current_day: String,
}

/// 当前全局级别（由 AMUX_LOG 决定，首次调用时解析）。
fn level() -> Level {
    *LEVEL.get_or_init(|| {
        std::env::var("AMUX_LOG")
            .ok()
            .map(|v| match v.to_ascii_lowercase().as_str() {
                "error" => Level::Error,
                "warn" => Level::Warn,
                "info" => Level::Info,
                "debug" => Level::Debug,
                "trace" => Level::Trace,
                _ => Level::Info,
            })
            .unwrap_or(Level::Info)
    })
}

/// 该级别是否启用。
pub fn enabled(l: Level) -> bool {
    l <= level()
}

/// 初始化文件日志输出（按天切片、保留 7 天）。
///
/// `base_path` 为今日当前日志文件路径（如 `~/.amux/logs/server.log`）。
/// 调用方应在进程启动时调用一次；重复调用无效。
pub fn init_file_output(base_path: &Path) {
    let _ = APPENDER.get_or_init(|| {
        if let Some(parent) = base_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        rotate_if_stale(base_path);
        let file = open_append(base_path);
        let current_day = today_utc();
        cleanup_old_logs(base_path, &current_day);
        Mutex::new(FileAppender {
            base: base_path.to_path_buf(),
            file,
            current_day,
        })
    });
}

fn open_append(path: &Path) -> Option<File> {
    OpenOptions::new().create(true).append(true).open(path).ok()
}

/// 若基路径已存在且不是今天的文件，则按日期归档。
fn rotate_if_stale(base_path: &Path) {
    if !base_path.exists() {
        return;
    }
    let Ok(meta) = fs::metadata(base_path) else {
        return;
    };
    let Ok(mtime) = meta.modified() else {
        return;
    };
    let mtime_day = day_string(system_time_to_days(&mtime));
    let today = today_utc();
    if mtime_day == today {
        return;
    }
    let rotated = rotated_path(base_path, &mtime_day);
    let _ = fs::rename(base_path, rotated);
}

/// 清理超过 7 天的归档日志。
fn cleanup_old_logs(base_path: &Path, today: &str) {
    let Some(parent) = base_path.parent() else {
        return;
    };
    let Ok(today_days) = parse_day(today) else {
        return;
    };
    let prefix = base_stem(base_path);
    let suffix = base_suffix(base_path);
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let expected_prefix = format!("{prefix}-");
        if !name.starts_with(&expected_prefix) || !name.ends_with(&suffix) {
            continue;
        }
        let date_part = &name[expected_prefix.len()..name.len() - suffix.len()];
        let Ok(days) = parse_day(date_part) else {
            continue;
        };
        if today_days.saturating_sub(days) > 7 {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn base_stem(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("log")
        .to_string()
}

fn base_suffix(path: &Path) -> String {
    path.extension()
        .and_then(|s| s.to_str())
        .map(|s| format!(".{s}"))
        .unwrap_or_else(|| ".log".to_string())
}

fn rotated_path(base_path: &Path, day: &str) -> PathBuf {
    let parent = base_path.parent().unwrap_or_else(|| Path::new("."));
    let stem = base_stem(base_path);
    let suffix = base_suffix(base_path);
    parent.join(format!("{stem}-{day}{suffix}"))
}

fn system_time_to_days(t: &SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64 / 86400)
        .unwrap_or(0)
}

fn today_utc() -> String {
    let days = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64 / 86400)
        .unwrap_or(0);
    day_string(days)
}

/// 将 UNIX 天数转换为 `yyyy-mm-dd`。
fn day_string(days: i64) -> String {
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// 解析 `yyyy-mm-dd` 为 UNIX 天数（用于清理旧日志）。
fn parse_day(s: &str) -> Result<i64, ()> {
    if s.len() != 10 {
        return Err(());
    }
    let y: i32 = s[0..4].parse().map_err(|_| ())?;
    let m: u32 = s[5..7].parse().map_err(|_| ())?;
    let d: u32 = s[8..10].parse().map_err(|_| ())?;
    Ok(days_from_civil(y, m, d))
}

/// 自 UNIX 纪元（1970-01-01）起的天数 → 民用日期 (year, month, day)。
fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 {
        z / 146097
    } else {
        (z - 146096) / 146097
    };
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i32 + era as i32 * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as u32, d as u32)
}

/// 民用日期 → 自 UNIX 纪元起的天数。
fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = y as i64 - if m <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y / 400 } else { (y - 399) / 400 };
    let yoe = y - era * 400;
    let mp = if m > 2 { m as i64 - 3 } else { m as i64 + 9 };
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn timestamp() -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let secs = ms / 1000;
    let millis = ms % 1000;
    let (h, m, s) = (secs / 3600 % 24, secs / 60 % 60, secs % 60);
    format!("{h:02}:{m:02}:{s:02}.{millis:03}")
}

fn log(l: Level, component: &str, msg: &str) {
    let line = format!("{} {:5} [{}] {}", timestamp(), l.tag(), component, msg);
    if enabled(l) {
        eprintln!("{line}");
    }
    if let Some(appender) = APPENDER.get() {
        if let Ok(mut a) = appender.lock() {
            a.write_line(&line);
        }
    }
}

impl FileAppender {
    fn write_line(&mut self, line: &str) {
        let today = today_utc();
        if self.current_day != today {
            if let Some(ref mut f) = self.file {
                let _ = f.flush();
            }
            let rotated = rotated_path(&self.base, &self.current_day);
            let _ = fs::rename(&self.base, rotated);
            self.current_day = today.clone();
            self.file = open_append(&self.base);
            cleanup_old_logs(&self.base, &today);
        }
        if let Some(ref mut f) = self.file {
            let _ = writeln!(f, "{line}");
            let _ = f.flush();
        }
    }
}

pub fn error(component: &str, msg: impl AsRef<str>) {
    log(Level::Error, component, msg.as_ref());
}

pub fn warn(component: &str, msg: impl AsRef<str>) {
    log(Level::Warn, component, msg.as_ref());
}

pub fn info(component: &str, msg: impl AsRef<str>) {
    log(Level::Info, component, msg.as_ref());
}

pub fn debug(component: &str, msg: impl AsRef<str>) {
    log(Level::Debug, component, msg.as_ref());
}

pub fn trace(component: &str, msg: impl AsRef<str>) {
    log(Level::Trace, component, msg.as_ref());
}

/// JSON 参数摘要：只保留关键字段，长文本截断（日志可读性）。
/// `keys` 为需要保留的字段名；其余丢弃。
pub fn params_summary(params: &serde_json::Value, keys: &[&str], max_text: usize) -> String {
    use serde_json::Value;
    let Some(obj) = params.as_object() else {
        return String::new();
    };
    let mut parts: Vec<String> = Vec::new();
    for k in keys {
        if let Some(v) = obj.get(*k) {
            let s = match v {
                Value::String(s) => {
                    if s.chars().count() > max_text {
                        let t: String = s.chars().take(max_text).collect();
                        format!("{t}…")
                    } else {
                        s.clone()
                    }
                }
                other => other.to_string(),
            };
            parts.push(format!("{k}={s}"));
        }
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_days_roundtrip() {
        let samples = vec![
            (1970, 1, 1, 0),
            (2000, 1, 1, 10957),
            (2024, 2, 29, 19782),
            (2025, 9, 8, 20339),
        ];
        for (y, m, d, days) in samples {
            assert_eq!(days_from_civil(y, m, d), days, "to_days {y}-{m}-{d}");
            assert_eq!(civil_from_days(days), (y, m, d), "from_days {days}");
        }
    }

    #[test]
    fn day_string_and_parse() {
        let s = day_string(19782);
        assert_eq!(s, "2024-02-29");
        assert_eq!(parse_day(&s), Ok(19782));
        assert!(parse_day("bad").is_err());
    }
}
