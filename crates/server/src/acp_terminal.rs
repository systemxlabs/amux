//! ACP terminal/* 反向请求的客户端实现：agent 经 `terminal/create`、
//! `terminal/output`、`terminal/wait_for_exit`、`terminal/kill`、
//! `terminal/release` 在 server 本机执行命令（kimi acp 的 shell 执行即依赖此能力）。
//!
//! 每个终端由一个 actor 任务独占持有子进程（避免把 Child 锁跨 await），
//! stdout/stderr 由独立读任务合流写入共享缓冲并以原子计数报告读尽；
//! 退出状态在「进程退出且两路管道读尽」后写回，保证 wait_for_exit 返回后
//! 的 output 快照含完整尾部输出。

use std::collections::HashMap;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::v1::{
    CreateTerminalRequest, CreateTerminalResponse, KillTerminalRequest, KillTerminalResponse,
    ReleaseTerminalRequest, ReleaseTerminalResponse, TerminalExitStatus, TerminalId,
    TerminalOutputRequest, TerminalOutputResponse, WaitForTerminalExitRequest,
    WaitForTerminalExitResponse,
};
use tokio::io::AsyncReadExt as _;
use tokio::process::Child;
use tokio::sync::{mpsc, oneshot};

/// 未指定 `output_byte_limit` 时的默认保留上限。
const DEFAULT_OUTPUT_LIMIT: u64 = 2 * 1024 * 1024;

/// 终态前的轮询间隔：进程/管道状态没有额外唤醒源，靠周期性收割。
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// 共享状态：stdout+stderr 合流输出缓冲与退出快照（actor 仅在终态写入退出值）。
struct TerminalCore {
    bytes: Mutex<Vec<u8>>,
    truncated: AtomicBool,
    limit: usize,
    exit_status: Mutex<Option<TerminalExitStatus>>,
}

impl TerminalCore {
    fn push(&self, chunk: &[u8]) {
        let mut bytes = self.bytes.lock().unwrap();
        bytes.extend_from_slice(chunk);
        if bytes.len() > self.limit {
            // 从头部丢弃超限部分并对齐 UTF-8 字符边界（跳过首字节即多字节续字节的
            // 位置），快照保持合法文本
            let mut cut = bytes.len() - self.limit;
            while let Some(&b) = bytes.get(cut) {
                if b & 0xC0 != 0x80 {
                    break;
                }
                cut += 1;
            }
            bytes.drain(..cut);
            self.truncated.store(true, Ordering::SeqCst);
        }
    }

    /// 输出快照 + 是否发生截断 + 已知时的退出状态。
    fn snapshot(&self) -> (String, bool, Option<TerminalExitStatus>) {
        let bytes = self.bytes.lock().unwrap();
        let output = String::from_utf8_lossy(&bytes[..]).into_owned();
        (
            output,
            self.truncated.load(Ordering::SeqCst),
            self.exit_status.lock().unwrap().clone(),
        )
    }

    fn set_exit_status(&self, status: TerminalExitStatus) {
        *self.exit_status.lock().unwrap() = Some(status);
    }
}

/// 注册表条目：core 支撑同步读取输出快照，tx 与 actor 交互（Wait/Kill）。
#[derive(Clone)]
struct TerminalEntry {
    core: Arc<TerminalCore>,
    tx: mpsc::UnboundedSender<Cmd>,
}

enum Cmd {
    Wait(oneshot::Sender<TerminalExitStatus>),
    Kill,
    /// 仅模块内部：actor 的定时轮询唤醒（非真实命令）。
    Tick,
}

/// 一个 driver 连接内的终端集合。终端与会话的关联由 agent 维护，
/// 客户端只按 id 提供生命周期与输出（ACP spec 未要求客户端校验会话归属）。
pub struct TerminalRegistry {
    next_id: AtomicU64,
    map: Mutex<HashMap<String, TerminalEntry>>,
}

