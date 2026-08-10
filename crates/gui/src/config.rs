//! GUI 本地配置（docs/DESIGN.md §6 / PRD §3.4）：机器注册表、通知偏好、快捷按钮。
//! 纯逻辑与 IO 分离：配置形状 + 校验/归一化 + 增删改查为可单测纯逻辑；
//! 持久化后端可注入（测试用临时目录）。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

// ---- 配置形状 ----

/// 注册机器（PRD §4.7：接入 / 移除 / 连接配置）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MachineConfig {
    pub id: String,
    pub name: String,
    /// ws://host:port
    pub url: String,
    pub token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NotifyPrefs {
    pub work_ended: bool,
    pub on_error: bool,
    pub long_idle_seconds: u64,
}

impl Default for NotifyPrefs {
    fn default() -> Self {
        NotifyPrefs {
            work_ended: true,
            on_error: true,
            long_idle_seconds: 300,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuiConfig {
    pub version: u32,
    pub machines: Vec<MachineConfig>,
    pub notify: NotifyPrefs,
}

impl Default for GuiConfig {
    fn default() -> Self {
        GuiConfig {
            version: 1,
            machines: Vec::new(),
            notify: NotifyPrefs::default(),
        }
    }
}

// ---- 归一化（坏字段回退默认，不抛错）----

fn is_machine(v: &serde_json::Value) -> bool {
    v.get("id").and_then(|x| x.as_str()).is_some()
        && v.get("name").and_then(|x| x.as_str()).is_some()
        && v.get("url").and_then(|x| x.as_str()).is_some()
        && v.get("token").and_then(|x| x.as_str()).is_some()
}

/// 任意输入 → 合法配置。
pub fn normalize(raw: &serde_json::Value) -> GuiConfig {
    let mut cfg = GuiConfig::default();
    if let Some(machines) = raw.get("machines").and_then(|m| m.as_array()) {
        for m in machines {
            if !is_machine(m) {
                continue;
            }
            cfg.machines.push(MachineConfig {
                id: m["id"].as_str().unwrap_or("").to_string(),
                name: m["name"].as_str().unwrap_or("").to_string(),
                url: m["url"].as_str().unwrap_or("").to_string(),
                token: m["token"].as_str().unwrap_or("").to_string(),
            });
        }
    }
    if let Some(n) = raw.get("notify") {
        cfg.notify = NotifyPrefs {
            work_ended: n.get("workEnded").and_then(|v| v.as_bool()).unwrap_or(true),
            on_error: n.get("onError").and_then(|v| v.as_bool()).unwrap_or(true),
            long_idle_seconds: n
                .get("longIdleSeconds")
                .and_then(|v| v.as_u64())
                .unwrap_or(300),
        };
    }
    cfg
}

/// 机器连接 URL（ws://host:port?token=...）。
/// tungstenite 对无路径 URL（ws://host:port?token=...）会发出 `GET ?token=...`
/// （请求行缺 `/`），服务端拒握手；统一补 `/` 保证请求行合法。
pub fn machine_ws_url(m: &MachineConfig) -> String {
    let base = m.url.trim_end_matches('/');
    format!("{base}/?token={}", m.token)
}

// ---- 配置存储（后端可注入）----

pub trait ConfigBackend: Send + Sync {
    fn load(&self) -> Option<String>;
    fn save(&self, json: &str) -> std::io::Result<()>;
}

/// 文件后端：`~/.amux/gui/config.json`（路径可注入）。
pub struct FileBackend {
    path: PathBuf,
}

impl FileBackend {
    pub fn new(path: PathBuf) -> Self {
        FileBackend { path }
    }
}

impl ConfigBackend for FileBackend {
    fn load(&self) -> Option<String> {
        std::fs::read_to_string(&self.path).ok()
    }
    fn save(&self, json: &str) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.path, json)
    }
}

/// 配置仓库：读写 + 增删改查机器（纯逻辑）。
pub struct ConfigStore {
    backend: Box<dyn ConfigBackend>,
}

impl ConfigStore {
    pub fn new(backend: Box<dyn ConfigBackend>) -> Self {
        ConfigStore { backend }
    }

    pub fn load(&self) -> GuiConfig {
        match self.backend.load() {
            Some(json) => serde_json::from_str::<serde_json::Value>(&json)
                .map(|v| normalize(&v))
                .unwrap_or_default(),
            None => GuiConfig::default(),
        }
    }

    fn persist(&self, cfg: &GuiConfig) {
        if let Ok(json) = serde_json::to_string_pretty(cfg) {
            let _ = self.backend.save(&json);
        }
    }

    pub fn list_machines(&self) -> Vec<MachineConfig> {
        self.load().machines
    }

