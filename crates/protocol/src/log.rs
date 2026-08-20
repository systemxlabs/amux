//! 统一日志门面（docs/DESIGN.md §8 可观测性）。
//!
//! 日志实现使用 logforth：默认 `info`，通过 `RUST_LOG` 调整级别，文件按天滚动并保留
//! 最近 7 个日志文件。应用和 server 只负责在启动时提供各自的日志路径。

use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::OnceLock;

use logforth::Filter;

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
    fn as_logforth(self) -> logforth::record::Level {
        match self {
            Self::Error => logforth::record::Level::Error,
            Self::Warn => logforth::record::Level::Warn,
            Self::Info => logforth::record::Level::Info,
            Self::Debug => logforth::record::Level::Debug,
            Self::Trace => logforth::record::Level::Trace,
        }
    }
}

const DEFAULT_FILTER: &str = "warn,gui=info,server=info,acp=info";
static INITIALIZED: OnceLock<()> = OnceLock::new();

fn current_filter() -> logforth::filter::EnvFilter {
    logforth::filter::env_filter::EnvFilterBuilder::from_default_env_or(DEFAULT_FILTER).build()
}

/// 初始化 stderr 与文件日志。重复调用不会替换已经安装的全局 logger。
pub fn init_file_output(path: &Path) {
    INITIALIZED.get_or_init(|| {
        let filter = current_filter;
        let builder = logforth::starter_log::builder().dispatch(|dispatch| {
            dispatch.filter(filter()).append(
                logforth::append::Stderr::default()
                    .with_layout(logforth::layout::TextLayout::default()),
            )
        });

        let Some(parent) = path.parent() else {
            apply(builder);
            return;
        };
        let Some(filename) = path.file_name().and_then(|name| name.to_str()) else {
            eprintln!("日志文件路径无效: {}", path.display());
            apply(builder);
            return;
        };

        let file = logforth::append::file::FileBuilder::new(parent, filename)
            .layout(logforth::layout::TextLayout::default().no_color())
            .rollover_daily()
            .max_log_files(NonZeroUsize::new(7).expect("7 非零"))
            .build();
        match file {
            Ok(file) => {
                let builder = builder.dispatch(|dispatch| dispatch.filter(filter()).append(file));
                apply(builder);
            }
            Err(error) => {
                eprintln!("初始化文件日志失败（{}）: {error}", path.display());
                apply(builder);
            }
        }
    });
}

fn apply(builder: logforth::starter_log::LogStarterBuilder) {
    if let Err(error) = builder.try_apply() {
        eprintln!("初始化日志系统失败: {error}");
    }
}

/// 判断指定 target 的日志级别是否可能被当前 logger 输出。
pub fn enabled_for(level: Level, target: &str) -> bool {
    let filter = current_filter();
    let criteria = logforth::record::FilterCriteria::builder()
        .level(level.as_logforth())
        .target(target)
        .build();
    matches!(
        filter.enabled(&criteria, &[]),
        logforth::filter::FilterResult::Accept | logforth::filter::FilterResult::Neutral
    )
}

pub fn error(component: &str, msg: impl AsRef<str>) {
    log::error!(target: component, "{}", msg.as_ref());
}

pub fn warn(component: &str, msg: impl AsRef<str>) {
    log::warn!(target: component, "{}", msg.as_ref());
}

pub fn info(component: &str, msg: impl AsRef<str>) {
    log::info!(target: component, "{}", msg.as_ref());
}

pub fn debug(component: &str, msg: impl AsRef<str>) {
    log::debug!(target: component, "{}", msg.as_ref());
}

pub fn trace(component: &str, msg: impl AsRef<str>) {
    log::trace!(target: component, "{}", msg.as_ref());
}

/// JSON 参数摘要：只保留关键字段，长文本截断（日志可读性）。
/// `keys` 为需要保留的字段名；其余丢弃。
pub fn params_summary(params: &serde_json::Value, keys: &[&str], max_text: usize) -> String {
    use serde_json::Value;

    let Some(obj) = params.as_object() else {
        return String::new();
    };
    let mut parts = Vec::new();
    for key in keys {
        if let Some(value) = obj.get(*key) {
            let value = match value {
                Value::String(value) if value.chars().count() > max_text => {
                    let prefix: String = value.chars().take(max_text).collect();
                    format!("{prefix}…")
                }
                Value::String(value) => value.clone(),
                value => value.to_string(),
            };
            parts.push(format!("{key}={value}"));
        }
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_summary_keeps_requested_fields_and_truncates_text() {
        let params = serde_json::json!({
            "sessionId": "s1",
            "input": "0123456789",
            "ignored": true
        });
        assert_eq!(
            params_summary(&params, &["sessionId", "input"], 5),
            "sessionId=s1 input=01234…"
        );
    }

    #[test]
    fn default_filter_prioritizes_app_logs_over_dependencies() {
        let filter =
            logforth::filter::env_filter::EnvFilterBuilder::from_spec(DEFAULT_FILTER).build();
        let allows = |level, target| {
            let criteria = logforth::record::FilterCriteria::builder()
                .level(level)
                .target(target)
                .build();
            matches!(
                filter.enabled(&criteria, &[]),
                logforth::filter::FilterResult::Accept | logforth::filter::FilterResult::Neutral
            )
        };

        assert!(allows(logforth::record::Level::Info, "server.startup"));
        assert!(allows(logforth::record::Level::Info, "gui.ws"));
        assert!(!allows(logforth::record::Level::Info, "tokio"));
        assert!(allows(logforth::record::Level::Warn, "tokio"));
    }
}
