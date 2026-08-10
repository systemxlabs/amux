//! 极简日志（docs/DESIGN.md §8 可观测性）。
//!
//! 统一格式与级别策略，GUI 与 server 共用（跨进程串起同一链路）：
//! - 输出：全部到 stderr（server 常驻进程、GUI 终端启动）
//! - 格式：`HH:MM:SS.mmm LEVEL [组件] 消息`
//! - 级别：`AMUX_LOG` 环境变量控制（error / warn / info / debug / trace），默认 info
//! - 不引入外部依赖；trace 级用于 ACP 线上原始帧等高频日志（默认关闭）

use std::sync::OnceLock;
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
    if enabled(l) {
        eprintln!("{} {:5} [{}] {}", timestamp(), l.tag(), component, msg);
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
