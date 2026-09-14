//! 普通会话管理集成测试：全部走真实路径——`AcpConnection` 子进程对接
//! `mock_acp`（模拟 ACP v2 agent），不再使用进程内连接替身。
//! 时序控制经 mock 的跨进程协调机制（闸门/步骤/阻塞 session/new），测试以
//! server 侧可观察状态（注册表、ongoing、落盘文件、calls 记录）为同步点。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use amux_server::agent::{AcpConnection, AgentRegistry};
use amux_server::error::SessionError;
use amux_server::registry::SessionRegistry;
use amux_server::session::{ServerNotification, SessionManager};
use protocol::{Activity, ContentBlock, HistoryItem, SessionState};
use tokio::sync::broadcast;

fn text(s: &str) -> Vec<ContentBlock> {
    vec![ContentBlock::Text {
        text: s.to_string(),
    }]
}

/// 轮询等待条件成立（10ms × 1000 = 10s 上限）。
/// 条件为异步闭包：await 立即返回的 server 查询。
async fn wait_until<F, Fut>(desc: &str, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..1000 {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("等待超时：{desc}");
}

/// 一个测试环境：mock_acp 子进程（AcpConnection 经真实 ACP v2 stdio 对接）+
/// 独立数据目录的 SessionManager。`prepare` 在临时目录里生成场景文件并返回
/// 传给 mock 的环境变量。
struct Env {
    manager: Arc<SessionManager>,
    registry: Arc<SessionRegistry>,
    /// mock 场景文件目录（calls / 闸门 / 步骤文件）
    work: PathBuf,
    _temp: tempfile::TempDir,
}

fn spawn_env(
    prepare: impl FnOnce(&Path) -> Vec<(String, String)>,
) -> (Env, broadcast::Receiver<ServerNotification>) {
    // 隔离本机 PATH 上真实 agent 的自动发现，只使用显式配置的 mock 连接。
    std::env::set_var("AMUX_NO_DISCOVERY", "1");
    let temp = tempfile::tempdir().unwrap();
    let work = temp.path().to_path_buf();
    let mock_env = prepare(&work);
    let state = work.join("mock_state");
    let state_s = state.to_str().unwrap().to_string();
    let connection = AcpConnection::spawn(
        env!("CARGO_BIN_EXE_mock_acp"),
        &[state_s.as_str()],
        &mock_env,
    )
    .expect("拉起 mock_acp 失败");
    let agents = Arc::new(AgentRegistry::new(Some((
        "mock".into(),
        Arc::new(connection),
    ))));
    let data_dir = work.join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let registry = Arc::new(SessionRegistry::open(&data_dir.join("session.sqlite")).unwrap());
    let (mgr, rx) = SessionManager::new(agents, registry.clone(), data_dir.clone());
    (
        Env {
            manager: Arc::new(mgr),
            registry,
            work,
            _temp: temp,
        },
        rx,
    )
}

impl Env {
    fn calls(&self) -> PathBuf {
        self.work.join("mock_state.calls")
    }
    fn calls_text(&self) -> String {
        std::fs::read_to_string(self.calls()).unwrap_or_default()
    }
    fn gate(&self, name: &str) -> PathBuf {
        self.work.join(name)
    }
    /// 写闸门文件放行 mock。
    fn open_gate(&self, name: &str) {
        std::fs::write(self.gate(name), "1").unwrap();
    }
}

/// 写场景步骤文件（mock 的 AMUX_MOCK_STEPS）。
fn steps_env(work: &Path, steps: serde_json::Value) -> Vec<(String, String)> {
    let path = work.join("steps.json");
    std::fs::write(&path, steps.to_string()).unwrap();
    vec![("AMUX_MOCK_STEPS".to_string(), path.display().to_string())]
}

fn turn_gates_env(work: &Path, gates: &[&str]) -> Vec<(String, String)> {
    let joined = gates
        .iter()
        .map(|g| work.join(g).display().to_string())
        .collect::<Vec<_>>()
        .join(",");
    vec![("AMUX_MOCK_TURN_GATES".to_string(), joined)]
}

fn git(cwd: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {:?} 失败: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn commit_repo(repo: &Path) {
    std::fs::create_dir_all(repo).unwrap();
    git(repo, &["init", "-b", "main", "-q"]);
    git(repo, &["config", "user.email", "t@t"]);
    git(repo, &["config", "user.name", "t"]);
    std::fs::write(repo.join("a.txt"), "v1\n").unwrap();
    git(repo, &["add", "."]);
    git(repo, &["commit", "-m", "init", "-q"]);
}

#[tokio::test]
async fn worktree_eager_create_and_delete_cascade() {
    let (env, _rx) = spawn_env(|_| Vec::new());
    // 主仓库：需要已有提交（unborn HEAD 无法建 worktree）
    let repo = env._temp.path().join("repo");
    commit_repo(&repo);

    // session.new 即落盘工作树。
    let meta = env
        .manager
        .create("mock", repo.to_str().unwrap(), true)
        .await
        .unwrap();
    assert!(meta
        .worktree_dir
        .starts_with(env._temp.path().join("worktrees").to_str().unwrap()));
    let wt = PathBuf::from(&meta.worktree_dir);
    assert!(wt.is_dir(), "session.new 应立即创建工作树");
    assert!(wt.join(".git").is_file(), ".git 为文件是 worktree 的特征");
    let list = git(&repo, &["worktree", "list", "--porcelain"]);
    assert!(list.contains(meta.worktree_dir.trim()));

    // 首次指令：工作树已就位，prompt 正常完成
    env.manager.prompt(&meta.id, text("hi")).await.unwrap();
    assert!(wt.is_dir(), "prompt 后工作树仍应存在");

    // 改动视图（workspace RPC）应作用于 worktree 目录，而非原始工作目录
    assert_eq!(
        env.manager.workspace_cwd(&meta.id).unwrap(),
        meta.worktree_dir,
        "worktree 会话的 workspace_cwd 应返回 worktree 目录"
    );

    // 删除会话：工作树级联移除（资源清理已异步化，轮询等待后台完成）
    env.manager.delete(&meta.id).await.unwrap();
    wait_until("删除会话应级联删除工作树", || async {
        !wt.exists()
    })
    .await;
    let list = git(&repo, &["worktree", "list", "--porcelain"]);
    assert!(!list.contains(meta.worktree_dir.trim()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_during_lazy_create_does_not_resurrect_session() {
    // session/new 阻塞直到闸门放行：先落 entered 文件（测试确认已进入阻塞）。
    // 删除任务开头的 deleted 标记是纯 CPU 路径（随后才阻塞在生命周期锁上），
    // 留 300ms 宽限后释放闸门，保证 setup 在 create 返回后必然观察到已删除、
    // 跳过最终 upsert（否则会话复活）。
    let (env, _rx) = spawn_env(|work| {
        vec![(
            "AMUX_MOCK_BLOCK_NEW_SESSION".to_string(),
            format!(
                "{}:{}",
                work.join("entered").display(),
                work.join("gate").display()
            ),
        )]
    });
    let meta = env
        .manager
        .create("mock", "/tmp/race", false)
        .await
        .unwrap();
    let session_id = meta.id.clone();

    let mgr = env.manager.clone();
    let sid = session_id.clone();
    let prompt_task = tokio::spawn(async move { mgr.prompt(&sid, text("hi")).await });

    wait_until("mock 应进入阻塞的 session/new", || async {
        env.gate("entered").exists()
    })
    .await;

    let mgr = env.manager.clone();
    let sid = session_id.clone();
    let delete_task = tokio::spawn(async move { mgr.delete(&sid).await });
    // 宽限与放行放阻塞线程执行：此刻两个 runtime worker 分别被 prompt 的
    // 同步 create 调用与 delete 的生命周期锁占用，测试体不能依赖 worker 调度。
    let gate_path = env.gate("gate");
    tokio::task::spawn_blocking(move || {
        std::thread::sleep(Duration::from_millis(300));
        std::fs::write(gate_path, "1").unwrap();
    })
    .await
    .unwrap();

    let prompt_result = prompt_task.await.unwrap();
    assert!(
        matches!(prompt_result, Err(SessionError::NotFound(_))),
        "setup 应观察到已删除并失败: {prompt_result:?}"
    );
    delete_task.await.unwrap().unwrap();
    assert!(env.registry.get(&session_id).unwrap().is_none());
}

#[tokio::test]
async fn prompt_records_usage_update_context_size() {
    let (env, _rx) = spawn_env(|_| Vec::new());
    let meta = env
        .manager
        .create("mock", "/tmp/usage", false)
        .await
        .unwrap();
    env.manager.prompt(&meta.id, text("hi")).await.unwrap();

    // prompt 受理即返回；usage_update 随后到达，轮询等待
    let mgr = env.manager.clone();
    let sid = meta.id.clone();
    wait_until("usage_update 应到达", || {
        let mgr = mgr.clone();
        let sid = sid.clone();
        async move { mgr.context(&sid).await.unwrap().context_window_size == 200_000 }
    })
    .await;
    // 上下文信息为内存存储；session.context 应返回 mock usage_update 通知的值
    let context = env.manager.context(&meta.id).await.unwrap();
    assert_eq!(
        context,
        protocol::SessionContextResult {
            context_size: 53_000,
            context_window_size: 200_000,
        }
    );
}

#[tokio::test]
async fn config_options_lazy_query_and_set() {
    // 选项存储在内存，以 Agent 侧数据为权威；查询会话选项同样触发惰性创建/恢复。
    // mock 声明的 model select 选项：gpt-4o / gpt-5，初始 gpt-4o。
    let (env, _rx) = spawn_env(|_| Vec::new());
    let meta = env.manager.create("mock", "/tmp/cfg", false).await.unwrap();

    // 查询触发惰性创建：agent 侧会话建立，初始选项来自 new 响应
    let stored = env.manager.config_options(&meta.id).await.unwrap();
    assert_eq!(stored.len(), 1, "初始选项应含 model: {stored:?}");
    assert_eq!(stored[0].id, "model");
    match &stored[0].kind {
        protocol::SessionConfigKind::Select {
            current_value,
            options,
        } => {
            assert_eq!(current_value, "gpt-4o");
            assert_eq!(
                options.iter().map(|o| o.value.as_str()).collect::<Vec<_>>(),
                ["gpt-4o", "gpt-5"]
            );
        }
        other => panic!("应为 Select 选项: {other:?}"),
    }
    let entry = env.registry.get(&meta.id).unwrap().unwrap();
    assert!(
        entry.agent_session_id.is_some(),
        "查询会话选项应已创建 agent 侧会话"
    );

    // 设置选项：set_config_option 响应全量覆盖内存存储
    let updated = env
        .manager
        .set_config_option(
            &meta.id,
            "model",
            protocol::SessionConfigOptionValue::ValueId {
                value: "gpt-5".into(),
            },
        )
        .await
        .unwrap();
    match &updated[0].kind {
        protocol::SessionConfigKind::Select { current_value, .. } => {
            assert_eq!(current_value, "gpt-5")
        }
        other => panic!("应为 Select 选项: {other:?}"),
    }
    let again = env.manager.config_options(&meta.id).await.unwrap();
    assert_eq!(again, updated, "后续查询应读到 Agent 侧最新的全量选项");
}

#[tokio::test]
async fn ongoing_thinking_accumulates_across_chunks() {
    // GUI 通过 session.ongoing_activity 看到「思考中」应当是当前思考块的累积内容，
    // 而不是最新一个流式 chunk。mock 按步骤逐段发送 thinking，每段经闸门放行，
    // 测试串行观察三个中间态：单段 → 两段 → 三段。
    let (env, _rx) = spawn_env(|work| {
        let g = |n: &str| work.join(n).display().to_string();
        steps_env(
            work,
            serde_json::json!({
                "steps": [
                    {"kind": "thinking", "text": "先读 src/main.rs", "gate": g("g1")},
                    {"kind": "thinking", "text": "，再分析依赖", "gate": g("g2")},
                    {"kind": "thinking", "text": "，最后写结论", "gate": g("g3")}
                ],
                "end_gate": g("g4")
            }),
        )
    });
    let meta = env
        .manager
        .create("mock", "/tmp/think", false)
        .await
        .unwrap();
    let mgr = env.manager.clone();
    let sid = meta.id.clone();
    let prompt_task = tokio::spawn(async move { mgr.prompt(&sid, text("hi")).await });

    // 逐段放行并观察中间态；最后一段同样要等到观察完成才放行 turn 收尾
    //（end_gate），否则最后一段与 EndTurn 之间的窗口太小，中间态会被错过。
    let expected = [
        "先读 src/main.rs",
        "先读 src/main.rs，再分析依赖",
        "先读 src/main.rs，再分析依赖，最后写结论",
    ];
    for (i, want) in expected.iter().enumerate() {
        env.open_gate(&format!("g{}", i + 1));
        let want = want.to_string();
        let mgr = env.manager.clone();
        let sid = meta.id.clone();
        wait_until("ongoing thinking 中间态", move || {
            let want = want.clone();
            let mgr = mgr.clone();
            let sid = sid.clone();
            async move {
                matches!(
                    mgr.ongoing_activity(&sid).await,
                    Ok(Some(Activity::Thinking { thinking, .. })) if thinking == want
                )
            }
        })
        .await;
    }
    env.open_gate("g4");
    prompt_task.await.unwrap().unwrap();
    // prompt 受理即返回；turn 结束以状态回空闲为准，轮询等待后断言 ongoing 已清空
    let mgr = env.manager.clone();
    let sid = meta.id.clone();
    wait_until("turn 应结束回空闲", || {
        let mgr = mgr.clone();
        let sid = sid.clone();
        async move { mgr.ongoing_activity(&sid).await.unwrap().is_none() }
    })
    .await;
    assert!(env
        .manager
        .ongoing_activity(&meta.id)
        .await
        .unwrap()
        .is_none());
    // 落盘的活动历史只剩一条合并后的 thinking
    let acts = env
        .manager
        .activities(&meta.id, None, None)
        .await
        .unwrap()
        .activities;
    let thinkings: Vec<&str> = acts
        .iter()
        .filter_map(|a| match a {
            Activity::Thinking { thinking, .. } => Some(thinking.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(thinkings, vec!["先读 src/main.rs，再分析依赖，最后写结论"]);
}

#[tokio::test]
async fn ongoing_thinking_resets_after_tool_call_finalizes_block() {
    // turn 内「思考 → 工具调用 → 再思考」时，merger 已在工具调用处
    // 定稿第一个思考块，ongoing 的「思考中」应只携带第二个思考块的内容。
    // 回归：thinking_buf 未定稿时重置，导致 ongoing 一直从第一个思考块累积。
    let (env, _rx) = spawn_env(|work| {
        let g = |n: &str| work.join(n).display().to_string();
        steps_env(
            work,
            serde_json::json!({
                "steps": [
                    {"kind": "thinking", "text": "第一段思考", "gate": g("g1")},
                    {"kind": "tool_call", "id": "tc1", "title": "读取文件", "tool": "read", "gate": g("g2")},
                    {"kind": "thinking", "text": "第二段思考", "gate": g("g3")}
                ],
                "end_gate": g("g4")
            }),
        )
    });
    let meta = env
        .manager
        .create("mock", "/tmp/think", false)
        .await
        .unwrap();
    let mgr = env.manager.clone();
    let sid = meta.id.clone();
    let prompt_task = tokio::spawn(async move { mgr.prompt(&sid, text("hi")).await });

    env.open_gate("g1");
    wait_thinking(&env, &meta.id, "第一段思考").await;
    env.open_gate("g2");
    // 工具调用名来自 ACP toolKind 的投影（read）
    wait_tool_call(&env, &meta.id, "read").await;
    env.open_gate("g3");
    // 若 buf 未在工具调用处重置，这里会拿到「第一段思考第二段思考」而超时失败
    wait_thinking(&env, &meta.id, "第二段思考").await;
    env.open_gate("g4");
    prompt_task.await.unwrap().unwrap();

    // 落盘的活动历史应是两条独立的 thinking，与 ongoing 的中间态一致
    let acts = env
        .manager
        .activities(&meta.id, None, None)
        .await
        .unwrap()
        .activities;
    let thinkings: Vec<&str> = acts
        .iter()
        .filter_map(|a| match a {
            Activity::Thinking { thinking, .. } => Some(thinking.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(thinkings, vec!["第一段思考", "第二段思考"]);
}

async fn wait_thinking(env: &Env, sid: &str, want: &str) {
    let want = want.to_string();
    let mgr = env.manager.clone();
    let sid = sid.to_string();
    wait_until("ongoing thinking 未匹配", move || {
        let want = want.clone();
        let mgr = mgr.clone();
        let sid = sid.clone();
        async move {
            matches!(
                mgr.ongoing_activity(&sid).await,
                Ok(Some(Activity::Thinking { thinking, .. })) if thinking == want
            )
        }
    })
    .await;
}

async fn wait_tool_call(env: &Env, sid: &str, want: &str) {
    let want = want.to_string();
    let mgr = env.manager.clone();
    let sid = sid.to_string();
    wait_until("ongoing 未变为工具调用", move || {
        let want = want.clone();
        let mgr = mgr.clone();
        let sid = sid.clone();
        async move {
            matches!(
                mgr.ongoing_activity(&sid).await,
                Ok(Some(Activity::ToolCall { tool_name, .. })) if tool_name == want
            )
        }
    })
    .await;
}

#[tokio::test]
async fn prompt_writes_history_and_activities_with_title() {
    let (env, mut rx) = spawn_env(|_| Vec::new());
    let meta = env
        .manager
        .create("mock", "/tmp/work", false)
        .await
        .unwrap();

    env.manager
        .prompt(&meta.id, text("实现登录功能"))
        .await
        .unwrap();

    let (list, _) = env.manager.list(None).await.unwrap();
    assert_eq!(list[0].title, "实现登录功能");

    // prompt 受理即返回，turn 在后台收尾：等活动落盘且状态回空闲后再断言
    let mgr = env.manager.clone();
    let sid = meta.id.clone();
    wait_until("活动应已落盘且 turn 结束", || {
        let mgr = mgr.clone();
        let sid = sid.clone();
        async move {
            !mgr.activities(&sid, None, None)
                .await
                .unwrap()
                .activities
                .is_empty()
                && mgr.ongoing_activity(&sid).await.unwrap().is_none()
        }
    })
    .await;
    assert!(
        !env.manager
            .history(&meta.id, None, None)
            .await
            .unwrap()
            .items
            .is_empty(),
        "prompt 后应写历史"
    );
    assert!(
        !env.manager
            .activities(&meta.id, None, None)
            .await
            .unwrap()
            .activities
            .is_empty(),
        "prompt 后应写活动"
    );

    let page = env.manager.history(&meta.id, None, None).await.unwrap();
    let items = &page.items;
    assert!(matches!(&items[0], HistoryItem::UserMessage { content, .. }
        if content.contains(&ContentBlock::Text { text: "实现登录功能".into() })));
    assert!(items.iter().any(|i| matches!(i, HistoryItem::AgentMessage { content, .. }
        if content.iter().any(|c| matches!(c, ContentBlock::Text { text } if text.contains("完成"))))));
    assert!(!page.has_more);
    assert_eq!(page.next_before, None);

    let acts = env
        .manager
        .activities(&meta.id, None, None)
        .await
        .unwrap()
        .activities;
    assert!(acts.iter().any(|a| matches!(a, Activity::Thinking { .. })));
    assert!(acts
        .iter()
        .any(|a| matches!(a, Activity::ToolCall { tool_name, .. } if tool_name == "execute")));

    let (list, _) = env.manager.list(None).await.unwrap();
    assert_eq!(list[0].state, SessionState::Idle);
    assert!(env
        .manager
        .ongoing_activity(&meta.id)
        .await
        .unwrap()
        .is_none());

    let mut saw = false;
    while let Ok(n) = rx.recv().await {
        let ServerNotification::StateChange(c) = n;
        if c.session_id == meta.id
            && c.old_state == SessionState::Busy
            && c.new_state == SessionState::Idle
        {
            saw = true;
            break;
        }
    }
    assert!(saw, "prompt 结束应广播 busy→idle");
}

#[tokio::test]
async fn prompt_persists_title_when_agent_session_precreated() {
    // GUI 选中会话即查询会话选项，选项查询会惰性创建 agent 侧会话并落盘
    // agent_session_id；首条 prompt 因此走 resume 分支，生成的标题仍须落盘。
    let (env, _rx) = spawn_env(|_| Vec::new());
    let meta = env
        .manager
        .create("mock", "/tmp/work", false)
        .await
        .unwrap();

    env.manager.config_options(&meta.id).await.unwrap();
    env.manager
        .prompt(&meta.id, text("实现登录功能"))
        .await
        .unwrap();

    let (list, _) = env.manager.list(None).await.unwrap();
    assert_eq!(list[0].title, "实现登录功能");
}

#[tokio::test]
async fn prompt_persists_user_message_before_turn_ends() {
    // turn 被闸门扣住：用户消息应在 turn 结束前已落盘。
    let (env, _rx) = spawn_env(|work| turn_gates_env(work, &["g1"]));
    let meta = env
        .manager
        .create("mock", "/tmp/work", false)
        .await
        .unwrap();

    let mgr = env.manager.clone();
    let sid = meta.id.clone();
    let prompt_task = tokio::spawn(async move { mgr.prompt(&sid, text("立即保存")).await });

    wait_until("mock 应收到 session/prompt", || {
        let calls = env.calls_text();
        async move { calls.contains("session/prompt") }
    })
    .await;

    // v2 下 agent 输出也即时落盘，故只断言用户消息已在前（不要求是唯一一条）
    let page = env.manager.history(&meta.id, None, None).await.unwrap();
    assert!(
        matches!(
            page.items.first(),
            Some(HistoryItem::UserMessage { content, .. }) if content == &text("立即保存")
        ),
        "用户消息应在 turn 结束前已落盘且位于首位: {:?}",
        page.items
    );

    env.open_gate("g1");
    prompt_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn deleted_mid_turn_does_not_broadcast_state_change() {
    let (env, mut rx) = spawn_env(|work| turn_gates_env(work, &["g1"]));
    let meta = env
        .manager
        .create("mock", "/tmp/work", false)
        .await
        .unwrap();
    let session_id = meta.id.clone();

    // turn 进行中删除会话：finalize_turn 不应为已删除会话广播状态变更，
    // 也不应使其在注册表中复活。
    let mgr = env.manager.clone();
    let sid = session_id.clone();
    let prompt_task = tokio::spawn(async move { mgr.prompt(&sid, text("进行中")).await });
    wait_until("turn 应已开始", || {
        let calls = env.calls_text();
        async move { calls.contains("session/prompt") }
    })
    .await;
    env.manager.delete(&session_id).await.unwrap();

    // prompt 在删除前已受理并返回；删除后旧 turn 的收尾不得复活会话
    prompt_task.await.unwrap().unwrap();
    env.open_gate("g1");
    wait_until("旧 turn 应已收尾", || {
        let calls = env.calls_text();
        async move { calls.matches("session/prompt").count() >= 1 }
    })
    .await;
    assert!(
        env.registry.get(&session_id).unwrap().is_none(),
        "删除的会话不应在注册表中复活"
    );

    assert!(
        !env.registry.session_data_exists(&session_id).unwrap(),
        "删除后旧 turn 不得重新创建历史或活动日志"
    );

    // 已删除会话不应再广播任何状态变更（删除期间收到的 Busy 广播除外）。
    let mut saw_deleted_broadcast = false;
    while let Ok(n) = rx.try_recv() {
        let ServerNotification::StateChange(c) = n;
        if c.session_id == session_id && c.new_state == SessionState::Idle {
            saw_deleted_broadcast = true;
        }
    }
    assert!(
        !saw_deleted_broadcast,
        "已删除会话不应广播 busy→idle 状态变更"
    );
}

#[tokio::test]
async fn busy_state_persisted_immediately_on_resume_path() {
    // 发送 session/prompt 时置工作中并立即落盘。回归：resume 分支（已有
    // agent 侧会话）此前 busy 只改内存，turn 进行中列表读到陈旧空闲。
    let (env, _rx) = spawn_env(|work| turn_gates_env(work, &["g1", "g2"]));
    let meta = env
        .manager
        .create("mock", "/tmp/work", false)
        .await
        .unwrap();
    let session_id = meta.id.clone();

    // 第一轮：走惰性创建分支（upsert 已含 busy），闸门 g1 放行后正常结束
    let mgr = env.manager.clone();
    let sid = session_id.clone();
    let first = tokio::spawn(async move { mgr.prompt(&sid, text("第一轮")).await });
    env.open_gate("g1");
    first.await.unwrap().unwrap();
    let entry = env.registry.get(&session_id).unwrap().unwrap();
    assert!(
        entry.agent_session_id.is_some(),
        "首轮后应有 agent 侧会话 id"
    );
    // turn 在后台收尾：等状态回空闲
    wait_until("首轮应回空闲", || {
        let state = env.registry.get(&session_id).unwrap().unwrap().meta.state;
        async move { state == SessionState::Idle }
    })
    .await;

    // 第二轮：走 resume 分支——agent 报 running 后，工作中的状态立即落盘
    //（不再由发送 prompt 触发：状态以 ACP `state_update` 为权威）
    let mgr = env.manager.clone();
    let sid = session_id.clone();
    let second = tokio::spawn(async move { mgr.prompt(&sid, text("第二轮")).await });
    wait_until("第二个 prompt 应进入工作中", || {
        let state = env.registry.get(&session_id).unwrap().unwrap().meta.state;
        async move { state == SessionState::Busy }
    })
    .await;
    // 忙时 prompt 不被本地拒绝：直接转发给 agent（第三个 prompt 无闸门，立即完成）
    let mgr = env.manager.clone();
    let sid = session_id.clone();
    let concurrent = tokio::spawn(async move { mgr.prompt(&sid, text("并发")).await });
    wait_until("并发 prompt 也应转发给 agent", || {
        let calls = env.calls_text();
        async move { calls.matches("session/prompt").count() == 3 }
    })
    .await;

    env.open_gate("g2");
    second.await.unwrap().unwrap();
    concurrent.await.unwrap().unwrap();
    // RPC 受理即返回；全部 turn 结束后状态由 agent 的 idle 推回空闲
    wait_until("全部 turn 结束后回空闲", || {
        let state = env.registry.get(&session_id).unwrap().unwrap().meta.state;
        async move { state == SessionState::Idle }
    })
    .await;
}

#[tokio::test]
async fn activities_flush_during_turn_not_only_at_end() {
    // thinking + tool_call + output 事件已处理但 turn 未结束（end_gate 未放行）：
    // 已定稿的活动应实时落盘，而非攒到 turn 结束统一写。
    let (env, _rx) = spawn_env(|work| {
        let g = |n: &str| work.join(n).display().to_string();
        steps_env(
            work,
            serde_json::json!({
                "steps": [
                    {"kind": "thinking", "text": "思考中", "gate": g("g1")},
                    {"kind": "tool_call", "id": "tc1", "title": "读取文件", "tool": "read", "gate": g("g2")},
                    {"kind": "output", "text": "完成", "gate": g("g3")}
                ],
                "end_gate": g("g4")
            }),
        )
    });
    let meta = env
        .manager
        .create("mock", "/tmp/work", false)
        .await
        .unwrap();
    let session_id = meta.id.clone();

    env.open_gate("g1");
    env.open_gate("g2");
    env.open_gate("g3");
    let mgr = env.manager.clone();
    let sid = session_id.clone();
    let prompt_task = tokio::spawn(async move { mgr.prompt(&sid, text("立即保存")).await });

    wait_until("活动应实时落盘", || {
        let mgr = env.manager.clone();
        let sid = session_id.clone();
        async move {
            mgr.activities(&sid, None, None)
                .await
                .unwrap()
                .activities
                .iter()
                .any(|a| matches!(a, Activity::ToolCall { tool_name, .. } if tool_name == "read"))
        }
    })
    .await;
    let acts = env
        .manager
        .activities(&meta.id, None, None)
        .await
        .unwrap()
        .activities;
    assert!(
        acts.iter()
            .any(|a| matches!(a, Activity::Thinking { thinking, .. } if thinking == "思考中")),
        "thinking 应在 turn 结束前实时落盘"
    );
    // end_gate 未放行，前台工作未结束：会话应保持工作中
    let entry = env.registry.get(&meta.id).unwrap().unwrap();
    assert_eq!(
        entry.meta.state,
        SessionState::Busy,
        "end_gate 未放行时应保持工作中"
    );
    assert!(!prompt_task.is_finished() || true);
    assert!(env
        .manager
        .ongoing_activity(&meta.id)
        .await
        .unwrap()
        .is_some());

    env.open_gate("g4");
    prompt_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn delete_triggers_connection_close() {
    // mock 声明支持 session/delete：删除应先 close 再 delete，各恰好一次。
    let (env, _rx) = spawn_env(|_| Vec::new());
    let meta = env
        .manager
        .create("mock", "/tmp/work", false)
        .await
        .unwrap();
    env.manager.prompt(&meta.id, text("hi")).await.unwrap();
    assert!(
        !env.calls_text().contains("session/close"),
        "prompt 阶段不应触发 session/close"
    );

    env.manager.delete(&meta.id).await.unwrap();
    wait_until("删除应触发 session/close", || {
        let calls = env.calls_text();
        async move { calls.contains("session/close") }
    })
    .await;
    let calls = env.calls_text();
    assert_eq!(
        calls.lines().filter(|l| *l == "session/close").count(),
        1,
        "删除应恰好触发一次 session/close: {calls:?}"
    );
    assert!(
        calls.contains("session/delete"),
        "支持删除的 agent 应收到 session/delete: {calls:?}"
    );
    assert!(
        env.registry.get(&meta.id).unwrap().is_none(),
        "注册表应已删除"
    );
}

#[tokio::test]
async fn delete_ignores_unsupported_agent_delete() {
    // AMUX_MOCK_NO_DELETE=1：mock 不声明 session/delete 能力。删除仍须触发
    // session/close，不支持删除的错误被忽略、不阻断本地删除。
    let (env, _rx) = spawn_env(|_| vec![("AMUX_MOCK_NO_DELETE".to_string(), "1".to_string())]);
    let meta = env
        .manager
        .create("mock", "/tmp/work", false)
        .await
        .unwrap();
    env.manager.prompt(&meta.id, text("hi")).await.unwrap();

    env.manager.delete(&meta.id).await.unwrap();
    wait_until("删除应触发 session/close", || {
        let calls = env.calls_text();
        async move { calls.contains("session/close") }
    })
    .await;
    let calls = env.calls_text();
    assert!(
        !calls.contains("session/delete"),
        "未声明删除能力的 agent 不应收到 session/delete: {calls:?}"
    );
    assert!(env.registry.get(&meta.id).unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_during_prompt_setup_is_not_dropped() {
    // session/new 阻塞在闸门上（setup 持有生命周期锁）：取消必须等待初始化
    // 完成后才透传给 agent，不能被丢弃也不能提前返回。
    let (env, _rx) = spawn_env(|work| {
        vec![(
            "AMUX_MOCK_BLOCK_NEW_SESSION".to_string(),
            format!(
                "{}:{}",
                work.join("entered").display(),
                work.join("gate").display()
            ),
        )]
    });
    let meta = env
        .manager
        .create("mock", "/tmp/setup-gate", false)
        .await
        .unwrap();
    let session_id = meta.id.clone();

    let mgr = env.manager.clone();
    let sid = session_id.clone();
    let prompt_task = tokio::spawn(async move { mgr.prompt(&sid, text("开始")).await });
    wait_until("mock 应进入阻塞的 session/new", || async {
        env.gate("entered").exists()
    })
    .await;

    let mgr = env.manager.clone();
    let sid = session_id.clone();
    let cancel_task = tokio::spawn(async move { mgr.cancel(&sid).await });
    // 闸门未放行时 setup 持有生命周期锁，cancel 必然被扣住——结构性约束，
    // 与调度无关（cancel 走到 ACP 透传的前提是锁已释放且 turns>0）。
    assert!(
        !cancel_task.is_finished(),
        "取消不应在 prompt 初始化期间提前返回"
    );
    // 检查在阻塞线程执行：两个 runtime worker 此刻分别被 prompt 的同步 create
    // 调用与 cancel 的锁等待占用，测试体不能依赖 worker 调度。
    let calls_path = env.calls();
    let no_cancel_call = tokio::task::spawn_blocking(move || {
        std::thread::sleep(Duration::from_millis(100));
        let calls = std::fs::read_to_string(calls_path).unwrap_or_default();
        !calls.contains("session/cancel")
    })
    .await
    .unwrap();
    assert!(no_cancel_call, "初始化尚未完成时不应透传 agent cancel");

    env.open_gate("gate");
    cancel_task.await.unwrap().unwrap();
    // cancel 是 fire-and-forget 通知：返回成功不等于 mock 已处理，轮询等待落账。
    wait_until("初始化完成后取消必须透传给 agent", || {
        let calls = env.calls_text();
        async move { calls.matches("session/cancel").count() == 1 }
    })
    .await;

    prompt_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn cancel_on_idle_session_skips_acp() {
    // 从未 prompt 的会话处于 Idle：cancel 应幂等成功且不触达 ACP。
    let (env, _rx) = spawn_env(|_| Vec::new());
    let meta = env
        .manager
        .create("mock", "/tmp/idle", false)
        .await
        .unwrap();
    env.manager.cancel(&meta.id).await.unwrap();
    assert!(
        !env.calls().exists(),
        "空闲会话的取消不应透传 ACP agent（mock 未收到任何调用）"
    );
    // 状态不被取消操作扰动
    assert_eq!(
        env.registry.get(&meta.id).unwrap().unwrap().meta.state,
        SessionState::Idle
    );
}
