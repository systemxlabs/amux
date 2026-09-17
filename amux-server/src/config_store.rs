//! 配置存储：`<amux_home>/config/*.json`（技能、工作流计划、常用工作目录、快捷指令、内置智能体）。
//!
//! 读写为低频操作，不做并发与原子写入处理（docs/DESIGN.md 各「存储」一节）。

use std::path::PathBuf;

use amux_common::api::{
    OrchestratorConfig, QuickCommand, RecentWorkspace, Skill, WorkflowPlanItem,
};
use parking_lot::Mutex;

use crate::timestamps::now_ms;

/// 常用工作目录保留条数。
const RECENT_WORKSPACE_LIMIT: usize = 30;

pub struct ConfigStore {
    home: PathBuf,
    /// 常用工作目录在内存中维护（读改写频繁于其他配置）
    recent: Mutex<Vec<RecentWorkspace>>,
}

impl ConfigStore {
    pub fn new(home: PathBuf) -> Self {
        let recent = read_json::<Vec<RecentWorkspace>>(&recent_path(&home)).unwrap_or_default();
        Self {
            home,
            recent: Mutex::new(recent),
        }
    }

    pub fn skills(&self) -> Vec<Skill> {
        read_json::<Vec<Skill>>(&self.path("skills")).unwrap_or_default()
    }

    pub fn set_skills(&self, skills: &[Skill]) -> Result<(), String> {
        reject_duplicate_names(skills, |item| &item.name, "技能")?;
        write_json(&self.path("skills"), skills)
    }

    pub fn workflow_plans(&self) -> Vec<WorkflowPlanItem> {
        read_json::<Vec<WorkflowPlanItem>>(&self.path("workflows")).unwrap_or_default()
    }

    pub fn set_workflow_plans(&self, plans: &[WorkflowPlanItem]) -> Result<(), String> {
        reject_duplicate_names(plans, |item| &item.name, "工作流计划")?;
        write_json(&self.path("workflows"), plans)
    }

    pub fn quick_commands(&self) -> Vec<QuickCommand> {
        read_json::<Vec<QuickCommand>>(&self.path("quick_commands")).unwrap_or_default()
    }

    pub fn set_quick_commands(&self, commands: &[QuickCommand]) -> Result<(), String> {
        reject_duplicate_names(commands, |item| &item.name, "快捷指令")?;
        write_json(&self.path("quick_commands"), commands)
    }

    pub fn orchestrator(&self) -> Option<OrchestratorConfig> {
        read_json::<OrchestratorConfig>(&self.path("agent"))
    }

    pub fn set_orchestrator(&self, config: &OrchestratorConfig) -> Result<(), String> {
        write_json(&self.path("agent"), config)
    }

    pub fn recent_workspaces(&self) -> Vec<RecentWorkspace> {
        self.recent.lock().clone()
    }

    /// 记录常用工作目录：同一 (machine, workspace) 只保留最新一条，按时间倒序、只留最近若干条。
    pub fn record_workspace(&self, machine: &str, workspace: &str) {
        let now = now_ms();
        let mut recent = self.recent.lock();
        recent.retain(|item| !(item.machine == machine && item.workspace == workspace));
        // 最新使用的插到最前：避免同一毫秒内多次记录时按时间戳排序出现并列
        recent.insert(
            0,
            RecentWorkspace {
                machine: machine.to_string(),
                workspace: workspace.to_string(),
                last_used: now,
            },
        );
        recent.truncate(RECENT_WORKSPACE_LIMIT);
        let _ = write_json(&recent_path(&self.home), &*recent);
    }

    fn path(&self, name: &str) -> PathBuf {
        self.home.join("config").join(format!("{name}.json"))
    }
}

/// 配置项 name 必须唯一（docs/DESIGN.md 各「存储」一节）。
fn reject_duplicate_names<T>(
    items: &[T],
    name: impl Fn(&T) -> &str,
    kind: &str,
) -> Result<(), String> {
    let mut seen = std::collections::HashSet::new();
    for item in items {
        let name = name(item);
        if !seen.insert(name) {
            return Err(format!("{kind} name 重复: {name}"));
        }
    }
    Ok(())
}

fn recent_path(home: &std::path::Path) -> PathBuf {
    home.join("config").join("recent_workspaces.json")
}

fn read_json<T: serde::de::DeserializeOwned>(path: &std::path::Path) -> Option<T> {
    let text = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str(&text) {
        Ok(value) => Some(value),
        Err(error) => {
            log::warn!("配置解析失败（{}）: {error}", path.display());
            None
        }
    }
}

fn write_json<T: serde::Serialize + ?Sized>(
    path: &std::path::Path,
    value: &T,
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建配置目录失败: {e}"))?;
    }
    let text = serde_json::to_string_pretty(value).map_err(|e| format!("配置序列化失败: {e}"))?;
    std::fs::write(path, text).map_err(|e| format!("写入 {} 失败: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configs_roundtrip_per_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = ConfigStore::new(dir.path().to_path_buf());

        store
            .set_skills(&[Skill {
                name: "opencli".into(),
                description: "desc".into(),
            }])
            .unwrap();
        assert_eq!(store.skills().len(), 1);

        store.record_workspace("pc", "/w1");
        store.record_workspace("pc", "/w2");
        store.record_workspace("pc", "/w1");
        let recent = store.recent_workspaces();
        assert_eq!(recent.len(), 2, "同一 (machine, workspace) 只保留一条");
        assert_eq!(recent[0].workspace, "/w1", "最近使用的排在最前");

        // 重新打开后仍能读回
        let reopened = ConfigStore::new(dir.path().to_path_buf());
        assert_eq!(reopened.recent_workspaces().len(), 2);
        assert_eq!(reopened.skills()[0].name, "opencli");
    }

    #[test]
    fn named_configs_reject_duplicate_name() {
        let dir = tempfile::tempdir().unwrap();
        let store = ConfigStore::new(dir.path().to_path_buf());

        let skills = [
            Skill {
                name: "opencli".into(),
                description: "a".into(),
            },
            Skill {
                name: "opencli".into(),
                description: "b".into(),
            },
        ];
        assert!(store.set_skills(&skills).is_err(), "技能 name 重复应报错");

        let plans = [
            WorkflowPlanItem {
                name: "w".into(),
                plan: "p".into(),
            },
            WorkflowPlanItem {
                name: "w".into(),
                plan: "p2".into(),
            },
        ];
        assert!(
            store.set_workflow_plans(&plans).is_err(),
            "工作流计划 name 重复应报错"
        );

        let commands = [
            QuickCommand {
                name: "c".into(),
                prompt: "a".into(),
            },
            QuickCommand {
                name: "c".into(),
                prompt: "b".into(),
            },
        ];
        assert!(
            store.set_quick_commands(&commands).is_err(),
            "快捷指令 name 重复应报错"
        );
    }
}
