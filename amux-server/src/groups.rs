//! 群聊会话：元数据（group.sqlite）、transcript（每群聊一个 transcript.sqlite）、
//! 成员普通会话与 @ 驱动（docs/DESIGN.md「群聊会话驱动」「群聊会话存储」）。

use std::collections::HashSet;
use std::sync::Arc;

use agent_client_protocol::schema::v2::TextContent;
use amux_common::api::{Group, GroupMember, GroupMemberTemplate, GroupMessage};
use amux_common::domain::{generate_title, rand_id, ContentBlock, SessionState, StateChangeReason};
use uuid::Uuid;

use crate::sessions::{CreateSessionParams, SessionService, SessionWorktree};
use crate::store::{GroupRow, MemberRow, Store};
use crate::timestamps::now_ms;

/// 群聊会话及成员普通会话的驱动逻辑。
pub struct GroupService {
    store: Arc<Store>,
    sessions: Arc<SessionService>,
}

impl GroupService {
    pub fn new(store: Arc<Store>, sessions: Arc<SessionService>) -> Self {
        Self { store, sessions }
    }

    /// 新建群聊会话：先写群聊元数据，再逐个创建成员普通会话。
    pub async fn create(
        &self,
        members: &[GroupMemberTemplate],
        title: Option<String>,
        project: Option<&str>,
    ) -> Result<Group, String> {
        let members: Vec<GroupMemberTemplate> = members
            .iter()
            .map(|member| GroupMemberTemplate {
                name: member.name.trim().to_string(),
                description: member.description.trim().to_string(),
                machine: member.machine.trim().to_string(),
                agent: member.agent.trim().to_string(),
                workspace: member.workspace.trim().to_string(),
                use_worktree: member.use_worktree,
                worktree_base: member.worktree_base.clone(),
            })
            .collect();
        validate_members(&members)?;
        let id = Uuid::new_v4().to_string();
        let now = now_ms();
        self.store.insert_group(&id, project, now);
        let title = title.unwrap_or_default();
        if !title.is_empty() {
            self.store.set_group_title(&id, &title);
        }

        let mut created: Vec<String> = Vec::new();
        for template in &members {
            let title = format!("{}：{}", template.name, template.description);
            let session = match SessionWorktree::new(
                template.use_worktree,
                template.worktree_base.as_ref(),
            ) {
                Ok(worktree) => {
                    self.sessions
                        .create(CreateSessionParams {
                            machine: &template.machine,
                            agent: &template.agent,
                            workspace: &template.workspace,
                            worktree,
                            title: Some(&title),
                            project,
                        })
                        .await
                }
                Err(error) => Err(error),
            };
            let session = match session {
                Ok(session) => session,
                Err(error) => {
                    for session in created {
                        let _ = self.sessions.delete(&session).await;
                    }
                    self.store.delete_group(&id);
                    return Err(error);
                }
            };
            self.store.link_group_session(&id, &session.id);
            let member_id = Uuid::new_v4().to_string();
            self.store
                .insert_group_member(&crate::store::GroupMemberInsert {
                    id: &member_id,
                    group_id: &id,
                    machine: &template.machine,
                    agent: &template.agent,
                    session_id: &session.id,
                    name: &template.name,
                    description: &template.description,
                    created_at: now_ms(),
                });
            created.push(session.id);
        }
        log::info!(
            "群聊会话已创建: group={id} members={} project={}",
            members.len(),
            project.unwrap_or("")
        );
        self.get(&id)
    }

    pub fn get(&self, id: &str) -> Result<Group, String> {
        let row = self.store.group(id).ok_or("群聊会话不存在")?;
        Ok(self.view(row))
    }

    pub fn list(&self, limit: usize, offset: usize, project: Option<&str>) -> (Vec<Group>, bool) {
        let (rows, has_more) = self.store.groups_page(limit, offset, project);
        (
            rows.into_iter().map(|row| self.view(row)).collect(),
            has_more,
        )
    }

    fn view(&self, row: GroupRow) -> Group {
        let members: Vec<GroupMember> = self
            .store
            .group_members(&row.id)
            .into_iter()
            .map(member_view)
            .collect();
        let sessions = members
            .iter()
            .filter_map(|member| self.store.session(&member.session_id))
            .collect();
        Group {
            id: row.id.clone(),
            title: row.title,
            state: row.state,
            project: row.project,
            pinned: row.pinned,
            created_at: row.created_at,
            updated_at: row.updated_at,
            members,
            sessions,
        }
    }

