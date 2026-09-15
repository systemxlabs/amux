//! 连接配置：`~/.amux/app/server.json`（docs/DESIGN.md「连接存储」）。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Server 连接信息：地址与认证 token。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Connection {
    #[serde(default)]
    pub server: String,
    #[serde(default)]
    pub token: String,
}

impl Connection {
    /// 配置完整（地址与 token 均已填写）。
    pub fn is_configured(&self) -> bool {
        !self.server.trim().is_empty() && !self.token.trim().is_empty()
    }

    /// 规范化后的 Server 地址：去掉尾部斜杠。
    pub fn base_url(&self) -> String {
        self.server.trim().trim_end_matches('/').to_string()
    }
}

/// 桌面应用数据目录：`<amux_home>/app`。
pub fn app_dir() -> PathBuf {
    amux_common::paths::amux_home().join("app")
}

/// 连接配置文件路径：`<amux_home>/app/server.json`。
pub fn connection_path() -> PathBuf {
    app_dir().join("server.json")
}

/// 读取连接配置（文件缺失或解析失败时返回空配置）。
pub fn load(path: &Path) -> Connection {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Connection::default();
    };
    match serde_json::from_str(&text) {
        Ok(connection) => connection,
        Err(error) => {
            log::warn!("连接配置解析失败（{}）: {error}", path.display());
            Connection::default()
        }
    }
}

/// 写入连接配置。
pub fn save(path: &Path, connection: &Connection) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建配置目录失败: {e}"))?;
    }
    let text = serde_json::to_string_pretty(connection).map_err(|e| format!("序列化失败: {e}"))?;
    std::fs::write(path, text).map_err(|e| format!("写入 {} 失败: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app").join("server.json");
        assert_eq!(load(&path), Connection::default(), "缺文件时为空配置");

        let connection = Connection {
            server: "https://amux.example.com:34567/".into(),
            token: "tk".into(),
        };
        save(&path, &connection).unwrap();
        let loaded = load(&path);
        assert_eq!(loaded, connection);
        assert!(loaded.is_configured());
        assert_eq!(loaded.base_url(), "https://amux.example.com:34567");

        let text = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["server"], "https://amux.example.com:34567/");
        assert_eq!(value["token"], "tk");
    }

    #[test]
    fn incomplete_connection_is_not_configured() {
        assert!(!Connection::default().is_configured());
        assert!(!Connection {
            server: "https://x".into(),
            token: "  ".into()
        }
        .is_configured());
    }

    #[test]
    fn broken_file_falls_back_to_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(load(&path), Connection::default());
    }
}
