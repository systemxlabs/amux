//! GUI 本地配置（docs/DESIGN.md「应用」/ PRD §3.4/§3.6/§3.7/§4.3）：
//! 应用本地数据**拆到 `~/.amux/app/` 多个文件**：
//! machines.json、skills.json、workflows.json、recent_workspaces.json、quick_commands.json、
//! agent.json，以及工作流会话的 sessions/ 目录。
//!
//! 纯逻辑与 IO 分离：每种文件的校验/归一化（坏条目丢弃）为可单测纯函数；
//! `ConfigStore` 按文件读写（数据目录可注入，测试用临时目录）。

use std::path::{Path, PathBuf};

pub use protocol::{
    MachineConfig, OrchestratorConfig, QuickCommand, RecentWorkspace, SkillEntry, WorkflowTemplate,
};

use crate::logic::{merge_recent_workspace, recent_workspaces_for_machine};

/// 每设备常用工作目录数量上限（实现决策；PRD 未明确，取一个合理值）。
pub const MAX_RECENT_WORKSPACES: usize = 20;

// ---- 归一化（坏字段回退/丢弃，不抛错）----

fn is_string_field(v: &serde_json::Value, key: &str) -> bool {
    v.get(key).and_then(|x| x.as_str()).is_some()
}

/// 机器目录归一化：缺 name/url/token 的条目丢弃（docs/DESIGN.md「注册机器存储」）。
pub fn normalize_machines(raw: &serde_json::Value) -> Vec<MachineConfig> {
    raw.as_array()
        .map(|arr| {
            arr.iter()
                .filter(|v| {
                    is_string_field(v, "name") && is_string_field(v, "url") && is_string_field(v, "token")
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

/// 技能目录归一化：缺 name/description 的条目丢弃（docs/DESIGN.md「技能存储」）。
pub fn normalize_skills(raw: &serde_json::Value) -> Vec<SkillEntry> {
    raw.as_array()
        .map(|arr| {
            arr.iter()
                .filter(|v| {
                    is_string_field(v, "name") && is_string_field(v, "description")
                })
                .map(|v| SkillEntry {
                    name: v["name"].as_str().unwrap_or("").to_string(),
                    description: v["description"].as_str().unwrap_or("").to_string(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 工作流模板归一化：缺 name/plan 的条目丢弃（docs/DESIGN.md「工作流模板存储」）。
pub fn normalize_workflows(raw: &serde_json::Value) -> Vec<WorkflowTemplate> {
    raw.as_array()
        .map(|arr| {
            arr.iter()
                .filter(|v| is_string_field(v, "name") && is_string_field(v, "plan"))
                .map(|v| WorkflowTemplate {
                    name: v["name"].as_str().unwrap_or("").to_string(),
                    plan: v["plan"].as_str().unwrap_or("").to_string(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 常用工作目录归一化：缺 machine/workspace 的条目丢弃（docs/DESIGN.md「常用工作目录存储」）。
pub fn normalize_recent_workspaces(raw: &serde_json::Value) -> Vec<RecentWorkspace> {
    raw.as_array()
        .map(|arr| {
            arr.iter()
                .filter(|v| {
                    is_string_field(v, "machine") && is_string_field(v, "workspace")
                })
                .map(|v| RecentWorkspace {
                    machine: v["machine"].as_str().unwrap_or("").to_string(),
                    workspace: v["workspace"].as_str().unwrap_or("").to_string(),
                    last_used: v.get("lastUsed").and_then(|x| x.as_u64()).unwrap_or(0),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 快捷指令归一化：缺 name/prompt 的条目丢弃（docs/DESIGN.md「快捷指令存储」）。
pub fn normalize_quick_commands(raw: &serde_json::Value) -> Vec<QuickCommand> {
    raw.as_array()
        .map(|arr| {
            arr.iter()
                .filter(|v| is_string_field(v, "name") && is_string_field(v, "prompt"))
                .map(|v| QuickCommand {
                    name: v["name"].as_str().unwrap_or("").to_string(),
                    prompt: v["prompt"].as_str().unwrap_or("").to_string(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 编排配置归一化：坏配置回退默认（docs/DESIGN.md「编排智能体配置存储」）。
pub fn normalize_orchestrator(raw: &serde_json::Value) -> OrchestratorConfig {
    let has_req = ["apiFormat", "baseUrl", "apiKey", "model"]
        .iter()
        .all(|k| is_string_field(raw, k));
    if !has_req {
        return OrchestratorConfig::default();
    }
    OrchestratorConfig {
        api_format: raw["apiFormat"].as_str().unwrap_or("").to_string(),
        base_url: raw["baseUrl"].as_str().unwrap_or("").to_string(),
        api_key: raw["apiKey"].as_str().unwrap_or("").to_string(),
        model: raw["model"].as_str().unwrap_or("").to_string(),
    }
}

// ---- 存储工具 ----

/// 从文件读取并归一化（不存在/损坏 → 空）。`normalize` 负责坏条目丢弃。
fn read_file_normalized<T: Clone>(
    path: &Path,
    parse: impl Fn(&serde_json::Value) -> Vec<T>,
) -> Vec<T> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return Vec::new();
    };
    parse(&value)
}

fn write_file(path: &Path, json: &serde_json::Value) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, serde_json::to_string_pretty(json).unwrap_or_default());
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

    // ---- 机器（machines.json）----

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
        write_file(&self.path("machines.json"), &serde_json::to_value(machines).unwrap());
        m
    }

    pub fn remove_machine(&self, name: &str) {
        let machines: Vec<MachineConfig> = self
            .list_machines()
            .into_iter()
            .filter(|m| m.name != name)
            .collect();
        write_file(&self.path("machines.json"), &serde_json::to_value(machines).unwrap());
    }

    pub fn update_machine(&self, name: &str, url: &str, token: &str) {
        let mut machines = self.list_machines();
        if let Some(m) = machines.iter_mut().find(|m| m.name == name) {
            m.url = url.to_string();
            m.token = token.to_string();
        }
        write_file(&self.path("machines.json"), &serde_json::to_value(machines).unwrap());
    }

    // ---- 快捷指令（quick_commands.json）----

    pub fn list_quick_commands(&self) -> Vec<QuickCommand> {
        let mut cmds = read_file_normalized(&self.path("quick_commands.json"), normalize_quick_commands);
        // PRD §3.4：预设两条快捷指令
        if cmds.is_empty() {
            cmds = vec![
                QuickCommand {
                    name: "Commit & Push".into(),
                    prompt: "提交并推送当前工作区的更改：为改动写一条简洁的 commit message，commit 后 push。".into(),
                },
                QuickCommand {
                    name: "Submit PR".into(),
                    prompt: "提交一个 Pull Request：stage → commit → push → 创建 PR。".into(),
                },
            ];
        }
        cmds
    }

    pub fn add_quick_command(&self, name: &str, prompt: &str) -> QuickCommand {
        let mut cmds = self.list_quick_commands();
        cmds.retain(|c| c.name != name);
        let c = QuickCommand {
            name: name.to_string(),
            prompt: prompt.to_string(),
        };
        cmds.push(c.clone());
        write_file(&self.path("quick_commands.json"), &serde_json::to_value(cmds).unwrap());
        c
    }

    pub fn update_quick_command(&self, name: &str, prompt: &str) {
        let mut cmds = self.list_quick_commands();
        if let Some(c) = cmds.iter_mut().find(|c| c.name == name) {
            c.prompt = prompt.to_string();
        }
        write_file(&self.path("quick_commands.json"), &serde_json::to_value(cmds).unwrap());
    }

    pub fn remove_quick_command(&self, name: &str) {
        let cmds: Vec<QuickCommand> = self
            .list_quick_commands()
            .into_iter()
            .filter(|c| c.name != name)
            .collect();
        write_file(&self.path("quick_commands.json"), &serde_json::to_value(cmds).unwrap());
    }

    // ---- Skills（skills.json）----

    pub fn list_skills(&self) -> Vec<SkillEntry> {
        read_file_normalized(&self.path("skills.json"), normalize_skills)
    }

    pub fn add_skill(&self, name: &str, description: &str) -> SkillEntry {
        let mut skills = self.list_skills();
        skills.retain(|s| s.name != name);
        let s = SkillEntry {
            name: name.to_string(),
            description: description.to_string(),
        };
        skills.push(s.clone());
        write_file(&self.path("skills.json"), &serde_json::to_value(skills).unwrap());
        s
    }

    pub fn update_skill(&self, name: &str, description: &str) {
        let mut skills = self.list_skills();
        if let Some(s) = skills.iter_mut().find(|s| s.name == name) {
            s.description = description.to_string();
        }
        write_file(&self.path("skills.json"), &serde_json::to_value(skills).unwrap());
    }

    pub fn remove_skill(&self, name: &str) {
        let skills: Vec<SkillEntry> = self
            .list_skills()
            .into_iter()
            .filter(|s| s.name != name)
            .collect();
        write_file(&self.path("skills.json"), &serde_json::to_value(skills).unwrap());
    }

    // ---- 工作流模板（workflows.json）----

    pub fn list_templates(&self) -> Vec<WorkflowTemplate> {
        read_file_normalized(&self.path("workflows.json"), normalize_workflows)
    }

    pub fn add_template(&self, name: &str, plan: &str) -> WorkflowTemplate {
        let mut tpls = self.list_templates();
        tpls.retain(|t| t.name != name);
        let t = WorkflowTemplate {
            name: name.to_string(),
            plan: plan.to_string(),
        };
        tpls.push(t.clone());
        write_file(&self.path("workflows.json"), &serde_json::to_value(tpls).unwrap());
        t
    }

    pub fn update_template(&self, name: &str, plan: &str) {
        let mut tpls = self.list_templates();
        if let Some(t) = tpls.iter_mut().find(|t| t.name == name) {
            t.plan = plan.to_string();
        }
        write_file(&self.path("workflows.json"), &serde_json::to_value(tpls).unwrap());
    }

    pub fn remove_template(&self, name: &str) {
        let tpls: Vec<WorkflowTemplate> = self
            .list_templates()
            .into_iter()
            .filter(|t| t.name != name)
            .collect();
        write_file(&self.path("workflows.json"), &serde_json::to_value(tpls).unwrap());
    }

    // ---- 常用工作目录（recent_workspaces.json）----

    pub fn recent_workspaces(&self) -> Vec<RecentWorkspace> {
        read_file_normalized(&self.path("recent_workspaces.json"), normalize_recent_workspaces)
    }

    /// 某设备的常用工作目录（最近使用优先）。
    pub fn recent_workspaces_for_machine(&self, machine: &str) -> Vec<String> {
        let ws = self.recent_workspaces();
        recent_workspaces_for_machine(&ws, machine)
    }

    /// 记录一次常用工作目录使用（(machine, workspace) 唯一、最近优先、上限）。
    pub fn record_recent_workspace(&self, machine: &str, workspace: &str, now: u64) {
        let ws = self.recent_workspaces();
        let merged = merge_recent_workspace(&ws, machine, workspace, now, MAX_RECENT_WORKSPACES);
        let _ = merged;
        // 直接写合并结果（不再读回，防止并发覆盖）
        write_file(
            &self.path("recent_workspaces.json"),
            &serde_json::to_value(&merged).unwrap(),
        );
    }

    // ---- 编排 agent（agent.json）----

    pub fn orchestrator(&self) -> OrchestratorConfig {
        let Ok(raw) = std::fs::read_to_string(self.path("agent.json")) else {
            return OrchestratorConfig::default();
        };
        serde_json::from_str::<serde_json::Value>(&raw)
            .map(|v| normalize_orchestrator(&v))
            .unwrap_or_default()
    }

    pub fn save_orchestrator(&self, cfg: &OrchestratorConfig) {
        write_file(
            &self.path("agent.json"),
            &serde_json::to_value(cfg).unwrap(),
        );
    }

    /// 数据目录。
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// 工作流会话持久化目录（~/.amux/app/sessions/）。
    pub fn session_dir(&self) -> PathBuf {
        self.data_dir.join("sessions")
    }
}

/// 机器连接 URL：`ws://host:port?token=...`（ws 后台从查询串解析 token 用于 auth）。
/// 统一补 `/` 保证 tungstenite 请求行合法（`GET /?...` 而非 `GET ?...`）。
/// 机器 WS 连接 URL（不含 token；token 在建连后经 `auth` 发送，docs/DESIGN.md「认证」）。
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
    fn normalize_skills_drops_bad_entries() {
        let raw = serde_json::json!([
            { "name": "opencli", "description": "url" },
            { "name": "缺 desc" }
        ]);
        let skills = normalize_skills(&raw);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "opencli");
    }

    #[test]
    fn normalize_workflows_drops_bad_entries() {
        let raw = serde_json::json!([
            { "name": "开发", "plan": "xxx" },
            { "name": "缺 plan" }
        ]);
        let tpls = normalize_workflows(&raw);
        assert_eq!(tpls.len(), 1);
        assert_eq!(tpls[0].plan, "xxx");
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
    fn normalize_quick_commands_drops_bad_entries() {
        let raw = serde_json::json!([
            { "name": "Commit", "prompt": "提交" },
            { "name": "缺 prompt" }
        ]);
        let cmds = normalize_quick_commands(&raw);
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].name, "Commit");
    }

    #[test]
    fn normalize_orchestrator_falls_back_to_default_on_missing() {
        let raw = serde_json::json!({ "baseUrl": 1 });
        assert_eq!(normalize_orchestrator(&raw).model, "");
        let full = serde_json::json!({
            "apiFormat": "responses", "baseUrl": "http://x", "apiKey": "k", "model": "m"
        });
        assert_eq!(normalize_orchestrator(&full).model, "m");
    }

    #[test]
    fn machines_split_file_roundtrip_and_dedup() {
        let s = store();
        assert!(s.list_machines().is_empty());
        s.add_machine("本机", "ws://127.0.0.1:34567", "t");
        s.add_machine("本机", "ws://127.0.0.1:34567", "t2"); // name 唯一去重
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
            api_format: "responses".into(),
            base_url: "http://localhost:8000/v1".into(),
            api_key: "sk".into(),
            model: "gpt-4.1".into(),
        });

        // 重新加载（同一目录）：各分类独立往返
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
        s.record_recent_workspace("m1", "/a", 3); // (machine, workspace) 唯一，移到最前
        s.record_recent_workspace("m2", "/x", 4);
        let s2 = ConfigStore::new(dir.clone());
        assert_eq!(s2.recent_workspaces_for_machine("m1"), vec!["/a", "/b"]);
        assert_eq!(s2.recent_workspaces_for_machine("m2"), vec!["/x"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn quick_commands_defaults_when_empty() {
        let s = store();
        let cmds = s.list_quick_commands();
        assert_eq!(cmds.len(), 2);
        assert_eq!(cmds[0].name, "Commit & Push");
        assert_eq!(cmds[1].name, "Submit PR");
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
