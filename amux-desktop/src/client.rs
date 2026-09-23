//! Client API 客户端：经 HTTPS 调用 Server（docs/DESIGN.md「Client-Server 通信」）。

use amux_common::api::*;
use amux_common::domain::{
    Activity, ContentBlock, FsEntry, FsListResult, FsReadResult, GitDiffResult,
    SessionConfigOption, SessionPlanEntry, SlashCommand,
};
use futures_util::StreamExt as _;
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::config::Connection;

/// 目录列表请求的页大小（Daemon 单页上限 500）。
const DIRS_PAGE_LIMIT: usize = 500;

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl Client {
    pub fn new(connection: &Connection) -> Self {
        Self {
            http: reqwest::Client::new(),
            base: connection.base_url(),
            token: connection.token.clone(),
        }
    }

    /// 连通性与认证检查（`GET /machines`）。
    pub async fn ping(&self) -> Result<(), String> {
        let _: Vec<Machine> = self.get_json("/machines").await?;
        Ok(())
    }

    // ---------- 机器与 agent ----------

    pub async fn machines(&self) -> Result<Vec<Machine>, String> {
        self.get_json("/machines").await
    }

    pub async fn agents(&self, machine: &str) -> Result<Vec<Agent>, String> {
        self.get_json(&format!("/machines/{machine}/agents")).await
    }

    pub async fn rediscover(&self, machine: &str) -> Result<Vec<Agent>, String> {
        self.post_json(
            &format!("/machines/{machine}/agents/rediscover"),
            &serde_json::json!({}),
        )
        .await
    }

    pub async fn restart_agent(&self, machine: &str, agent: &str) -> Result<(), String> {
        self.post_empty(
            &format!("/machines/{machine}/agents/{agent}/restart"),
            &serde_json::json!({}),
        )
        .await
    }

    /// 列目录；`dirs_only` 只返回子目录（工作目录前缀匹配用）。
    pub async fn list_dir(
        &self,
        machine: &str,
        path: Option<&str>,
        limit: usize,
        offset: usize,
        dirs_only: bool,
    ) -> Result<FsListResult, String> {
        let path = path.unwrap_or_default();
        self.get_json(&format!(
            "/machines/{machine}/list_dir?path={}&limit={limit}&offset={offset}&dirs_only={dirs_only}",
            urlencode(path)
        ))
        .await
    }

    /// 拉取目录下全部子目录：分页续拉到底，前缀匹配要在完整列表上做过滤。
    pub async fn list_all_dirs(&self, machine: &str, dir: &str) -> Result<Vec<FsEntry>, String> {
        let mut entries = Vec::new();
        let mut offset = 0;
        loop {
            let page = self
                .list_dir(machine, Some(dir), DIRS_PAGE_LIMIT, offset, true)
                .await?;
            entries.extend(page.entries);
            if !page.has_more || page.next_offset <= offset {
                return Ok(entries);
            }
            offset = page.next_offset;
        }
    }

    pub async fn read_file(
        &self,
        machine: &str,
        path: &str,
        limit: usize,
        offset: usize,
    ) -> Result<FsReadResult, String> {
        self.get_json(&format!(
            "/machines/{machine}/read_file?path={}&limit={limit}&offset={offset}",
            urlencode(path)
        ))
        .await
    }

    // ---------- 普通会话 ----------

    pub async fn create_session(&self, request: &CreateSessionRequest) -> Result<Session, String> {
        self.post_json("/sessions", request).await
    }

    pub async fn sessions(
        &self,
        limit: usize,
        offset: usize,
        project: Option<&str>,
    ) -> Result<SessionList, String> {
        let mut query = format!("limit={limit}&offset={offset}");
        if let Some(project) = project {
            query.push_str(&format!("&project={}", urlencode(project)));
        }
        self.get_json(&format!("/sessions?{query}")).await
    }

    pub async fn session(&self, id: &str) -> Result<Session, String> {
        self.get_json(&format!("/sessions/{id}")).await
    }

    pub async fn prompt(&self, id: &str, input: Vec<ContentBlock>) -> Result<(), String> {
        self.post_empty(&format!("/sessions/{id}"), &PromptRequest { input })
            .await
    }

    pub async fn cancel(&self, id: &str) -> Result<(), String> {
        self.post_empty(&format!("/sessions/{id}/cancel"), &serde_json::json!({}))
            .await
    }

    pub async fn delete_session(&self, id: &str) -> Result<(), String> {
        self.delete(&format!("/sessions/{id}")).await
    }

    pub async fn configure_session(
        &self,
        id: &str,
        title: Option<String>,
        config: Option<SessionConfigSetting>,
        project: Option<Option<String>>,
    ) -> Result<(), String> {
        self.post_empty(
            &format!("/sessions/{id}/configure"),
            &ConfigureSessionRequest {
                title,
                config,
                project,
            },
        )
        .await
    }

    pub async fn config_options(&self, id: &str) -> Result<Vec<SessionConfigOption>, String> {
        let response: ConfigOptions = self
            .get_json(&format!("/sessions/{id}/config_options"))
            .await?;
        Ok(response.options)
    }

    pub async fn slash_commands(&self, id: &str) -> Result<Vec<SlashCommand>, String> {
        let response: SlashCommands = self
            .get_json(&format!("/sessions/{id}/slash_commands"))
            .await?;
        Ok(response.commands)
    }

    pub async fn plan(&self, id: &str) -> Result<Vec<SessionPlanEntry>, String> {
        let response: Plan = self.get_json(&format!("/sessions/{id}/plan")).await?;
        Ok(response.entries)
    }

    pub async fn context(&self, id: &str) -> Result<ContextInfo, String> {
        self.get_json(&format!("/sessions/{id}/context")).await
    }

    pub async fn history(
        &self,
        id: &str,
        limit: usize,
        offset: usize,
    ) -> Result<HistoryPage, String> {
        self.get_json(&format!(
            "/sessions/{id}/history?limit={limit}&offset={offset}"
        ))
        .await
    }

    pub async fn activities(
        &self,
        id: &str,
        limit: usize,
        offset: usize,
    ) -> Result<ActivitiesPage, String> {
        self.get_json(&format!(
            "/sessions/{id}/activities?limit={limit}&offset={offset}"
        ))
        .await
    }

    pub async fn ongoing_activity(&self, id: &str) -> Result<Option<Activity>, String> {
        let response: OngoingActivity = self
            .get_json(&format!("/sessions/{id}/ongoing_activity"))
            .await?;
        Ok(response.activity)
    }

    pub async fn diff(&self, id: &str) -> Result<GitDiffResult, String> {
        self.get_json(&format!("/sessions/{id}/diff")).await
    }

    pub async fn session_attachments(
        &self,
        id: &str,
        limit: usize,
        offset: usize,
    ) -> Result<AttachmentList, String> {
        self.get_json(&format!(
            "/sessions/{id}/attachments?limit={limit}&offset={offset}"
        ))
        .await
    }

    pub async fn upload_session_attachment(
        &self,
        id: &str,
        filename: &str,
        bytes: Vec<u8>,
    ) -> Result<Attachment, String> {
        self.upload_attachment(&format!("/sessions/{id}/attachments"), filename, bytes)
            .await
    }

    pub fn session_attachment_uri(&self, id: &str, name: &str) -> String {
        format!(
            "{}/sessions/{id}/attachments/{}",
            self.base,
            urlencode(name)
        )
    }

    pub fn workflow_attachment_uri(&self, id: &str, name: &str) -> String {
        format!(
            "{}/workflows/{id}/attachments/{}",
            self.base,
            urlencode(name)
        )
    }

    pub async fn delete_session_attachment(&self, id: &str, name: &str) -> Result<(), String> {
        self.delete(&format!("/sessions/{id}/attachments/{}", urlencode(name)))
            .await
    }

    pub async fn delete_session_attachments(&self, id: &str) -> Result<(), String> {
        self.delete(&format!("/sessions/{id}/attachments")).await
    }

    // ---------- 终端 ----------

    pub async fn open_terminal(
        &self,
        id: &str,
        cwd: Option<String>,
        cols: u16,
        rows: u16,
    ) -> Result<String, String> {
        let response: amux_common::domain::TerminalOpenResult = self
            .post_json(
                &format!("/sessions/{id}/terminals"),
                &OpenTerminalRequest { cwd, cols, rows },
            )
            .await?;
        Ok(response.terminal_id)
    }

    pub async fn terminals(&self, id: &str) -> Result<Vec<Terminal>, String> {
        self.get_json(&format!("/sessions/{id}/terminals")).await
    }

    pub async fn terminal_input(
        &self,
        id: &str,
        terminal: &str,
        data: String,
    ) -> Result<(), String> {
        self.post_empty(
            &format!("/sessions/{id}/terminals/{terminal}"),
            &TerminalInputRequest { data },
        )
        .await
    }

    /// 持续读取终端 SSE；每个 `data` 事件解码为一个输出事件，流断开后返回。
    pub async fn terminal_output_stream(
        &self,
        id: &str,
        terminal: &str,
        mut on_output: impl FnMut(TerminalOutput),
    ) -> Result<(), String> {
        let response = self
            .http
            .get(format!("{}/sessions/{id}/terminals/{terminal}", self.base))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|error| format!("请求失败: {error}"))?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(error_message(status.as_u16(), &body));
        }

        let mut events = SseDecoder::default();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| format!("读取终端流失败: {error}"))?;
            for data in events.push(&chunk) {
                let output = serde_json::from_str(&data)
                    .map_err(|error| format!("终端输出解析失败: {error}"))?;
                on_output(output);
            }
        }
        Ok(())
    }

    pub async fn close_terminal(&self, id: &str, terminal: &str) -> Result<(), String> {
        self.delete(&format!("/sessions/{id}/terminals/{terminal}"))
            .await
    }

    pub async fn resize_terminal(
        &self,
        id: &str,
        terminal: &str,
        cols: u16,
        rows: u16,
    ) -> Result<(), String> {
        self.post_empty(
            &format!("/sessions/{id}/terminals/{terminal}/resize"),
            &TerminalResizeRequest { cols, rows },
        )
        .await
    }

    // ---------- 工作流会话 ----------

    pub async fn create_workflow(
        &self,
        plan: &str,
        title: Option<String>,
        project: Option<String>,
    ) -> Result<Workflow, String> {
        self.post_json(
            "/workflows",
            &CreateWorkflowRequest {
                plan: plan.to_string(),
                title,
                project,
            },
        )
        .await
    }

    pub async fn workflows(
        &self,
        limit: usize,
        offset: usize,
        project: Option<&str>,
    ) -> Result<WorkflowList, String> {
        let mut query = format!("limit={limit}&offset={offset}");
        if let Some(project) = project {
            query.push_str(&format!("&project={}", urlencode(project)));
        }
        self.get_json(&format!("/workflows?{query}")).await
    }

    pub async fn workflow(&self, id: &str) -> Result<Workflow, String> {
        self.get_json(&format!("/workflows/{id}")).await
    }

    pub async fn prompt_workflow(&self, id: &str, input: Vec<ContentBlock>) -> Result<(), String> {
        self.post_empty(&format!("/workflows/{id}"), &PromptRequest { input })
            .await
    }

    pub async fn delete_workflow(&self, id: &str) -> Result<(), String> {
        self.delete(&format!("/workflows/{id}")).await
    }

    pub async fn configure_workflow(
        &self,
        id: &str,
        title: Option<String>,
        project: Option<Option<String>>,
    ) -> Result<(), String> {
        self.post_empty(
            &format!("/workflows/{id}/configure"),
            &ConfigureWorkflowRequest { title, project },
        )
        .await
    }

    pub async fn workflow_history(
        &self,
        id: &str,
        limit: usize,
        offset: usize,
    ) -> Result<HistoryPage, String> {
        self.get_json(&format!(
            "/workflows/{id}/history?limit={limit}&offset={offset}"
        ))
        .await
    }

    pub async fn workflow_activities(
        &self,
        id: &str,
        limit: usize,
        offset: usize,
    ) -> Result<ActivitiesPage, String> {
        self.get_json(&format!(
            "/workflows/{id}/activities?limit={limit}&offset={offset}"
        ))
        .await
    }

    pub async fn workflow_ongoing_activity(&self, id: &str) -> Result<Option<Activity>, String> {
        let response: OngoingActivity = self
            .get_json(&format!("/workflows/{id}/ongoing_activity"))
            .await?;
        Ok(response.activity)
    }

    pub async fn workflow_attachments(
        &self,
        id: &str,
        limit: usize,
        offset: usize,
    ) -> Result<AttachmentList, String> {
        self.get_json(&format!(
            "/workflows/{id}/attachments?limit={limit}&offset={offset}"
        ))
        .await
    }

    pub async fn upload_workflow_attachment(
        &self,
        id: &str,
        filename: &str,
        bytes: Vec<u8>,
    ) -> Result<Attachment, String> {
        self.upload_attachment(&format!("/workflows/{id}/attachments"), filename, bytes)
            .await
    }

    pub async fn delete_workflow_attachment(&self, id: &str, name: &str) -> Result<(), String> {
        self.delete(&format!("/workflows/{id}/attachments/{}", urlencode(name)))
            .await
    }

    pub async fn delete_workflow_attachments(&self, id: &str) -> Result<(), String> {
        self.delete(&format!("/workflows/{id}/attachments")).await
    }

    // ---------- 配置 ----------

    pub async fn skills(&self) -> Result<Vec<Skill>, String> {
        self.get_json(path::CONFIG_SKILLS).await
    }

    pub async fn set_skills(&self, skills: &[Skill]) -> Result<(), String> {
        self.put_empty(path::CONFIG_SKILLS, &skills).await
    }

    pub async fn workflow_plans(&self) -> Result<Vec<WorkflowPlanItem>, String> {
        self.get_json(path::CONFIG_WORKFLOWS).await
    }

    pub async fn set_workflow_plans(&self, plans: &[WorkflowPlanItem]) -> Result<(), String> {
        self.put_empty(path::CONFIG_WORKFLOWS, &plans).await
    }

    pub async fn projects(&self) -> Result<Vec<Project>, String> {
        self.get_json(path::CONFIG_PROJECTS).await
    }

    pub async fn create_project(&self, project: &Project) -> Result<(), String> {
        self.post_empty(path::CONFIG_PROJECTS, project).await
    }

    pub async fn update_project(&self, name: &str, description: &str) -> Result<(), String> {
        self.put_empty(
            &format!("{}{}", path::CONFIG_PROJECTS, urlencode(name)),
            &serde_json::json!({ "description": description }),
        )
        .await
    }

    pub async fn delete_project(&self, name: &str) -> Result<(), String> {
        self.delete(&format!("{}{}", path::CONFIG_PROJECTS, urlencode(name)))
            .await
    }

    pub async fn set_project_order(&self, names: &[String]) -> Result<(), String> {
        self.post_empty(
            &format!("{}order", path::CONFIG_PROJECTS),
            &serde_json::json!({ "names": names }),
        )
        .await
    }

    pub async fn recent_workspaces(&self) -> Result<Vec<RecentWorkspace>, String> {
        self.get_json(path::CONFIG_RECENT_WORKSPACES).await
    }

    pub async fn set_recent_workspaces(
        &self,
        workspaces: &[RecentWorkspace],
    ) -> Result<(), String> {
        self.put_empty(path::CONFIG_RECENT_WORKSPACES, &workspaces)
            .await
    }

    pub async fn quick_commands(&self) -> Result<Vec<QuickCommand>, String> {
        self.get_json(path::CONFIG_QUICK_COMMANDS).await
    }

    pub async fn set_quick_commands(&self, commands: &[QuickCommand]) -> Result<(), String> {
        self.put_empty(path::CONFIG_QUICK_COMMANDS, &commands).await
    }

    pub async fn orchestrator(&self) -> Result<Option<OrchestratorConfig>, String> {
        self.get_json(path::CONFIG_AGENT).await
    }

    pub async fn set_orchestrator(&self, config: &OrchestratorConfig) -> Result<(), String> {
        self.put_empty(path::CONFIG_AGENT, config).await
    }

    // ---------- 内部 ----------

    async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T, String> {
        let response = self
            .http
            .get(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|error| format!("请求失败: {error}"))?;
        decode(response).await
    }

    async fn post_json<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, String> {
        let response = self
            .http
            .post(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await
            .map_err(|error| format!("请求失败: {error}"))?;
        decode(response).await
    }

    async fn post_empty<B: Serialize>(&self, path: &str, body: &B) -> Result<(), String> {
        let response = self
            .http
            .post(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await
            .map_err(|error| format!("请求失败: {error}"))?;
        ensure_success(response).await
    }

    async fn put_empty<B: Serialize>(&self, path: &str, body: &B) -> Result<(), String> {
        let response = self
            .http
            .put(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await
            .map_err(|error| format!("请求失败: {error}"))?;
        ensure_success(response).await
    }

    async fn delete(&self, path: &str) -> Result<(), String> {
        let response = self
            .http
            .delete(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|error| format!("请求失败: {error}"))?;
        ensure_success(response).await
    }

    async fn upload_attachment(
        &self,
        path: &str,
        filename: &str,
        bytes: Vec<u8>,
    ) -> Result<Attachment, String> {
        let response = self
            .http
            .post(format!(
                "{}{path}?filename={}",
                self.base,
                urlencode(filename)
            ))
            .bearer_auth(&self.token)
            .header("content-type", "application/octet-stream")
            .body(bytes)
            .send()
            .await
            .map_err(|error| format!("请求失败: {error}"))?;
        decode(response).await
    }
}

/// 解析 Server 发出的单行 `data:` SSE 事件；响应由本项目控制，故忽略 event/id 字段。
#[derive(Default)]
struct SseDecoder {
    buffer: String,
    data: String,
}

impl SseDecoder {
    fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buffer.push_str(&String::from_utf8_lossy(chunk));
        let mut events = Vec::new();
        while let Some(line_end) = self.buffer.find('\n') {
            let line: String = self.buffer.drain(..=line_end).collect();
            let line = line.trim_end_matches(['\r', '\n']);
            if line.is_empty() {
                if !self.data.is_empty() {
                    events.push(std::mem::take(&mut self.data));
                }
                continue;
            }
            let Some(value) = line.strip_prefix("data:") else {
                continue;
            };
            if !self.data.is_empty() {
                self.data.push('\n');
            }
            self.data.push_str(value.strip_prefix(' ').unwrap_or(value));
        }
        events
    }
}

async fn ensure_success(response: reqwest::Response) -> Result<(), String> {
    if response.status().is_success() {
        return Ok(());
    }
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    Err(error_message(status.as_u16(), &body))
}

async fn decode<T: DeserializeOwned>(response: reqwest::Response) -> Result<T, String> {
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(error_message(status.as_u16(), &body));
    }
    let text = response
        .text()
        .await
        .map_err(|error| format!("读取响应失败: {error}"))?;
    serde_json::from_str(&text).map_err(|error| format!("响应解析失败: {error}"))
}

