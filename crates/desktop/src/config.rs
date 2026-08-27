//! GUI 本地配置：
//! 应用本地数据**拆到 `~/.amux/app/` 多个文件**：
//! machines.json、skills.json、workflows.json、recent_workspaces.json、quick_commands.json、
//! agent.json，以及工作流会话的 sessions/ 目录。
//!
//! 纯逻辑与 IO 分离：每种文件的校验/归一化（坏条目丢弃）为可单测纯函数；
//! `ConfigStore` 按文件读写（数据目录可注入，测试用临时目录）。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::logic::{merge_recent_workspace, recent_workspaces_for_machine};
/// 注册机器：name 唯一。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MachineConfig {
    pub name: String,
    pub url: String,
    pub token: String,
}

/// 技能条目：name 唯一，只存描述。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillEntry {
    pub name: String,
    pub description: String,
}

/// 工作流计划：name 唯一。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkflowTemplate {
    pub name: String,
    pub plan: String,
}

/// 常用工作目录条目：(machine, workspace) 唯一。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RecentWorkspace {
    pub machine: String,
    pub workspace: String,
    pub last_used: u64,
}

/// 快捷指令：name 唯一。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuickCommand {
    pub name: String,
    pub prompt: String,
}

/// 编排 agent 的 API 格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiFormat {
    ChatCompletions,
    Responses,
    Messages,
}

/// 内置编排 agent 的 API 配置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrchestratorConfig {
    pub api_format: ApiFormat,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}

impl Default for OrchestratorConfig {
    fn default() -> Self {
        OrchestratorConfig {
            api_format: ApiFormat::ChatCompletions,
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
        }
    }
}

impl OrchestratorConfig {
    /// 编排 agent 是否已配置可用：Base URL、API key、模型均非空。
    pub fn is_configured(&self) -> bool {
        !self.base_url.trim().is_empty()
            && !self.api_key.trim().is_empty()
            && !self.model.trim().is_empty()
    }
}

/// 每设备常用工作目录数量上限。
pub const MAX_RECENT_WORKSPACES: usize = 20;

fn is_string_field(v: &serde_json::Value, key: &str) -> bool {
    v.get(key).and_then(|x| x.as_str()).is_some()
}