    /// 用户往群聊发送指令：用户消息落盘后解析 @ 提及并驱动成员。
    pub async fn prompt(&self, id: &str, input: Vec<ContentBlock>) -> Result<(), String> {
        let group = self.get(id)?;
        if group.title.is_empty() {
            let title = generate_title(&blocks_text(&input));
            if !title.is_empty() {
                self.store.set_group_title_if_empty(id, &title);
            }
        }
        let content = serde_json::to_string(&input).unwrap_or_default();
        self.store
            .insert_group_message(id, &rand_id(), None, None, Some(&content), now_ms());
        self.store.touch_group(id);
        self.drive_mentioned(id, &input).await;
        self.recompute_state(id);
        log::info!("群聊指令已发送: group={id}");
        Ok(())
    }

    pub async fn cancel(&self, id: &str) -> Result<(), String> {
        self.get(id)?;
        let mut first_error = None;
        for session_id in self.store.member_sessions(id) {
            if let Err(error) = self.sessions.cancel(&session_id).await {
                log::warn!("取消群聊成员会话失败: group={id} session={session_id}: {error}");
                first_error.get_or_insert(error);
            }
        }
        self.recompute_state(id);
        if let Some(error) = first_error {
            return Err(error);
        }
        log::info!("群聊会话取消请求已发送: group={id}");
        Ok(())
    }

    pub fn configure(
        &self,
        id: &str,
        title: String,
        project: Option<String>,
        pinned: bool,
    ) -> Result<(), String> {
        self.store.group(id).ok_or("群聊会话不存在")?;
        if !title.is_empty() {
            self.store.set_group_title(id, &title);
        }
        self.store.set_group_project(id, project.as_deref());
        self.store.set_group_pinned(id, pinned);
        log::info!("群聊配置已更新: group={id}");
        Ok(())
    }

    /// 删除群聊会话：级联删除成员普通会话。
    pub async fn delete(&self, id: &str) -> Result<(), String> {
        self.store.group(id).ok_or("群聊会话不存在")?;
        let sessions = self.store.member_sessions(id);
        self.store.delete_group(id);
        for session_id in sessions {
            let _ = self.sessions.delete(&session_id).await;
        }
        log::info!("群聊会话已删除: group={id}");
        Ok(())
    }

    pub fn history(&self, id: &str, limit: usize, offset: usize) -> (Vec<GroupMessage>, bool) {
        self.store.group_messages_page(id, limit, offset)
    }

    /// 项目删除后，其下所有群聊会话回到未归属（docs/PRD.md「项目管理」）。
    pub fn unassign_project(&self, project: &str) {
        self.store.unassign_project(project);
    }

    /// 群聊状态重算：任一成员普通会话工作中即为工作中。
    pub fn recompute_state(&self, id: &str) {
        let Some(group) = self.store.group(id) else {
            return;
        };
        let busy = self
            .store
            .member_sessions(id)
            .into_iter()
            .filter_map(|session_id| self.store.session(&session_id))
            .any(|session| session.state == SessionState::Busy);
        let state = if busy {
            SessionState::Busy
        } else {
            SessionState::Idle
        };
        if group.state != state {
            self.store.set_group_state(id, state);
        }
    }

    /// 成员普通会话进入空闲：解析自上次用户消息以来 agent 消息中的 @ 提及。
    pub async fn on_member_idle(&self, session_id: &str, reason: StateChangeReason) {
        let Some(member) = self.store.group_member_by_session(session_id) else {
            return;
        };
        self.recompute_state(&member.group_id);
        if matches!(
            reason,
            StateChangeReason::Cancelled | StateChangeReason::Aborted
        ) {
            return;
        }
        let since = self
            .store
            .last_user_message_at(session_id, member.created_at);
        let mentions = self.mentions_in_messages(&member.group_id, since);
        for member_id in mentions {
            self.drive_member(&member_id).await;
        }
    }

    /// 成员普通会话产生 agent 输出：非空输出进入群聊消息（引用普通会话消息）。
    pub fn on_member_output(&self, session_id: &str, message_id: &str, content: &[ContentBlock]) {
        if content.is_empty() || is_blank_output(content) {
            return;
        }
        let Some(member) = self.store.group_member_by_session(session_id) else {
            return;
        };
        self.store.insert_group_message(
            &member.group_id,
            &rand_id(),
            Some(&member.id),
            Some(message_id),
            None,
            now_ms(),
        );
        self.store.touch_group(&member.group_id);
    }

    /// 解析用户消息中的 @ 提及并驱动对应成员。
    async fn drive_mentioned(&self, group_id: &str, content: &[ContentBlock]) {
        let text = blocks_text(content);
        let members = self.store.group_members(group_id);
        for member_id in mentioned_members(&text, &members) {
            self.drive_member(&member_id).await;
        }
    }