fn error_message(status: u16, body: &str) -> String {
    let body = body.trim();
    if body.is_empty() {
        format!("HTTP {status}")
    } else {
        format!("HTTP {status}: {body}")
    }
}

/// 极简百分号编码：仅保留 URL 安全字符，其余按 UTF-8 字节转义。
fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        let safe =
            byte.is_ascii_alphanumeric() || matches!(*byte, b'-' | b'_' | b'.' | b'~' | b'/');
        if safe {
            out.push(*byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencode_escapes_spaces_and_keeps_path_separators() {
        assert_eq!(urlencode("/home/me/My Docs"), "/home/me/My%20Docs");
        assert_eq!(urlencode("中文"), "%E4%B8%AD%E6%96%87");
    }

    #[test]
    fn error_message_includes_status_and_body() {
        assert_eq!(error_message(404, "会话不存在\n"), "HTTP 404: 会话不存在");
        assert_eq!(error_message(500, ""), "HTTP 500");
    }

    #[test]
    fn sse_decoder_handles_chunk_boundaries_and_keep_alive() {
        let mut decoder = SseDecoder::default();
        assert!(decoder
            .push(b": keep-alive\n\nevent: output\ndata: {\"data\":\"a")
            .is_empty());
        assert_eq!(
            decoder.push(b"Gk=\"}\n\n"),
            [r#"{"data":"aGk="}"#.to_string()]
        );
    }

    #[tokio::test]
    async fn client_sends_bearer_token_and_decodes_responses() {
        use axum::http::HeaderMap;
        use axum::routing::get;
        use axum::Router;

        let app = Router::new()
            .route(
                "/machines",
                get(|headers: HeaderMap| async move {
                    let authorized = headers
                        .get("authorization")
                        .and_then(|value| value.to_str().ok())
                        .is_some_and(|value| value == "Bearer tk");
                    if !authorized {
                        return (axum::http::StatusCode::UNAUTHORIZED, "unauthorized").into_response();
                    }
                    (
                        axum::http::StatusCode::OK,
                        r#"[{"name":"localpc","os":"linux","arch":"x86_64","hostname":"pc","tempDir":"/tmp","version":"0.1.0"}]"#,
                    )
                        .into_response()
                }),
            )
            .route(
                "/sessions",
                get(|| async { (axum::http::StatusCode::OK, r#"{"sessions":[],"hasMore":false}"#) }),
            );
        use axum::response::IntoResponse;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let connection = Connection {
            server: format!("http://{addr}"),
            token: "tk".into(),
        };
        let client = Client::new(&connection);
        let machines = client.machines().await.unwrap();
        assert_eq!(machines[0].name, "localpc");
        client.ping().await.unwrap();

        let wrong = Client::new(&Connection {
            server: format!("http://{addr}"),
            token: "bad".into(),
        });
        let error = wrong.machines().await.unwrap_err();
        assert!(error.contains("401"), "{error}");
    }
}
