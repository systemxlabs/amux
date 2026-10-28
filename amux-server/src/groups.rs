//! 群聊会话：元数据（group.sqlite）、transcript（每群聊一个 transcript.sqlite）、
//! 成员普通会话与 @ 驱动（docs/DESIGN.md「群聊会话驱动」「群聊会话存储」）。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use agent_client_protocol::schema::v2::TextContent;
use amux_common::api::{Group, GroupMember, GroupMemberTemplate, GroupMessage};
use amux_common::domain::{generate_title, rand_id, ContentBlock, SessionState};
use uuid::Uuid;

use crate::db;
use crate::error::{ServerError, ServerResult};
use crate::sessions::{CreateSessionParams, SessionService, SessionWorktree};
use crate::store::{self, GroupMessageRow, GroupRow, MemberRow, Store};
use crate::timestamps::now_ms;

/// 群聊会话及成员普通会话的驱动逻辑。
pub struct GroupService {
    store: Arc<Store>,
    sessions: Arc<SessionService>,
    /// 串行化解析：同一批未解析消息只能被取走一次，避免并发触发重复驱动。
    parse_lock: tokio::sync::Mutex<()>,
}

impl GroupService {
    pub fn new(store: Arc<Store>, sessions: Arc<SessionService>) -> Self {
        Self {
            store,
            sessions,
            parse_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// 新建群聊会话：先写群聊元数据，再逐个创建成员普通会话。
    pub async fn create(
        &self,
        members: &[GroupMemberTemplate],
        title: Option<String>,
        project: Option<&str>,
    ) -> ServerResult<Group> {
        let members: Vec<GroupMemberTemplate> = members
            .iter()
            .map(|member| GroupMemberTemplate {
                name: member.name.trim().to_string(),
                description: member.description.trim().to_string(),
                machine: member.machine.trim().to_string(),
                agent: member.agent.trim().to_string(),
                model: member
                    .model
                    .as_deref()
                    .map(str::trim)
                    .filter(|model| !model.is_empty())
                    .map(str::to_string),
                workspace: member.workspace.trim().to_string(),
                use_worktree: member.use_worktree,
                worktree_base: member.worktree_base.clone(),
            })
            .collect();
        validate_members(&members)?;
        let id = Uuid::new_v4().to_string();
        let now = now_ms();
        db::blocking_sync(|| self.store.insert_group(&id, project, now));
        let title = title.unwrap_or_default();
        if !title.is_empty() {
            db::blocking_sync(|| self.store.set_group_title(&id, &title));
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
                            model: template.model.as_deref(),
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
                    db::blocking_sync(|| self.store.delete_group(&id));
                    return Err(error);
                }
            };
            db::blocking_sync(|| self.store.link_group_session(&id, &session.id));
            let member_id = Uuid::new_v4().to_string();
            db::blocking_sync(|| {
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
                    })
            });
            created.push(session.id);
        }
        log::info!(
            "群聊会话已创建: group={id} members={} project={}",
            members.len(),
            project.unwrap_or("")
        );
        db::blocking_sync(|| self.get(&id))
    }

    pub fn get(&self, id: &str) -> ServerResult<Group> {
        let row = self
            .store
            .group(id)
            .ok_or_else(|| ServerError::not_found("群聊会话不存在"))?;
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
            unread: row.unread,
            created_at: row.created_at,
            updated_at: row.updated_at,
            members,
            sessions,
        }
    }

    /// 用户往群聊发送指令：用户消息落盘后解析 @ 提及并驱动成员。
    pub async fn prompt(&self, id: &str, input: Vec<ContentBlock>) -> ServerResult<()> {
        let group = db::blocking_sync(|| self.get(id))?;
        if group.title.is_empty() {
            let title = generate_title(&blocks_text(&input));
            if !title.is_empty() {
                db::blocking_sync(|| self.store.set_group_title_if_empty(id, &title));
            }
        }
        let content = serde_json::to_string(&input).unwrap_or_default();
        db::blocking_sync(|| {
            self.store
                .insert_group_message(id, &rand_id(), None, None, Some(&content), now_ms());
            self.store.touch_group(id);
        });
        self.parse(id).await;
        db::blocking_sync(|| self.recompute_state(id));
        log::info!("群聊指令已发送: group={id}");
        Ok(())
    }

    pub async fn cancel(&self, id: &str) -> ServerResult<()> {
        db::blocking_sync(|| self.get(id))?;
        let mut first_error = None;
        for session_id in db::blocking_sync(|| self.store.member_sessions(id)) {
            if let Err(error) = self.sessions.cancel(&session_id).await {
                log::warn!("取消群聊成员会话失败: group={id} session={session_id}: {error}");
                first_error.get_or_insert(error);
            }
        }
        db::blocking_sync(|| self.recompute_state(id));
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
    ) -> ServerResult<()> {
        self.store
            .group(id)
            .ok_or_else(|| ServerError::not_found("群聊会话不存在"))?;
        if !title.is_empty() {
            self.store.set_group_title(id, &title);
        }
        self.store.set_group_project(id, project.as_deref());
        self.store.set_group_pinned(id, pinned);
        log::info!("群聊配置已更新: group={id}");
        Ok(())
    }

    /// 标记群聊会话及其成员普通会话已读（docs/DESIGN.md「会话交互视图」）。
    pub fn read(&self, id: &str) -> ServerResult<()> {
        self.store
            .group(id)
            .ok_or_else(|| ServerError::not_found("群聊会话不存在"))?;
        self.store.set_group_unread(id, false);
        for session_id in self.store.member_sessions(id) {
            self.store.set_session_unread(&session_id, false);
        }
        Ok(())
    }

    /// 删除群聊会话：级联删除成员普通会话。
    pub async fn delete(&self, id: &str) -> ServerResult<()> {
        db::blocking_sync(|| {
            self.store
                .group(id)
                .ok_or_else(|| ServerError::not_found("群聊会话不存在"))
        })?;
        let sessions = db::blocking_sync(|| self.store.member_sessions(id));
        db::blocking_sync(|| self.store.delete_group(id));
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

    /// 成员普通会话状态变更：重算群聊状态并触发解析。
    pub async fn on_member_state_change(&self, group_id: &str) {
        db::blocking_sync(|| self.recompute_state(group_id));
        self.parse(group_id).await;
    }

    /// 解析群聊中已完整的未解析消息，驱动被 @ 的成员（docs/DESIGN.md「群聊会话驱动」）。
    pub async fn parse(&self, group_id: &str) {
        let guard = self.parse_lock.lock().await;
        let pending = db::blocking_sync(|| take_pending(&self.store, group_id));
        drop(guard);
        let Some((mentions, unread_end)) = pending else {
            return;
        };
        for member_id in mentions {
            self.drive_member(&member_id, &unread_end).await;
        }
    }

    /// 成员普通会话产生 agent 输出：非空输出进入群聊消息（引用普通会话消息）。
    /// 返回新增消息所属的群聊 ID；同一流式消息的后续分片返回 None。
    pub fn on_member_output(&self, session_id: &str, message_id: &str) -> Option<String> {
        record_member_output(&self.store, session_id, message_id)
    }

    /// 驱动一个成员普通会话：以用户角色发送群聊未读消息与行为准则。
    async fn drive_member(&self, member_id: &str, unread_end: &GroupMessageRow) {
        let Some((group_id, session_id, member_name, prompt)) = db::blocking_sync(|| {
            let member = self.store.group_member(member_id)?;
            let group = self.store.group(&member.group_id)?;
            let members = self.store.group_members(&group.id);
            let since = self
                .store
                .last_user_message_at(&member.session_id, member.created_at);
            let mut rows = self.store.group_messages_since(&group.id, since);
            // 未读列表截止到本次待解析列表的最后一条消息。
            match rows.iter().position(|row| row.id == unread_end.id) {
                Some(index) => rows.truncate(index + 1),
                None => rows.clear(),
            }
            let unread = rows
                .iter()
                .filter_map(|row| self.unread_message(row))
                .collect::<Vec<_>>();
            let prompt = drive_prompt(&member, &members, &unread);
            Some((
                group.id.clone(),
                member.session_id.clone(),
                member.name.clone(),
                prompt,
            ))
        }) else {
            return;
        };
        match self
            .sessions
            .prompt(
                &session_id,
                vec![ContentBlock::Text(TextContent::new(prompt))],
            )
            .await
        {
            Ok(()) => {
                // ACP 的 busy 事件晚于 prompt 响应到达；先落状态，避免群聊在两者之间
                // 被重算为空闲并提前触发自动化完成。
                db::blocking_sync(|| {
                    self.store.set_state(&session_id, SessionState::Busy);
                    self.recompute_state(&group_id);
                });
                log::info!(
                    "群聊成员已驱动: group={group_id} member={member_id} name={member_name} session={session_id}"
                );
            }
            Err(error) => log::warn!(
                "群聊成员驱动失败: group={group_id} member={member_id} session={session_id}: {error}"
            ),
        }
    }

    fn unread_message(&self, row: &store::GroupMessageRow) -> Option<UnreadMessage> {
        let member = match &row.member_id {
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
        Some(UnreadMessage {
            time: format_prompt_time(row.created_at),
            member,
            text,
        })
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

fn validate_members(members: &[GroupMemberTemplate]) -> ServerResult<()> {
    if members.is_empty() {
        return Err(ServerError::invalid_input("群聊成员不能为空"));
    }
    let mut names = HashSet::new();
    for member in members {
        if member.name.trim().is_empty()
            || member.description.trim().is_empty()
            || member.machine.trim().is_empty()
            || member.agent.trim().is_empty()
            || member.workspace.trim().is_empty()
        {
            return Err(ServerError::invalid_input(
                "群聊成员的名称、角色描述、机器、agent 和工作目录不能为空",
            ));
        }
        if matches!(member.name.as_str(), "all" | "user") {
            return Err(ServerError::invalid_input("群聊成员名称不能为 all 或 user"));
        }
        if !names.insert(member.name.trim()) {
            return Err(ServerError::invalid_input(format!(
                "群聊成员名称重复: {}",
                member.name
            )));
        }
        if member.use_worktree && member.worktree_base.is_none() {
            return Err(ServerError::invalid_input("开启 worktree 时必须指定基分支"));
        }
    }
    Ok(())
}

/// 成员普通会话产生 agent 输出：非空输出写入群聊消息引用。
/// `created_at` 取该消息在成员普通会话中的创建时间，与成员会话时间线一致
/// （docs/DESIGN.md「群聊会话存储」）。
fn record_member_output(store: &Store, session_id: &str, message_id: &str) -> Option<String> {
    let member = store.group_member_by_session(session_id)?;
    let content = store.session_message(&member.session_id, message_id)?;
    if content.is_empty() || is_blank_output(&content) {
        return None;
    }
    let created_at = store.session_message_created_at(&member.session_id, message_id)?;
    let inserted = store.insert_group_message(
        &member.group_id,
        &rand_id(),
        Some(&member.id),
        Some(message_id),
        None,
        created_at,
    );
    store.set_group_unread(&member.group_id, true);
    store.touch_group(&member.group_id);
    inserted.then_some(member.group_id)
}

/// 取出本次可解析的消息前缀，提取 @ 提及并置为已解析。
///
/// 待解析列表是未解析消息的前缀：遍历时遇到「状态工作中且其后没有该成员消息」的
/// agent 消息即截止，这条消息可能仍在流式传输中（docs/DESIGN.md「群聊会话驱动」）。
fn take_pending(store: &Store, group_id: &str) -> Option<(Vec<String>, GroupMessageRow)> {
    let rows = store.unparsed_group_messages(group_id);
    if rows.is_empty() {
        return None;
    }
    let members = store.group_members(group_id);
    let last_by_member = last_index_by_member(&rows);
    let cutoff = rows
        .iter()
        .enumerate()
        .find_map(|(index, row)| {
            let member_id = row.member_id.as_deref()?;
            let member = members.iter().find(|member| member.id == member_id)?;
            if store.session(&member.session_id)?.state != SessionState::Busy {
                return None;
            }
            (last_by_member.get(member_id) == Some(&index)).then_some(index)
        })
        .unwrap_or(rows.len());
    if cutoff == 0 {
        return None;
    }

    let pending = &rows[..cutoff];
    let mut seen = HashSet::new();
    let mut mentions = Vec::new();
    for row in pending {
        let Some(text) = message_text(store, row) else {
            continue;
        };
        for member_id in mentioned_members(&text, &members, row.member_id.as_deref()) {
            if seen.insert(member_id.clone()) {
                mentions.push(member_id);
            }
        }
    }
    let last = pending.last()?.clone();
    store.mark_group_messages_parsed_through(group_id, &last);
    Some((mentions, last))
}

/// 每个成员在未解析列表中的最后一条消息下标。
fn last_index_by_member(rows: &[GroupMessageRow]) -> HashMap<&str, usize> {
    rows.iter()
        .enumerate()
        .filter_map(|(index, row)| row.member_id.as_deref().map(|id| (id, index)))
        .collect()
}

/// 消息文本：用户消息直接解析，成员消息读取普通会话中的最新完整内容。
fn message_text(store: &Store, row: &GroupMessageRow) -> Option<String> {
    match &row.member_id {
        Some(member_id) => {
            let member = store.group_member(member_id)?;
            let content =
                store.session_message(&member.session_id, row.session_message_id.as_deref()?)?;
            Some(blocks_text(&content))
        }
        None => {
            let content =
                serde_json::from_str::<Vec<ContentBlock>>(row.user_content.as_deref()?).ok()?;
            Some(blocks_text(&content))
        }
    }
}

/// 按 `<成员名>` 解析消息中的 `@name`；用户名为 `user`，`@all` 匹配所有 agent 成员。
///
/// 在整个消息内容中按大小写精确匹配（docs/DESIGN.md「群聊会话驱动」）。
fn mentioned_members(
    text: &str,
    members: &[MemberRow],
    sender_member_id: Option<&str>,
) -> Vec<String> {
    let mut mentions = Vec::new();
    let all = has_mention(text, "all");
    for member in members {
        if sender_member_id == Some(member.id.as_str()) {
            continue;
        }
        if all || has_mention(text, &member.name) {
            mentions.push(member.id.clone());
        }
    }
    mentions
}

fn has_mention(text: &str, name: &str) -> bool {
    text.contains(&format!("@{name}"))
}

/// 流式文本按相邻 text block 分片存储，按原顺序无分隔拼接。
fn blocks_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect()
}

/// 只含空白文本块视为空输出；图片等非文本内容算作有效输出。
fn is_blank_output(content: &[ContentBlock]) -> bool {
    content.iter().all(|block| match block {
        ContentBlock::Text(text) => text.text.trim().is_empty(),
        _ => false,
    })
}

#[derive(serde::Serialize)]
struct UnreadMessage {
    time: String,
    member: String,
    text: String,
}

fn format_prompt_time(timestamp_ms: u64) -> String {
    jiff::Timestamp::from_millisecond(timestamp_ms as i64)
        .map(|timestamp| {
            timestamp
                .to_zoned(jiff::tz::TimeZone::system())
                .strftime("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_default()
}

/// 成员驱动消息模板（docs/DESIGN.md「群聊会话驱动」）。
fn drive_prompt(member: &MemberRow, members: &[MemberRow], unread: &[UnreadMessage]) -> String {
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
        .map(|message| serde_json::to_string(message).expect("未读消息应可序列化为 JSON"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "你现在在一个群聊会话中，你的每个非空输出都会作为群聊消息，系统会解析每条群聊消息内容的 @ 提及，若 @ agent 成员则会唤醒 agent 读取群聊消息并执行任务。\n\n\
         你在群聊中的名称是 `{}`，角色描述是：{}，运行在机器 `{}` 上。\n\n\
         群聊成员除了用户（用户的群聊名称为 `user`）和你，还有以下 agent 成员：\n{}\n\n\
         行为准则\n\
         - 当你需要其他成员协助或需要通知其他成员时，可以使用 @ 成员（格式为 @<name>）来唤醒其他 agent 成员或通知用户\n\
         - 当你读取未读消息后判断无需做任何事时，保持静默不输出任何内容（包括「静默」「无需操作」这类占位回复）\n\n\
         当前有成员发送消息 @ 你，这是你自上次读取后的所有未读消息\n\
         <unread_messages>\n{}\n</unread_messages>",
        member.name, member.description, member.machine, others, unread
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::GroupMemberInsert;

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

    fn session(id: &str) -> amux_common::api::Session {
        amux_common::api::Session {
            id: id.to_string(),
            workflow_id: None,
            group_id: None,
            machine: "pc".into(),
            agent: "codex".into(),
            model: None,
            title: String::new(),
            state: SessionState::Idle,
            project: None,
            pinned: false,
            unread: false,
            workspace: "/tmp".into(),
            worktree_dir: String::new(),
            created_at: 1,
            updated_at: 1,
        }
    }

    /// 新建带两个成员（coder / reviewer）的群聊测试数据。
    fn group_with_members(store: &Store) {
        store.insert_group("g1", None, 1);
        for (session_id, member_id, name) in [("s-m1", "m1", "coder"), ("s-m2", "m2", "reviewer")] {
            store.insert_session(&session(session_id)).unwrap();
            store.insert_group_member(&GroupMemberInsert {
                id: member_id,
                group_id: "g1",
                machine: "pc",
                agent: "codex",
                session_id,
                name,
                description: "",
                created_at: 1,
            });
        }
    }

    fn agent_message(
        store: &Store,
        member_id: &str,
        session_id: &str,
        id: &str,
        text: &str,
        at: u64,
    ) {
        store.upsert_message(
            session_id,
            id,
            "agent",
            &serde_json::to_string(&vec![ContentBlock::Text(TextContent::new(text))]).unwrap(),
            at,
        );
        assert!(store.insert_group_message(
            "g1",
            &format!("g-{id}"),
            Some(member_id),
            Some(id),
            None,
            at
        ));
    }

    /// 工作中的成员只解析到其最后一条消息之前：该消息可能仍在流式传输。
    #[test]
    fn pending_stops_before_working_members_last_message() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        group_with_members(&store);

        store.insert_group_message(
            "g1",
            "u1",
            None,
            None,
            Some(r#"[{"type":"text","text":"@coder 开始"}]"#),
            10,
        );
        agent_message(&store, "m1", "s-m1", "a1", "完成，@reviewer 请审查", 20);
        store.set_state("s-m1", SessionState::Busy);

        // coder 工作中：自己最后一条消息作为截止点，只解析前面的用户消息
        let (mentions, last) = take_pending(&store, "g1").unwrap();
        assert_eq!(mentions, ["m1"]);
        assert_eq!(last.id, "u1");
        assert_eq!(
            store
                .unparsed_group_messages("g1")
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            ["g-a1"]
        );

        // coder 发出下一条消息后，上一条已完整，可以解析
        agent_message(&store, "m1", "s-m1", "a2", "@reviewer 再看一次", 30);
        let (mentions, last) = take_pending(&store, "g1").unwrap();
        assert_eq!(mentions, ["m2"]);
        assert_eq!(last.id, "g-a1");

        // coder 回到空闲，最后一条消息也可解析
        store.set_state("s-m1", SessionState::Idle);
        let (mentions, last) = take_pending(&store, "g1").unwrap();
        assert_eq!(mentions, ["m2"]);
        assert_eq!(last.id, "g-a2");
        assert!(store.unparsed_group_messages("g1").is_empty());
    }

    /// @all 展开为所有成员，发送者被排除，同一成员只驱动一次。
    #[test]
    fn pending_expands_all_excludes_sender_and_dedupes() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        group_with_members(&store);

        store.insert_group_message(
            "g1",
            "u1",
            None,
            None,
            Some(r#"[{"type":"text","text":"@all 请开始"}]"#),
            10,
        );
        let mut mentions = take_pending(&store, "g1").unwrap().0;
        mentions.sort();
        assert_eq!(mentions, ["m1", "m2"]);

        agent_message(&store, "m1", "s-m1", "a1", "@all @coder @reviewer", 20);
        assert_eq!(take_pending(&store, "g1").unwrap().0, ["m2"]);

        // 同一消息重复写入不会新增行，也不会重复进入待解析列表
        assert!(!store.insert_group_message("g1", "dup", Some("m1"), Some("a1"), None, 30));
    }

    /// 群聊 agent 消息的创建时间取自成员普通会话中的消息，而不是当前时刻。
    #[test]
    fn member_output_uses_session_message_created_at() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        group_with_members(&store);
        store.upsert_message(
            "s-m1",
            "a1",
            "agent",
            &serde_json::to_string(&vec![ContentBlock::Text(TextContent::new("完成"))]).unwrap(),
            42,
        );

        assert_eq!(
            record_member_output(&store, "s-m1", "a1").as_deref(),
            Some("g1")
        );
        let rows = store.unparsed_group_messages("g1");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].session_message_id.as_deref(), Some("a1"));
        assert_eq!(rows[0].created_at, 42);

        // 同一流式消息的后续分片不会新增群聊消息
        assert_eq!(record_member_output(&store, "s-m1", "a1"), None);
    }

    #[test]
    fn mentions_match_exact_text_without_whitespace_boundaries() {
        let members = vec![member("m1", "coder"), member("m2", "reviewer")];
        assert_eq!(mentioned_members("@coder 实现", &members, None), ["m1"]);
        assert_eq!(mentioned_members("请@reviewer审查", &members, None), ["m2"]);
        assert_eq!(
            mentioned_members("@coder@reviewer", &members, None),
            ["m1", "m2"]
        );
        assert_eq!(mentioned_members("mail@coder", &members, None), ["m1"]);
        assert!(mentioned_members("@Coder 实现", &members, None).is_empty());
        assert!(mentioned_members("@user 请确认", &members, None).is_empty());
    }

    #[test]
    fn mentions_match_across_stream_chunks() {
        let members = vec![member("m1", "fabric-coder")];
        let content = vec![
            ContentBlock::Text(TextContent::new("请 @fabric")),
            ContentBlock::Text(TextContent::new("-coder 处理")),
        ];
        assert_eq!(blocks_text(&content), "请 @fabric-coder 处理");
        assert_eq!(
            mentioned_members(&blocks_text(&content), &members, None),
            ["m1"]
        );
    }

    #[test]
    fn mentions_exclude_sender() {
        let members = vec![member("m1", "coder"), member("m2", "reviewer")];
        assert_eq!(
            mentioned_members("@coder @reviewer", &members, Some("m1")),
            ["m2"]
        );
        assert_eq!(mentioned_members("@all", &members, Some("m2")), ["m1"]);
        assert!(mentioned_members("@reviewer", &members, Some("m2")).is_empty());
    }

    #[test]
    fn all_mentions_every_member() {
        let members = vec![member("m1", "coder"), member("m2", "reviewer")];
        assert_eq!(mentioned_members("@all开始", &members, None), ["m1", "m2"]);
    }

    #[test]
    fn reserved_member_names_are_rejected() {
        for name in ["all", "user"] {
            let member = GroupMemberTemplate {
                name: name.into(),
                description: "负责实现".into(),
                machine: "pc".into(),
                agent: "codex".into(),
                model: None,
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
            &[UnreadMessage {
                time: "2026-10-22 22:15:03".to_string(),
                member: "user".to_string(),
                text: "@reviewer 请审查".to_string(),
            }],
        );
        assert!(prompt.contains("你在群聊中的名称是 `reviewer`"));
        assert!(prompt.contains("- coder（运行在机器 `pc`）：负责实现"));
        assert!(prompt.contains(
            r#"{"time":"2026-10-22 22:15:03","member":"user","text":"@reviewer 请审查"}"#
        ));
        assert!(prompt.contains("<unread_messages>"));
        assert!(!prompt.contains("- reviewer（运行在机器"));
    }
}