pub type SharedTerminals = Arc<TerminalRegistry>;

impl Default for TerminalRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalRegistry {
    pub fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            map: Mutex::new(HashMap::new()),
        }
    }

    /// 拉起子进程并注册 actor。环境继承 server 进程后覆盖 agent 补设的变量；
    /// 命令启动失败返回 Err（agent 视为请求错误）。
    ///
    /// 兼容性：规范形式是 `command` 为可执行文件、参数放 `args`；但 kimi acp 等
    /// 实现会把整条命令行（含引号与参数）塞进 `command` 且 `args` 为空。此处按
    /// 「args 非空 → 直接 exec；args 空且 command 含空白 → 交由 /bin/sh -c 原样
    /// 执行」归一化，两类 agent 均正确。
    pub fn create(&self, req: &CreateTerminalRequest) -> Result<CreateTerminalResponse, String> {
        let limit = req.output_byte_limit.unwrap_or(DEFAULT_OUTPUT_LIMIT) as usize;
        let id = format!("term_{}", self.next_id.fetch_add(1, Ordering::SeqCst));

        if let Some(cwd) = &req.cwd {
            if !cwd.is_dir() {
                return Err(format!("terminal/create 工作目录不存在: {}", cwd.display()));
            }
        }
        let inline_shell = req.args.is_empty() && req.command.split_whitespace().nth(1).is_some();
        let (program, args) = if inline_shell {
            (
                "/bin/sh".to_string(),
                vec!["-c".to_string(), req.command.clone()],
            )
        } else {
            (req.command.clone(), req.args.clone())
        };

        let mut cmd = tokio::process::Command::new(&program);
        cmd.args(&args);
        if let Some(cwd) = &req.cwd {
            cmd.current_dir(cwd);
        }
        for var in &req.env {
            cmd.env(&var.name, &var.value);
        }
        let mut child = cmd
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .stdin(std::process::Stdio::null())
            // runtime 意外关闭时兜底杀掉未回收子进程，避免孤儿残留
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("terminal/create 启动命令失败（{}）: {e}", req.command))?;

        let live_streams = Arc::new(AtomicU32::new(
            u32::from(child.stdout.is_some()) + u32::from(child.stderr.is_some()),
        ));
        let core = Arc::new(TerminalCore {
            bytes: Mutex::new(Vec::new()),
            truncated: AtomicBool::new(false),
            limit,
            exit_status: Mutex::new(None),
        });
        // 管道先于 child 移交 actor 取出，交由读任务合流写入并递减存活计数
        if let Some(stdout) = child.stdout.take() {
            spawn_reader(stdout, core.clone(), live_streams.clone());
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_reader(stderr, core.clone(), live_streams.clone());
        }
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(run_actor(child, rx, core.clone(), live_streams));

        self.map
            .lock()
            .unwrap()
            .insert(id.clone(), TerminalEntry { core, tx });
        Ok(CreateTerminalResponse::new(TerminalId::new(id)))
    }

    /// 输出快照（合流文本、截断标志、已知时的退出状态）。
    pub fn output(&self, req: &TerminalOutputRequest) -> Result<TerminalOutputResponse, String> {
        let entry = self.entry(req.terminal_id.to_string())?;
        let (output, truncated, exit_status) = entry.core.snapshot();
        let mut resp = TerminalOutputResponse::new(output, truncated);
        if let Some(status) = exit_status {
            resp = resp.exit_status(status);
        }
        Ok(resp)
    }

    /// 等待进程退出并回收，返回退出状态（被信号终止时仅携带信号名）。
    pub async fn wait(
        &self,
        req: &WaitForTerminalExitRequest,
    ) -> Result<WaitForTerminalExitResponse, String> {
        let tx = self.entry(req.terminal_id.to_string())?.tx.clone();
        let (ack_tx, ack_rx) = oneshot::channel();
        tx.send(Cmd::Wait(ack_tx))
            .map_err(|_| "terminal 已释放".to_string())?;
        let status = ack_rx
            .await
            .map_err(|_| "terminal actor 已结束".to_string())?;
        Ok(WaitForTerminalExitResponse::new(status))
    }

    /// 强杀进程；实际退出状态由后续 `terminal/output` / `terminal/wait_for_exit` 报告。
    pub fn kill(&self, req: &KillTerminalRequest) -> Result<KillTerminalResponse, String> {
        let tx = self.entry(req.terminal_id.to_string())?.tx.clone();
        let _ = tx.send(Cmd::Kill);
        Ok(KillTerminalResponse::new())
    }

    /// 释放终端：移除句柄（后续按未知 id 拒绝）并杀掉仍在运行的进程，由 actor 回收。
    pub fn release(&self, req: &ReleaseTerminalRequest) -> Result<ReleaseTerminalResponse, String> {
        let tx = self.entry(req.terminal_id.to_string())?.tx.clone();
        self.map
            .lock()
            .unwrap()
            .remove(req.terminal_id.to_string().as_str());
        let _ = tx.send(Cmd::Kill);
        Ok(ReleaseTerminalResponse::new())
    }

    /// 连接终止路径：杀掉全部剩余终端并等待回收（超时兜底交给 kill_on_drop）。
    pub async fn terminate_all(&self) {
        let entries: Vec<TerminalEntry> = {
            let mut map = self.map.lock().unwrap();
            map.drain().map(|(_, e)| e).collect()
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        for entry in entries {
            let (ack_tx, ack_rx) = oneshot::channel();
            let _ = entry.tx.send(Cmd::Kill);
            let _ = entry.tx.send(Cmd::Wait(ack_tx));
            if tokio::time::timeout_at(deadline, ack_rx).await.is_err() {
                log::warn!("部分 terminal 未在时限内回收（kill_on_drop 兜底）");
                return;
            }
        }
    }

    fn entry(&self, id: String) -> Result<TerminalEntry, String> {
        self.map
            .lock()
            .unwrap()
            .get(id.as_str())
            .cloned()
            .ok_or_else(|| format!("未知 terminal id: {id}"))
    }
}

