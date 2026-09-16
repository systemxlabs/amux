//! 统一日志门面。
//!
//! 使用 logforth：默认 `info`，通过 `RUST_LOG` 调整级别，文件按天滚动并保留最近 7 天。
//! 组件只负责在启动时提供自己的日志路径（`~/.amux/logs/<组件>.log`）。
//! 日志布局自带 `file:line`，业务代码直接使用 `log` crate 宏。

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// amux 各 crate 默认 info，依赖库默认 warn。
const DEFAULT_FILTER: &str = "warn,amux=info";
static INITIALIZED: OnceLock<()> = OnceLock::new();

fn current_filter() -> logforth::filter::RustLogFilter {
    logforth::filter::rustlog::RustLogFilterBuilder::from_default_env_or(DEFAULT_FILTER).build()
}

/// 日志目录：`<amux_home>/logs`。
pub fn log_dir() -> PathBuf {
    crate::paths::logs_dir()
}

/// 组件日志文件路径：`<amux_home>/logs/<name>.log`。
pub fn log_path(name: &str) -> PathBuf {
    log_dir().join(format!("{name}.log"))
}

/// 初始化 stderr 与文件日志。重复调用不会替换已安装的全局 logger。
pub fn init_file_output(path: &Path) {
    INITIALIZED.get_or_init(|| {
        let builder = logforth::starter_log::builder().dispatch(|dispatch| {
            dispatch.filter(current_filter()).append(
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
                apply(builder.dispatch(|dispatch| dispatch.filter(current_filter()).append(file)))
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

#[cfg(test)]
mod tests {
    use super::*;
    use logforth::Filter;

    #[test]
    fn default_filter_prioritizes_amux_logs_over_dependencies() {
        let filter =
            logforth::filter::rustlog::RustLogFilterBuilder::from_spec(DEFAULT_FILTER).build();
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

        assert!(allows(
            logforth::record::Level::Info,
            "amux_daemon::connection"
        ));
        assert!(allows(logforth::record::Level::Info, "amux_server::acp"));
        assert!(!allows(logforth::record::Level::Info, "tokio"));
        assert!(allows(logforth::record::Level::Warn, "tokio"));
    }

    #[test]
    fn log_path_lives_under_amux_logs() {
        let path = log_path("daemon");
        assert!(path.ends_with("logs/daemon.log"), "{path:?}");
    }
}