/// 机器目录归一化：缺 name/url/token 的条目丢弃。
pub fn normalize_machines(raw: &serde_json::Value) -> Vec<MachineConfig> {
    raw.as_array()
        .map(|arr| {
            arr.iter()
                .filter(|v| {
                    is_string_field(v, "name")
                        && is_string_field(v, "url")
                        && is_string_field(v, "token")
                })
                .map(|v| MachineConfig {
                    name: v["name"].as_str().unwrap_or("").to_string(),
                    url: v["url"].as_str().unwrap_or("").to_string(),
                    token: v["token"].as_str().unwrap_or("").to_string(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 常用工作目录归一化：缺 machine/workspace 的条目丢弃。
pub fn normalize_recent_workspaces(raw: &serde_json::Value) -> Vec<RecentWorkspace> {
    raw.as_array()
        .map(|arr| {
            arr.iter()
                .filter(|v| is_string_field(v, "machine") && is_string_field(v, "workspace"))
                .map(|v| RecentWorkspace {
                    machine: v["machine"].as_str().unwrap_or("").to_string(),
                    workspace: v["workspace"].as_str().unwrap_or("").to_string(),
                    last_used: v.get("lastUsed").and_then(|x| x.as_u64()).unwrap_or(0),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 编排配置归一化：字段缺失或 api_format 非法 → 整体回退默认。
pub fn normalize_orchestrator(raw: &serde_json::Value) -> OrchestratorConfig {
    let has_req = ["apiFormat", "baseUrl", "apiKey", "model"]
        .iter()
        .all(|k| is_string_field(raw, k));
    if !has_req {
        return OrchestratorConfig::default();
    }
    let Ok(api_format) = serde_json::from_value::<ApiFormat>(raw["apiFormat"].clone()) else {
        return OrchestratorConfig::default();
    };
    OrchestratorConfig {
        api_format,
        base_url: raw["baseUrl"].as_str().unwrap_or("").to_string(),
        api_key: raw["apiKey"].as_str().unwrap_or("").to_string(),
        model: raw["model"].as_str().unwrap_or("").to_string(),
    }
}

fn read_file_typed<T: serde::de::DeserializeOwned>(path: &Path) -> Vec<T> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            log::warn!("读取配置失败 {}: {e}", path.display());
            return Vec::new();
        }
    };
    match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            log::warn!("配置解析失败（按空处理）{}: {e}", path.display());
            Vec::new()
        }
    }
}

fn write_typed<T: serde::Serialize>(path: &Path, value: &T) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("json.tmp");
    match serde_json::to_string_pretty(value) {
        Ok(body) => {
            if let Err(e) = std::fs::write(&tmp, body).and_then(|_| std::fs::rename(&tmp, path)) {
                log::warn!("写入配置失败 {}: {e}", path.display());
            }
        }
        Err(e) => {
            log::warn!("序列化失败 {}: {e}", path.display())
        }
    }
}

/// name 键控的 JSON 集合文件（quick_commands/skills/workflows 共用）：
/// list/add(upsert)/update/remove 四组 CRUD 曾逐字重复三份，仅文件名与类型不同。
struct JsonCollection<'a, T> {
    path: std::borrow::Cow<'a, Path>,
    /// 从存储值提取唯一键（name）
    key: for<'x> fn(&'x T) -> &'x str,
}

impl<'a, T: Clone + serde::de::DeserializeOwned + serde::Serialize> JsonCollection<'a, T> {
    fn list(&self) -> Vec<T> {
        read_file_typed(self.path.as_ref())
    }

    fn upsert(&self, item: T) {
        let key = (self.key)(&item);
        let mut items = self.list();
        items.retain(|x| (self.key)(x) != key);
        items.push(item);
        write_typed(self.path.as_ref(), &items);
    }

    fn update(&self, key: &str, mutate: impl FnOnce(&mut T)) {
        let mut items = self.list();
        if let Some(x) = items.iter_mut().find(|x| (self.key)(x) == key) {
            mutate(x);
        }
        write_typed(self.path.as_ref(), &items);
    }

    fn remove(&self, key: &str) {
        let items = self
            .list()
            .into_iter()
            .filter(|x| (self.key)(x) != key)
            .collect::<Vec<_>>();
        write_typed(self.path.as_ref(), &items);
    }
}

/// 从文件读取并归一化（不存在/损坏 → 空）。`normalize` 负责坏条目丢弃。
fn read_file_normalized<T: Clone>(
    path: &Path,
    parse: impl Fn(&serde_json::Value) -> Vec<T>,
) -> Vec<T> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            log::error!("读取配置失败 {}: {e}", path.display());
            return Vec::new();
        }
    };
    let value = match serde_json::from_str::<serde_json::Value>(&raw) {
        Ok(value) => value,
        Err(e) => {
            log::error!("解析配置失败 {}: {e}", path.display());
            return Vec::new();
        }
    };
    parse(&value)
}

fn write_file(path: &Path, json: &serde_json::Value) {
    let result = (|| -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(json).map_err(std::io::Error::other)?;
        std::fs::write(path, content)
    })();
    if let Err(error) = result {
        log::error!("写入配置文件失败 {}: {error}", path.display());
    }
}

/// 配置仓库：按文件读写拆分的本地数据（数据目录可注入）。
pub struct ConfigStore {
    data_dir: PathBuf,
}

impl ConfigStore {
    /// 指定数据目录（默认可由调用方传入 `~/.amux/app`）。
    pub fn new(data_dir: PathBuf) -> Self {
        ConfigStore { data_dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.data_dir.join(name)
    }

    pub fn list_machines(&self) -> Vec<MachineConfig> {
        read_file_normalized(&self.path("machines.json"), normalize_machines)
    }

    pub fn add_machine(&self, name: &str, url: &str, token: &str) -> MachineConfig {
        let mut machines = self.list_machines();
        machines.retain(|m| m.name != name);
        let m = MachineConfig {
            name: name.to_string(),
            url: url.to_string(),
            token: token.to_string(),
        };
        machines.push(m.clone());
        write_file(
            &self.path("machines.json"),
            &serde_json::to_value(machines).unwrap(),
        );
        m
    }

    pub fn remove_machine(&self, name: &str) {
        let machines: Vec<MachineConfig> = self
            .list_machines()
            .into_iter()
            .filter(|m| m.name != name)
            .collect();
        write_file(
            &self.path("machines.json"),
            &serde_json::to_value(machines).unwrap(),
        );
    }

    fn quick_commands(&self) -> JsonCollection<'_, QuickCommand> {
        JsonCollection {
            path: std::borrow::Cow::Owned(self.path("quick_commands.json")),
            key: |c| &c.name,
        }
    }

    pub fn list_quick_commands(&self) -> Vec<QuickCommand> {
        self.quick_commands().list()
    }

    pub fn add_quick_command(&self, name: &str, prompt: &str) -> QuickCommand {
        let c = QuickCommand {
            name: name.to_string(),
            prompt: prompt.to_string(),
        };
        self.quick_commands().upsert(c.clone());
        c
    }