    /// 解析未读窗口内所有群聊消息的 @ 提及（按成员去重）。
    fn mentions_in_messages(&self, group_id: &str, since: u64) -> Vec<String> {
        let members = self.store.group_members(group_id);
        let mut seen = HashSet::new();
        let mut mentions = Vec::new();
        for row in self.store.group_messages_since(group_id, since) {
            let text = match &row.member_id {
                Some(member_id) => {
                    let Some(member) = self.store.group_member(member_id) else {
                        continue;
                    };
                    let Some(session_message_id) = row.session_message_id else {
                        continue;
                    };
                    self.store
                        .session_message(&member.session_id, &session_message_id)
                        .map(|content| blocks_text(&content))
                        .unwrap_or_default()
                }
                None => row
                    .user_content
                    .and_then(|content| serde_json::from_str::<Vec<ContentBlock>>(&content).ok())
                    .map(|content| blocks_text(&content))
                    .unwrap_or_default(),
            };
            for member_id in mentioned_members(&text, &members) {
                if seen.insert(member_id.clone()) {
                    mentions.push(member_id);
                }
            }
        }
        mentions
    }

    /// 驱动一个成员普通会话：以用户角色发送群聊未读消息与行为准则。
    async fn drive_member(&self, member_id: &str) {
        let Some(member) = self.store.group_member(member_id) else {
            return;
        };
        let Some(group) = self.store.group(&member.group_id) else {
            return;
        };
        let members = self.store.group_members(&group.id);
        let since = self
            .store
            .last_user_message_at(&member.session_id, member.created_at);
        let unread = self
            .store
            .group_messages_since(&group.id, since)
            .into_iter()
            .filter_map(|row| self.message_line(&row))
            .collect::<Vec<_>>();
        let prompt = drive_prompt(&member, &members, &unread);
        match self
            .sessions
            .prompt(
                &member.session_id,
                vec![ContentBlock::Text(TextContent::new(prompt))],
            )
            .await
        {
            Ok(()) => {
                // ACP 的 busy 事件晚于 prompt 响应到达；先落状态，避免群聊在两者之间
                // 被重算为空闲并提前触发自动化完成。
                self.store.set_state(&member.session_id, SessionState::Busy);
                self.recompute_state(&group.id);
                log::info!(
                    "群聊成员已驱动: group={} member={} name={} session={}",
                    group.id,
                    member.id,
                    member.name,
                    member.session_id
                );
            }
            Err(error) => log::warn!(
                "群聊成员驱动失败: group={} member={} session={}: {error}",
                group.id,
                member.id,
                member.session_id
            ),
        }
    }

    fn message_line(&self, row: &crate::store::GroupMessageRow) -> Option<String> {
        let name = match &row.member_id {
            Some(member_id) => self.store.group_member(member_id)?.name,
            None => "user".to_string(),
        };
        let text = match &row.member_id {
            Some(member_id) => {
                let member = self.store.group_member(member_id)?;
                let content = self
                    .store
                    .session_message(&member.session_id, row.session_message_id.as_deref()?)?;
                blocks_text(&content)
            }
            None => {
                let content =
                    serde_json::from_str::<Vec<ContentBlock>>(row.user_content.as_deref()?).ok()?;
                blocks_text(&content)
            }
        };
        if text.trim().is_empty() {
            return None;
        }
        Some(format!("{name}: {text}"))
    }
}

fn member_view(member: MemberRow) -> GroupMember {
    GroupMember {
        id: member.id,
        name: member.name,
        description: member.description,
        machine: member.machine,
        agent: member.agent,
        session_id: member.session_id,
        pinned: member.pinned,
        created_at: member.created_at,
        updated_at: member.updated_at,
    }
}

fn validate_members(members: &[GroupMemberTemplate]) -> Result<(), String> {
    if members.is_empty() {
        return Err("群聊成员不能为空".into());
    }
    let mut names = HashSet::new();
    for member in members {
        if member.name.trim().is_empty()
            || member.description.trim().is_empty()
            || member.machine.trim().is_empty()
            || member.agent.trim().is_empty()
            || member.workspace.trim().is_empty()
        {
            return Err("群聊成员的名称、角色描述、机器、agent 和工作目录不能为空".into());
        }
        if matches!(member.name.as_str(), "all" | "user") {
            return Err("群聊成员名称不能为 all 或 user".into());
        }
        if !names.insert(member.name.trim()) {
            return Err(format!("群聊成员名称重复: {}", member.name));
        }
        if member.use_worktree && member.worktree_base.is_none() {
            return Err("开启 worktree 时必须指定基分支".into());
        }
    }
    Ok(())
}