    pub fn add_machine(&self, name: &str, url: &str, token: &str) -> MachineConfig {
        let mut cfg = self.load();
        let machine = MachineConfig {
            id: format!("m_{:x}", cfg.machines.len() + 1),
            name: name.to_string(),
            url: url.to_string(),
            token: token.to_string(),
        };
        cfg.machines.push(machine.clone());
        self.persist(&cfg);
        machine
    }

    pub fn remove_machine(&self, id: &str) {
        let mut cfg = self.load();
        cfg.machines.retain(|m| m.id != id);
        self.persist(&cfg);
    }

    #[allow(dead_code)]
    pub fn update_machine(&self, id: &str, name: &str, url: &str, token: &str) {
        let mut cfg = self.load();
        if let Some(m) = cfg.machines.iter_mut().find(|m| m.id == id) {
            m.name = name.to_string();
            m.url = url.to_string();
            m.token = token.to_string();
        }
        self.persist(&cfg);
    }

    #[allow(dead_code)]
    pub fn save_notify(&self, prefs: &NotifyPrefs) {
        let mut cfg = self.load();
        cfg.notify = prefs.clone();
        self.persist(&cfg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MemBackend {
        data: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    }
    impl ConfigBackend for MemBackend {
        fn load(&self) -> Option<String> {
            self.data.lock().unwrap().clone()
        }
        fn save(&self, json: &str) -> std::io::Result<()> {
            *self.data.lock().unwrap() = Some(json.to_string());
            Ok(())
        }
    }

    fn store() -> (
        ConfigStore,
        std::sync::Arc<std::sync::Mutex<Option<String>>>,
    ) {
        let data = std::sync::Arc::new(std::sync::Mutex::new(None));
        let store = ConfigStore::new(Box::new(MemBackend { data: data.clone() }));
        (store, data)
    }

    #[test]
    fn add_list_remove_roundtrip() {
        let (store, _b) = store();
        assert!(store.list_machines().is_empty());
        let m = store.add_machine("本机", "ws://127.0.0.1:34567", "tok");
        assert_eq!(m.name, "本机");
        assert_eq!(store.list_machines().len(), 1);
        store.add_machine("远程", "ws://1.2.3.4:34567", "t2");
        assert_eq!(store.list_machines().len(), 2);
        store.remove_machine(&m.id);
        assert_eq!(store.list_machines().len(), 1);
        assert!(store.list_machines().iter().all(|x| x.id != m.id));
    }

    #[test]
    fn update_machine_persists() {
        let (store, _b) = store();
        let m = store.add_machine("a", "ws://x:1", "t");
        store.update_machine(&m.id, "a2", "ws://y:2", "t2");
        let m2 = store
            .list_machines()
            .into_iter()
            .find(|x| x.id == m.id)
            .unwrap();
        assert_eq!(m2.name, "a2");
        assert_eq!(m2.url, "ws://y:2");
    }

    #[test]
    fn persistence_roundtrip_via_file() {
        let dir = std::env::temp_dir().join(format!("amux-gui-cfg-{}", std::process::id()));
        let path = dir.join("config.json");
        let store = ConfigStore::new(Box::new(FileBackend::new(path.clone())));
        store.add_machine("m1", "ws://h:1", "t");
        store.save_notify(&NotifyPrefs {
            work_ended: false,
            on_error: true,
            long_idle_seconds: 60,
        });

        // 重新加载（同一文件）：机器与通知持久化往返
        let store2 = ConfigStore::new(Box::new(FileBackend::new(path)));
        let machines = store2.list_machines();
        assert_eq!(machines.len(), 1);
        assert_eq!(machines[0].name, "m1");
        assert_eq!(store2.load().notify.long_idle_seconds, 60);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn normalize_rejects_bad_fields() {
        let raw = serde_json::json!({
            "version": 1,
            "machines": [
                { "id": "a", "name": "ok", "url": "ws://h", "token": "t" },
                { "name": "缺 id", "url": "ws://h" }
            ],
            "notify": { "longIdleSeconds": "bad" }
        });
        let cfg = normalize(&raw);
        assert_eq!(cfg.machines.len(), 1);
        assert_eq!(cfg.machines[0].id, "a");
        assert_eq!(cfg.notify.long_idle_seconds, 300); // 坏值回退默认
    }

    #[test]
    fn machine_ws_url_always_has_path() {
        // 无路径 URL 必须补 `/`，否则 tungstenite 发出 `GET ?token=...`（请求行非法）
        let m = MachineConfig {
            id: "m".into(),
            name: "n".into(),
            url: "ws://127.0.0.1:34567".into(),
            token: "t".into(),
        };
        assert_eq!(machine_ws_url(&m), "ws://127.0.0.1:34567/?token=t");
        // 已带路径的 URL 不受影响（只去尾斜杠避免双斜杠）
        let m2 = MachineConfig {
            url: "ws://h:1/amux/".into(),
            ..m.clone()
        };
        assert_eq!(machine_ws_url(&m2), "ws://h:1/amux/?token=t");
    }
}
