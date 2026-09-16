//! 机器信息（`machine.info`）。

use amux_common::daemon::MachineInfo;

pub fn machine_info(name: &str) -> MachineInfo {
    MachineInfo {
        name: name.to_string(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        hostname: hostname(),
        temp_dir: std::env::temp_dir().to_string_lossy().into_owned(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

/// 主机名：优先环境变量，其次 `/etc/hostname`，都取不到时用 `unknown`。
fn hostname() -> String {
    if let Ok(value) = std::env::var("HOSTNAME") {
        if !value.is_empty() {
            return value;
        }
    }
    std::fs::read_to_string("/etc/hostname")
        .map(|text| text.trim().to_string())
        .ok()
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machine_info_carries_platform_and_name() {
        let info = machine_info("localpc");
        assert_eq!(info.name, "localpc");
        assert!(!info.os.is_empty());
        assert!(!info.arch.is_empty());
        assert!(!info.temp_dir.is_empty(), "技能操作要用机器的系统临时目录");
        assert_eq!(info.version, env!("CARGO_PKG_VERSION"));
    }
}