/// 按 `<成员名>` 解析消息中的 `@name`；用户名为 `user`，`@all` 匹配所有 agent 成员。
///
/// 在整个消息内容中按大小写精确匹配（docs/DESIGN.md「群聊会话驱动」）。
fn mentioned_members(text: &str, members: &[MemberRow]) -> Vec<String> {
    let mut mentions = Vec::new();
    let all = has_mention(text, "all");
    for member in members {
        if all || has_mention(text, &member.name) {
            mentions.push(member.id.clone());
        }
    }
    mentions
}

fn has_mention(text: &str, name: &str) -> bool {
    text.contains(&format!("@{name}"))
}

fn blocks_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 只含空白文本块视为空输出；图片等非文本内容算作有效输出。
fn is_blank_output(content: &[ContentBlock]) -> bool {
    content.iter().all(|block| match block {
        ContentBlock::Text(text) => text.text.trim().is_empty(),
        _ => false,
    })
}

/// 成员驱动消息模板（docs/DESIGN.md「群聊会话驱动」）。
fn drive_prompt(member: &MemberRow, members: &[MemberRow], unread: &[String]) -> String {
    let others = members
        .iter()
        .filter(|candidate| candidate.id != member.id)
        .map(|candidate| {
            format!(
                "- {}（运行在机器 `{}`）：{}",
                candidate.name, candidate.machine, candidate.description
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let unread = unread
        .iter()
        .map(|line| format!("- {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "你现在在一个群聊会话中，你的每个非空输出都会作为群聊消息，系统会解析每条群聊消息内容的 @ 提及，若 @ agent 成员则会唤醒 agent 读取群聊消息并执行任务。\n\n\
         你在群聊中的名称是 `{}`，角色描述是：{}，运行在机器 `{}` 上。\n\n\
         群聊成员除了用户（用户的群聊名称为 `user`）和你，还有以下 agent 成员：\n{}\n\n\
         当前有成员发送消息 @ 你，这是群聊自上次读取以来未读的消息列表\n{}\n\n\
         行为准则\n\
         - 当你需要其他成员协助或需要通知其他成员时，可以使用 @ 成员（格式为 @<name>，前后空格）来唤醒其他 agent 成员或通知用户\n\
         - 当你读取未读消息后，判断无需做任何事时，保持静默不输出任何内容",
        member.name, member.description, member.machine, others, unread
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(id: &str, name: &str) -> MemberRow {
        MemberRow {
            id: id.into(),
            group_id: "g1".into(),
            machine: "pc".into(),
            agent: "codex".into(),
            session_id: format!("s-{id}"),
            name: name.into(),
            description: "".into(),
            pinned: false,
            created_at: 1,
            updated_at: 1,
        }
    }

    #[test]
    fn mentions_match_exact_text_without_whitespace_boundaries() {
        let members = vec![member("m1", "coder"), member("m2", "reviewer")];
        assert_eq!(mentioned_members("@coder 实现", &members), ["m1"]);
        assert_eq!(mentioned_members("请@reviewer审查", &members), ["m2"]);
        assert_eq!(mentioned_members("@coder@reviewer", &members), ["m1", "m2"]);
        assert_eq!(mentioned_members("mail@coder", &members), ["m1"]);
        assert!(mentioned_members("@Coder 实现", &members).is_empty());
        assert!(mentioned_members("@user 请确认", &members).is_empty());
    }

    #[test]
    fn all_mentions_every_member() {
        let members = vec![member("m1", "coder"), member("m2", "reviewer")];
        assert_eq!(mentioned_members("@all开始", &members), ["m1", "m2"]);
    }

    #[test]
    fn reserved_member_names_are_rejected() {
        for name in ["all", "user"] {
            let member = GroupMemberTemplate {
                name: name.into(),
                description: "负责实现".into(),
                machine: "pc".into(),
                agent: "codex".into(),
                workspace: "/repo".into(),
                use_worktree: false,
                worktree_base: None,
            };
            assert!(validate_members(&[member]).is_err(), "{name}");
        }
    }

    #[test]
    fn drive_prompt_lists_other_members_and_unread_messages() {
        let members = vec![
            MemberRow {
                description: "负责实现".into(),
                ..member("m1", "coder")
            },
            MemberRow {
                description: "负责审查".into(),
                ..member("m2", "reviewer")
            },
        ];
        let prompt = drive_prompt(
            &members[1],
            &members,
            &["user: @reviewer 请审查".to_string()],
        );
        assert!(prompt.contains("你在群聊中的名称是 `reviewer`"));
        assert!(prompt.contains("- coder（运行在机器 `pc`）：负责实现"));
        assert!(prompt.contains("- user: @reviewer 请审查"));
        assert!(!prompt.contains("- reviewer（运行在机器"));
    }
}