    pub fn update_quick_command(&self, name: &str, prompt: &str) {
        self.quick_commands()
            .update(name, |c| c.prompt = prompt.to_string());
    }

    pub fn remove_quick_command(&self, name: &str) {
        self.quick_commands().remove(name);
    }

    fn skills(&self) -> JsonCollection<'_, SkillEntry> {
        JsonCollection {
            path: std::borrow::Cow::Owned(self.path("skills.json")),
            key: |s| &s.name,
        }
    }

    pub fn list_skills(&self) -> Vec<SkillEntry> {
        self.skills().list()
    }

    pub fn add_skill(&self, name: &str, description: &str) -> SkillEntry {
        let entry = SkillEntry {
            name: name.to_string(),
            description: description.to_string(),
        };
        self.skills().upsert(entry.clone());
        entry
    }

    pub fn update_skill(&self, name: &str, description: &str) {
        self.skills()
            .update(name, |s| s.description = description.to_string());
    }

    pub fn remove_skill(&self, name: &str) {
        self.skills().remove(name);
    }

    fn templates(&self) -> JsonCollection<'_, WorkflowTemplate> {
        JsonCollection {
            path: std::borrow::Cow::Owned(self.path("workflows.json")),
            key: |t| &t.name,
        }
    }

    pub fn list_templates(&self) -> Vec<WorkflowTemplate> {
        self.templates().list()
    }

    pub fn add_template(&self, name: &str, plan: &str) -> WorkflowTemplate {
        let tpl = WorkflowTemplate {
            name: name.to_string(),
            plan: plan.to_string(),
        };
        self.templates().upsert(tpl.clone());
        tpl
    }

    pub fn update_template(&self, name: &str, plan: &str) {
        self.templates().update(name, |t| t.plan = plan.to_string());
    }

    pub fn remove_template(&self, name: &str) {
        self.templates().remove(name);
    }

    pub fn recent_workspaces(&self) -> Vec<RecentWorkspace> {
        read_file_normalized(
            &self.path("recent_workspaces.json"),
            normalize_recent_workspaces,
        )
    }

    /// 某设备的常用工作目录，最近使用优先。
    pub fn recent_workspaces_for_machine(&self, machine: &str) -> Vec<String> {
        recent_workspaces_for_machine(&self.recent_workspaces(), machine)
    }

    /// 记录一次常用工作目录使用（(machine, workspace) 唯一、最近优先、上限）。
    pub fn record_recent_workspace(&self, machine: &str, workspace: &str, now: u64) {
        let ws = self.recent_workspaces();
        // 直接写合并结果（不再读回，防止并发覆盖）
        let merged = merge_recent_workspace(&ws, machine, workspace, now, MAX_RECENT_WORKSPACES);
        write_file(
            &self.path("recent_workspaces.json"),
            &serde_json::to_value(&merged).unwrap(),
        );
    }

    pub fn orchestrator(&self) -> OrchestratorConfig {
        let path = self.path("agent.json");
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return OrchestratorConfig::default();
            }
            Err(e) => {
                log::error!("读取编排配置失败: {e}");
                return OrchestratorConfig::default();
            }
        };
        match serde_json::from_str::<serde_json::Value>(&raw) {
            Ok(value) => normalize_orchestrator(&value),
            Err(e) => {
                log::error!("解析编排配置失败 {}: {e}", path.display());
                OrchestratorConfig::default()
            }
        }
    }

    pub fn save_orchestrator(&self, cfg: &OrchestratorConfig) -> std::io::Result<()> {
        let path = self.path("agent.json");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(cfg).map_err(std::io::Error::other)?;
        std::fs::write(path, json)
    }

    /// 工作流会话持久化目录（~/.amux/app/sessions/）。
    pub fn session_dir(&self) -> PathBuf {
        self.data_dir.join("sessions")
    }
}