fn spawn_reader<R>(mut stream: R, core: Arc<TerminalCore>, live_streams: Arc<AtomicU32>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buf = [0u8; 8192];
        loop {
            match stream.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => core.push(&buf[..n]),
            }
        }
        live_streams.fetch_sub(1, Ordering::SeqCst);
    });
}

/// actor 主循环：独占持有 Child，处理 Wait/Kill。
/// 终态判定 = 进程退出且全部管道读尽；终态前以定时器兜底轮询。
async fn run_actor(
    mut child: Child,
    mut rx: mpsc::UnboundedReceiver<Cmd>,
    core: Arc<TerminalCore>,
    live_streams: Arc<AtomicU32>,
) {
    let mut pending: Vec<oneshot::Sender<TerminalExitStatus>> = Vec::new();
    let mut status: Option<ExitStatus> = None;
    let mut resolved = false;
    let mut ticker = tokio::time::interval(POLL_INTERVAL);

    loop {
        if !resolved {
            // 读管簿记与收割：tick 与命令都是潜在唤醒源
            let live = live_streams.load(Ordering::SeqCst);
            if status.is_none() {
                status = child.try_wait().ok().flatten();
            }
            if let Some(st) = &status {
                if live == 0 {
                    resolved = true;
                    let acp_st = acp_exit_status(*st);
                    core.set_exit_status(acp_st.clone());
                    for ack in pending.drain(..) {
                        let _ = ack.send(acp_st.clone());
                    }
                }
            }
        }

        let cmd = if resolved {
            rx.recv().await
        } else {
            tokio::select! {
                cmd = rx.recv() => cmd,
                _ = ticker.tick() => Some(Cmd::Tick),
            }
        };

        match cmd {
            Some(Cmd::Wait(ack)) => {
                if resolved {
                    let st = core.exit_status.lock().unwrap().clone();
                    let _ = ack.send(st.expect("终态时快照必已写回"));
                } else {
                    pending.push(ack);
                }
            }
            Some(Cmd::Kill) => {
                let _ = child.start_kill();
            }
            // 定时轮询唤醒，仅推进收割
            Some(Cmd::Tick) => {}
            // rx 关闭：注册表已释放句柄（release / 连接终止），进入回收收尾
            None => break,
        }
    }

    // 收尾：确保进程终止并被回收，防僵尸。
    if status.is_none() {
        let _ = child.start_kill();
    }
    let _ = child.wait().await;
}

