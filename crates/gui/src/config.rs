//! GUI 本地配置（docs/DESIGN.md §6 / PRD §3.4/§3.6/§3.7/§4.3）：
//! 机器注册表、通知偏好、快捷指令、Skills 注册表、工作流模板、编排 agent API 配置。
//! 纯逻辑与 IO 分离：配置形状 + 校验/归一化 + 增删改查为可单测纯逻辑；
//! 持久化后端可注入（测试用临时目录）。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub use protocol::{OrchestratorConfig, QuickCommand, SkillEntry, WorkflowTemplate};

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
#[serde(rename_all = "camelCase")]
pub struct GuiConfig {
    pub version: u32,
    pub machines: Vec<MachineConfig>,
    pub notify: NotifyPrefs,
    /// 快捷指令（PRD §3.4）
    pub quick_commands: Vec<QuickCommand>,
    /// Skills 注册表（PRD §3.6）
    pub skills: Vec<SkillEntry>,
    /// 工作流模板（PRD §3.7）
    pub workflow_templates: Vec<WorkflowTemplate>,
    /// 内置编排 agent API 配置（PRD §4.3「编排 agent」）
    pub orchestrator: OrchestratorConfig,
}

impl Default for GuiConfig {
    fn default() -> Self {
        GuiConfig {
            version: 1,
            machines: Vec::new(),
            notify: NotifyPrefs::default(),
            // PRD §3.4：预设 Commit & Push、Submit PR
            quick_commands: vec![
                QuickCommand {
                    id: "qc_commit_push".into(),
                    name: "Commit & Push".into(),
                    prompt: "提交并推送当前工作区的更改：为改动写一条简洁的 commit message，commit 后 push。".into(),
                },
                QuickCommand {
                    id: "qc_submit_pr".into(),
                    name: "Submit PR".into(),
                    prompt: "提交一个 Pull Request：stage → commit → push → 创建 PR。".into(),
                },
            ],
            skills: Vec::new(),
            workflow_templates: Vec::new(),
            orchestrator: OrchestratorConfig::default(),
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

fn is_quick_command(v: &serde_json::Value) -> bool {
    v.get("id").and_then(|x| x.as_str()).is_some()
        && v.get("name").and_then(|x| x.as_str()).is_some()
        && v.get("prompt").and_then(|x| x.as_str()).is_some()
}

fn is_skill(v: &serde_json::Value) -> bool {
    v.get("id").and_then(|x| x.as_str()).is_some()
        && v.get("name").and_then(|x| x.as_str()).is_some()
        && v.get("description").and_then(|x| x.as_str()).is_some()
}

fn is_template(v: &serde_json::Value) -> bool {
    v.get("id").and_then(|x| x.as_str()).is_some()
        && v.get("name").and_then(|x| x.as_str()).is_some()
        && v.get("description").and_then(|x| x.as_str()).is_some()
}

fn is_orchestrator(v: &serde_json::Value) -> bool {
    v.get("apiBackend").and_then(|x| x.as_str()).is_some()
        && v.get("baseUrl").and_then(|x| x.as_str()).is_some()
        && v.get("model").and_then(|x| x.as_str()).is_some()
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
    if let Some(list) = raw.get("quickCommands") {
        // 配置自带该分类时整体替换（清除预设），否则保留默认预设
        cfg.quick_commands.clear();
        if let Some(arr) = list.as_array() {
            for v in arr {
                if !is_quick_command(v) {
                    continue;
                }
                cfg.quick_commands.push(QuickCommand {
                    id: v["id"].as_str().unwrap_or("").to_string(),
                    name: v["name"].as_str().unwrap_or("").to_string(),
                    prompt: v["prompt"].as_str().unwrap_or("").to_string(),
                });
            }
        }
    }
    if let Some(list) = raw.get("skills") {
        cfg.skills.clear();
        if let Some(arr) = list.as_array() {
            for v in arr {
                if !is_skill(v) {
                    continue;
                }
                cfg.skills.push(SkillEntry {
                    id: v["id"].as_str().unwrap_or("").to_string(),
                    name: v["name"].as_str().unwrap_or("").to_string(),
                    description: v["description"].as_str().unwrap_or("").to_string(),
                });
            }
        }
    }
    if let Some(list) = raw.get("workflowTemplates") {
        cfg.workflow_templates.clear();
        if let Some(arr) = list.as_array() {
            for v in arr {
                if !is_template(v) {
                    continue;
                }
                cfg.workflow_templates.push(WorkflowTemplate {
                    id: v["id"].as_str().unwrap_or("").to_string(),
                    name: v["name"].as_str().unwrap_or("").to_string(),
                    description: v["description"].as_str().unwrap_or("").to_string(),
                });
            }
        }
    }
    if let Some(o) = raw.get("orchestrator") {
        if is_orchestrator(o) {
            cfg.orchestrator = OrchestratorConfig {
                api_backend: o["apiBackend"].as_str().unwrap_or("").to_string(),
                base_url: o["baseUrl"].as_str().unwrap_or("").to_string(),
                api_key: o["apiKey"].as_str().unwrap_or("").to_string(),
                model: o["model"].as_str().unwrap_or("").to_string(),
            };
        }
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
    workflow_dir: PathBuf,
}

impl ConfigStore {
    #[allow(dead_code)]
    pub fn new(backend: Box<dyn ConfigBackend>) -> Self {
        ConfigStore {
            backend,
            workflow_dir: std::env::var("HOME")
                .map(|h| PathBuf::from(h).join(".amux").join("gui").join("workflows"))
                .unwrap_or_else(|_| PathBuf::from(".amux/gui/workflows")),
        }
    }

    /// 指定工作流持久化目录（GUI 数据目录，docs/DESIGN.md §5.5）。
    pub fn new_with_dir(backend: Box<dyn ConfigBackend>, workflow_dir: PathBuf) -> Self {
        ConfigStore {
            backend,
            workflow_dir,
        }
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

    // ---- 快捷指令（PRD §3.4）----

    pub fn list_quick_commands(&self) -> Vec<QuickCommand> {
        self.load().quick_commands
    }

    pub fn add_quick_command(&self, name: &str, prompt: &str) -> QuickCommand {
        let mut cfg = self.load();
        let cmd = QuickCommand {
            id: format!("qc_{:x}", cfg.quick_commands.len() + 1),
            name: name.to_string(),
            prompt: prompt.to_string(),
        };
        cfg.quick_commands.push(cmd.clone());
        self.persist(&cfg);
        cmd
    }

    pub fn update_quick_command(&self, id: &str, name: &str, prompt: &str) {
        let mut cfg = self.load();
        if let Some(c) = cfg.quick_commands.iter_mut().find(|c| c.id == id) {
            c.name = name.to_string();
            c.prompt = prompt.to_string();
        }
        self.persist(&cfg);
    }

    pub fn remove_quick_command(&self, id: &str) {
        let mut cfg = self.load();
        cfg.quick_commands.retain(|c| c.id != id);
        self.persist(&cfg);
    }

    // ---- Skills 注册表（PRD §3.6）----

    pub fn list_skills(&self) -> Vec<SkillEntry> {
        self.load().skills
    }

    pub fn add_skill(&self, name: &str, description: &str) -> SkillEntry {
        let mut cfg = self.load();
        let skill = SkillEntry {
            id: format!("sk_{:x}", cfg.skills.len() + 1),
            name: name.to_string(),
            description: description.to_string(),
        };
        cfg.skills.push(skill.clone());
        self.persist(&cfg);
        skill
    }

    pub fn update_skill(&self, id: &str, name: &str, description: &str) {
        let mut cfg = self.load();
        if let Some(s) = cfg.skills.iter_mut().find(|s| s.id == id) {
            s.name = name.to_string();
            s.description = description.to_string();
        }
        self.persist(&cfg);
    }

    pub fn remove_skill(&self, id: &str) {
        let mut cfg = self.load();
        cfg.skills.retain(|s| s.id != id);
        self.persist(&cfg);
    }

    // ---- 工作流模板（PRD §3.7）----

    pub fn list_templates(&self) -> Vec<WorkflowTemplate> {
        self.load().workflow_templates
    }

    pub fn add_template(&self, name: &str, description: &str) -> WorkflowTemplate {
        let mut cfg = self.load();
        let tpl = WorkflowTemplate {
            id: format!("tpl_{:x}", cfg.workflow_templates.len() + 1),
            name: name.to_string(),
            description: description.to_string(),
        };
        cfg.workflow_templates.push(tpl.clone());
        self.persist(&cfg);
        tpl
    }

    pub fn update_template(&self, id: &str, name: &str, description: &str) {
        let mut cfg = self.load();
        if let Some(t) = cfg.workflow_templates.iter_mut().find(|t| t.id == id) {
            t.name = name.to_string();
            t.description = description.to_string();
        }
        self.persist(&cfg);
    }

    pub fn remove_template(&self, id: &str) {
        let mut cfg = self.load();
        cfg.workflow_templates.retain(|t| t.id != id);
        self.persist(&cfg);
    }

    // ---- 编排 agent 配置（PRD §4.3）----

    pub fn orchestrator(&self) -> OrchestratorConfig {
        self.load().orchestrator
    }

    pub fn save_orchestrator(&self, cfg: &OrchestratorConfig) {
        let mut c = self.load();
        c.orchestrator = cfg.clone();
        self.persist(&c);
    }

    /// 工作流持久化目录（GUI 本地，docs/DESIGN.md §5.5/§10）。
    pub fn workflow_dir(&self) -> PathBuf {
        self.workflow_dir.clone()
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
            self.data
                .lock()
                .expect("Mutex 中毒（临界区内不应 panic）")
                .clone()
        }
        fn save(&self, json: &str) -> std::io::Result<()> {
            *self.data.lock().expect("Mutex 中毒（临界区内不应 panic）") = Some(json.to_string());
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

    #[test]
    fn defaults_include_preset_quick_commands() {
        let cfg = GuiConfig::default();
        assert_eq!(cfg.quick_commands.len(), 2);
        assert_eq!(cfg.quick_commands[0].name, "Commit & Push");
        assert_eq!(cfg.quick_commands[1].name, "Submit PR");
        assert_eq!(cfg.orchestrator.model, "gpt-4o-mini");
    }

    #[test]
    fn quick_command_crud_roundtrip() {
        let (store, _b) = store();
        assert_eq!(store.list_quick_commands().len(), 2); // 预设
        let c = store.add_quick_command("构建", "运行 cargo build");
        assert_eq!(store.list_quick_commands().len(), 3);
        store.update_quick_command(&c.id, "构建+测试", "cargo test");
        let got = store
            .list_quick_commands()
            .into_iter()
            .find(|x| x.id == c.id)
            .unwrap();
        assert_eq!(got.name, "构建+测试");
        assert_eq!(got.prompt, "cargo test");
        store.remove_quick_command(&c.id);
        assert_eq!(store.list_quick_commands().len(), 2);
    }

    #[test]
    fn skill_crud_roundtrip() {
        let (store, _b) = store();
        let s = store.add_skill("web", "https://github.com/x/web");
        assert_eq!(store.list_skills().len(), 1);
        store.update_skill(&s.id, "web2", "本地 ~/skills/web");
        let got = store.list_skills().into_iter().next().unwrap();
        assert_eq!(got.description, "本地 ~/skills/web");
        store.remove_skill(&s.id);
        assert!(store.list_skills().is_empty());
    }

    #[test]
    fn template_crud_and_file_roundtrip() {
        let dir = std::env::temp_dir().join(format!("amux-gui-tpl-{}", std::process::id()));
        let path = dir.join("config.json");
        let store = ConfigStore::new(Box::new(FileBackend::new(path.clone())));
        let t = store.add_template("实现并审查", "用 codex 实现，claude 审查");
        store.add_template("部署", "构建并部署到远程");

        // 重新加载（落盘往返）
        let store2 = ConfigStore::new(Box::new(FileBackend::new(path.clone())));
        let templates = store2.list_templates();
        assert_eq!(templates.len(), 2);
        assert_eq!(templates[0].name, "实现并审查");

        store2.remove_template(&t.id);
        let store3 = ConfigStore::new(Box::new(FileBackend::new(path)));
        assert_eq!(store3.list_templates().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn orchestrator_save_roundtrip() {
        let (store, _b) = store();
        let cfg = OrchestratorConfig {
            api_backend: "messages".into(),
            base_url: "http://localhost:8000/v1".into(),
            api_key: "sk-test".into(),
            model: "gpt-4.1".into(),
        };
        store.save_orchestrator(&cfg);
        assert_eq!(store.orchestrator().model, "gpt-4.1");
        assert_eq!(store.orchestrator().api_backend, "messages");
    }

    #[test]
    fn normalize_keeps_new_sections_and_rejects_bad() {
        let raw = serde_json::json!({
            "version": 1,
            "machines": [{ "id": "a", "name": "ok", "url": "ws://h", "token": "t" }],
            "quickCommands": [
                { "id": "q1", "name": "构建", "prompt": "cargo build" },
                { "name": "缺 prompt" }
            ],
            "skills": [{ "id": "s1", "name": "web", "description": "url" }],
            "workflowTemplates": [{ "id": "t1", "name": "审查", "description": "desc" }],
            "orchestrator": { "apiBackend": "chat_completions", "baseUrl": "http://x", "apiKey": "k", "model": "m" }
        });
        let cfg = normalize(&raw);
        assert_eq!(cfg.machines.len(), 1);
        assert_eq!(cfg.quick_commands.len(), 1, "缺 prompt 的条目应被丢弃");
        assert_eq!(cfg.skills.len(), 1);
        assert_eq!(cfg.workflow_templates.len(), 1);
        assert_eq!(cfg.orchestrator.model, "m");
        // 坏的 orchestrator 回退默认
        let raw2 = serde_json::json!({ "orchestrator": { "baseUrl": 1 } });
        assert_eq!(normalize(&raw2).orchestrator.model, "gpt-4o-mini");
    }
}