/// 机器 WS 连接 URL；token 在建连后经 `auth` 发送，不放入 URL。
/// 统一补 `/` 保证 tungstenite 请求行合法（`GET /` 而非非法空路径）。
pub fn machine_ws_url(m: &MachineConfig) -> String {
    let base = m.url.trim_end_matches('/');
    format!("{base}/")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个测试独一无二的临时目录（避免并行测试共享同一目录互相清空）。
    fn temp_dir() -> PathBuf {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::env::temp_dir().join(format!("amux-app-{}-{n}", std::process::id()))
    }

    fn store() -> ConfigStore {
        let dir = temp_dir();
        let _ = std::fs::remove_dir_all(&dir);
        ConfigStore::new(dir)
    }

    fn store_with(dir: PathBuf) -> ConfigStore {
        let _ = std::fs::remove_dir_all(&dir);
        ConfigStore::new(dir)
    }

    #[test]
    fn normalize_machines_drops_bad_entries() {
        let raw = serde_json::json!([
            { "name": "a", "url": "ws://h:1", "token": "t" },
            { "name": "缺token", "url": "ws://h:2" },
            { "url": "ws://h:3", "token": "t" }
        ]);
        let machines = normalize_machines(&raw);
        assert_eq!(machines.len(), 1);
        assert_eq!(machines[0].name, "a");
    }

    #[test]
    fn normalize_recent_workspaces_drops_bad_entries() {
        let raw = serde_json::json!([
            { "machine": "m1", "workspace": "/a", "lastUsed": 10 },
            { "machine": "缺 workspace" }
        ]);
        let ws = normalize_recent_workspaces(&raw);
        assert_eq!(ws.len(), 1);
        assert_eq!(ws[0].machine, "m1");
        assert_eq!(ws[0].workspace, "/a");
        assert_eq!(ws[0].last_used, 10);
    }

    #[test]
    fn normalize_orchestrator_falls_back_to_default_on_missing() {
        let raw = serde_json::json!({ "baseUrl": 1 });
        assert_eq!(normalize_orchestrator(&raw).model, "");
        let full = serde_json::json!({
            "apiFormat": "responses", "baseUrl": "http://x", "apiKey": "k", "model": "m"
        });
        let cfg = normalize_orchestrator(&full);
        assert_eq!(cfg.model, "m");
        assert_eq!(cfg.api_format, ApiFormat::Responses);
        let bad = serde_json::json!({
            "apiFormat": "graphql", "baseUrl": "http://x", "apiKey": "k", "model": "m"
        });
        assert_eq!(normalize_orchestrator(&bad), OrchestratorConfig::default());
    }

    #[test]
    fn machines_split_file_roundtrip_and_dedup() {
        let s = store();
        assert!(s.list_machines().is_empty());
        s.add_machine("本机", "ws://127.0.0.1:34567", "t");
        s.add_machine("本机", "ws://127.0.0.1:34567", "t2");
        s.add_machine("远程", "ws://1.2.3.4:34567", "t3");
        let machines = s.list_machines();
        assert_eq!(machines.len(), 2, "name 唯一");
        assert!(machines.iter().any(|m| m.name == "远程"));
        s.remove_machine("本机");
        assert_eq!(s.list_machines().len(), 1);
        assert!(s.list_machines().iter().all(|m| m.name != "本机"));
    }

    #[test]
    fn split_files_persist_roundtrip() {
        let dir = temp_dir();
        let s = store_with(dir.clone());
        s.add_machine("m1", "ws://h:1", "t");
        s.add_skill("web", "https://github.com/x/web");
        s.add_template("审查", "用 codex 实现，claude 审查");
        s.add_quick_command("构建", "cargo build");
        s.save_orchestrator(&OrchestratorConfig {
            api_format: ApiFormat::Responses,
            base_url: "http://localhost:8000/v1".into(),
            api_key: "sk".into(),
            model: "gpt-4.1".into(),
        })
        .unwrap();

        let s2 = ConfigStore::new(dir.clone());
        assert_eq!(s2.list_machines().len(), 1);
        assert_eq!(s2.list_skills().len(), 1);
        assert_eq!(s2.list_templates().len(), 1);
        assert!(s2.list_quick_commands().iter().any(|c| c.name == "构建"));
        assert_eq!(s2.orchestrator().model, "gpt-4.1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recent_workspaces_record_and_persist_via_file() {
        let dir = temp_dir();
        let s = store_with(dir.clone());
        assert!(s.recent_workspaces().is_empty());
        s.record_recent_workspace("m1", "/a", 1);
        s.record_recent_workspace("m1", "/b", 2);
        s.record_recent_workspace("m1", "/a", 3);
        s.record_recent_workspace("m2", "/x", 4);
        let s2 = ConfigStore::new(dir.clone());
        assert_eq!(s2.recent_workspaces_for_machine("m1"), vec!["/a", "/b"]);
        assert_eq!(s2.recent_workspaces_for_machine("m2"), vec!["/x"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn skills_and_templates_crud() {
        let s = store();
        assert!(s.list_skills().is_empty());
        s.add_skill("web", "url1");
        s.update_skill("web", "url2");
        assert_eq!(s.list_skills()[0].description, "url2");
        s.remove_skill("web");
        assert!(s.list_skills().is_empty());

        s.add_template("t", "plan");
        assert_eq!(s.list_templates()[0].plan, "plan");
    }
}