/// ExitStatus → ACP 终止状态：正常退出携带退出码；被信号杀死时携带信号名。
fn acp_exit_status(status: ExitStatus) -> TerminalExitStatus {
    let mut es = TerminalExitStatus::new();
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        if let Some(code) = status.code() {
            es = es.exit_code(code as u32);
        } else if let Some(sig) = status.signal() {
            es = es.signal(signal_name(sig));
        }
    }
    #[cfg(windows)]
    if let Some(code) = status.code() {
        es = es.exit_code(code as u32);
    }
    #[cfg(not(any(unix, windows)))]
    compile_error!("amux terminal 宿主未适配该平台");
    es
}

#[cfg(unix)]
fn signal_name(sig: i32) -> String {
    let name = match sig {
        1 => "SIGHUP",
        2 => "SIGINT",
        3 => "SIGQUIT",
        6 => "SIGABRT",
        9 => "SIGKILL",
        11 => "SIGSEGV",
        13 => "SIGPIPE",
        14 => "SIGALRM",
        15 => "SIGTERM",
        _ => "",
    };
    if name.is_empty() {
        format!("SIG{sig}")
    } else {
        name.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{EnvVariable, SessionId};

    fn registry() -> SharedTerminals {
        Arc::new(TerminalRegistry::new())
    }

    fn create_req(command: &str, args: &[&str]) -> CreateTerminalRequest {
        let mut req = CreateTerminalRequest::new(SessionId::new("s1"), command);
        if !args.is_empty() {
            req = req.args(args.iter().map(|s| s.to_string()).collect());
        }
        req
    }

    async fn wait_req(reg: &SharedTerminals, tid: &str) -> TerminalExitStatus {
        reg.wait(&WaitForTerminalExitRequest::new(
            SessionId::new("s1"),
            TerminalId::new(tid),
        ))
        .await
        .expect("wait 应成功")
        .exit_status
    }

    async fn output_of(reg: &SharedTerminals, tid: &str) -> TerminalOutputResponse {
        reg.output(&TerminalOutputRequest::new(
            SessionId::new("s1"),
            TerminalId::new(tid),
        ))
        .expect("output 应成功")
    }

    #[tokio::test]
    async fn echo_flow_exit_and_output() {
        let reg = registry();
        let resp = reg
            .create(&create_req("/bin/sh", &["-c", "echo ok"]))
            .expect("create");
        let tid = resp.terminal_id.to_string();
        let st = wait_req(&reg, tid.as_str()).await;
        assert_eq!(st.exit_code, Some(0));
        let out = output_of(&reg, tid.as_str()).await;
        assert_eq!(out.output.trim(), "ok");
        assert_eq!(
            out.exit_status,
            Some(TerminalExitStatus::new().exit_code(0))
        );
    }

    #[tokio::test]
    async fn merges_stderr() {
        let reg = registry();
        let resp = reg
            .create(&create_req("/bin/sh", &["-c", "echo e >&2"]))
            .expect("create");
        let tid = resp.terminal_id.to_string();
        wait_req(&reg, tid.as_str()).await;
        let out = output_of(&reg, tid.as_str()).await;
        assert_eq!(out.output.trim(), "e");
    }

    #[tokio::test]
    async fn inline_command_line_without_args_executes_via_shell() {
        // kimi acp 形态：整条命令行塞进 command、args 为空
        let reg = registry();
        let resp = reg
            .create(&create_req("/bin/bash -lc 'echo amux-inline-ok'", &[]))
            .expect("create");
        let tid = resp.terminal_id.to_string();
        let st = wait_req(&reg, tid.as_str()).await;
        assert_eq!(st.exit_code, Some(0));
        let out = output_of(&reg, tid.as_str()).await;
        assert_eq!(out.output.trim(), "amux-inline-ok");
    }

    #[tokio::test]
    async fn missing_cwd_reports_clear_error() {
        let reg = registry();
        let req = create_req("/bin/sh", &["-c", "true"])
            .cwd(std::path::PathBuf::from("/nonexistent/amux-cwd"));
        let err = reg.create(&req).expect_err("应失败");
        assert!(err.contains("工作目录不存在"), "{err}");
    }

    #[tokio::test]
    async fn applies_env_vars() {
        let reg = registry();
        let req = create_req("/bin/sh", &["-c", "echo $AMUX_TERM_VAR"])
            .env(vec![EnvVariable::new("AMUX_TERM_VAR", "hello-env")]);
        let resp = reg.create(&req).expect("create");
        let tid = resp.terminal_id.to_string();
        wait_req(&reg, tid.as_str()).await;
        let out = output_of(&reg, tid.as_str()).await;
        assert_eq!(out.output.trim(), "hello-env");
    }

    #[tokio::test]
    async fn truncates_to_byte_limit_at_char_boundary() {
        let reg = registry();
        let req = create_req("/bin/sh", &["-c", "printf 'αααααααααα'"]).output_byte_limit(7u64);
        let resp = reg.create(&req).expect("create");
        let tid = resp.terminal_id.to_string();
        wait_req(&reg, tid.as_str()).await;
        let out = output_of(&reg, tid.as_str()).await;
        assert!(out.truncated);
        assert_eq!(out.output.chars().count(), 3);
    }

    #[tokio::test]
    async fn kill_reports_signal() {
        let reg = registry();
        let resp = reg
            .create(&create_req("/bin/sh", &["-c", "while true; do :; done"]))
            .expect("create");
        let tid = resp.terminal_id.to_string();
        reg.kill(&KillTerminalRequest::new(
            SessionId::new("s1"),
            TerminalId::new(tid.clone()),
        ))
        .expect("kill 应成功");
        let st = wait_req(&reg, tid.as_str()).await;
        assert!(st.exit_code.is_none(), "信号终止不应有退出码: {st:?}");
        assert_eq!(st.signal.as_deref(), Some("SIGKILL"));
    }

    #[tokio::test]
    async fn unknown_terminal_ids_are_errors() {
        let reg = registry();
        assert!(reg
            .output(&TerminalOutputRequest::new(
                SessionId::new("s1"),
                TerminalId::new("term_missing")
            ))
            .is_err());
        assert!(reg
            .wait(&WaitForTerminalExitRequest::new(
                SessionId::new("s1"),
                TerminalId::new("term_missing")
            ))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn release_disposes_running_process() {
        let reg = registry();
        let resp = reg
            .create(&create_req("/bin/sh", &["-c", "while true; do :; done"]))
            .expect("create");
        let tid = resp.terminal_id.to_string();
        reg.release(&ReleaseTerminalRequest::new(
            SessionId::new("s1"),
            TerminalId::new(tid.clone()),
        ))
        .expect("release 应成功");
        // 句柄已移除，后续访问报错
        assert!(reg
            .wait(&WaitForTerminalExitRequest::new(
                SessionId::new("s1"),
                TerminalId::new(tid)
            ))
            .await
            .is_err());
    }
}
