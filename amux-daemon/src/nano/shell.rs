//! Nano 的 shell 工具：会话内持久 shell。
//!
//! 行为参考 deepseek harness 极简模式（`dsh --profile sdk-minimal`）的持久 bash 工具
//! （`@deepseek-ai/dsh-tool-bash-persistent`）：
//! - 每个会话常驻一个 `sh`，工作目录、环境变量、函数跨调用保留；
//! - 每条命令用一次性随机标记包裹：输出取开始与结束标记之间的部分，退出码在结束标记之后带内返回，
//!   因此判定结束不依赖输出是否空闲；
//! - 超时、调用被取消、shell 退出都重置 shell（下一次调用重建），并在结果里说明；
//! - 输出限制沿用 pi 的 bash 工具：每流保留末尾 2000 行 / 50 KiB，完整输出写入临时文件并在结果里给出路径。

use std::{
    fs::File,
    io::{self, Write},
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::Arc,
    time::Duration,
};

use parking_lot::Mutex;
use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use rig_agent::tool::{Tool, ToolContext, ToolOutput};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStderr, ChildStdin, ChildStdout};
use tokio::time::{Instant, Sleep};

/// 每个流保留的行数上限（与 pi 一致）。
const MAX_LINES: usize = 2000;
/// 每个流保留的字节上限（与 pi 一致）。
const MAX_BYTES: usize = 50 * 1024;
/// 内存里滚动保留的字节数：比字节上限多留一倍，供最终按行/字节裁剪。
const ROLLING_BYTES: usize = MAX_BYTES * 2;
/// shell 退出后等待输出空闲的时间：后代可能仍持有管道继续写。
const EXIT_STDIO_GRACE: Duration = Duration::from_millis(100);
/// 超时上限（秒）。
pub(super) const MAX_TIMEOUT_SECONDS: u64 = 24 * 60 * 60;
/// 包裹标记前缀：后缀随机，避免与命令输出里的同名文本冲突。
const MARKER_PREFIX: &str = "__amux_nano_shell";
/// shell 被重置后的说明。
const RESET_NOTICE: &str = "持久 shell 已重置：下一条命令会以全新环境从工作目录重新开始。";

/// 会话内持久 shell：命令串行执行，进程跨调用保留。
#[derive(Clone)]
pub(super) struct Shell {
    inner: Arc<Inner>,
}

struct Inner {
    cwd: PathBuf,
    /// 会话的 shell 进程；取出期间被取消（future 被 drop）会连进程一起带走，下次调用重建。
    process: Mutex<Option<ShellProcess>>,
}

impl Shell {
    pub(super) fn new(cwd: PathBuf) -> Self {
        Self {
            inner: Arc::new(Inner {
                cwd,
                process: Mutex::new(None),
            }),
        }
    }

    async fn execute(&self, command: &str, timeout: Option<Duration>) -> io::Result<String> {
        let mut process = match self.inner.process.lock().take() {
            Some(process) => process,
            None => ShellProcess::spawn(&self.inner.cwd)?,
        };
        let outcome = process.run(command, timeout).await?;
        if outcome.reusable {
            self.inner.process.lock().replace(process);
        }
        Ok(outcome.text)
    }
}

/// shell 工具的参数。
#[derive(Deserialize)]
pub(super) struct ShellArgs {
    command: String,
    /// 超时秒数（可选，见 [`MAX_TIMEOUT_SECONDS`]）。
    timeout: Option<u64>,
}

impl Tool for Shell {
    const NAME: &'static str = "shell";
    type Args = ShellArgs;
    type Output = ToolOutput;
    type Error = io::Error;

    fn description(&self) -> String {
        "在会话内常驻的 sh 中执行命令：工作目录、环境变量、函数跨调用保留，命令的 stdin 是 /dev/null。\
         结果先给 stdout，再给 [stderr] 区段，末尾在非 0 时给 [exit code: N]。\
         每个输出流最多保留末尾 2000 行或 50 KiB，超出部分丢弃，完整输出写入临时文件并在结果里给出路径。\
         可选 timeout（秒），超时会终止整个进程组并重置 shell。"
            .into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "要执行的 shell 命令" },
                "timeout": {
                    "type": "integer",
                    "description": format!("超时秒数（1-{MAX_TIMEOUT_SECONDS}），缺省不超时")
                }
            },
            "required": ["command"]
        })
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: ShellArgs,
    ) -> Result<ToolOutput, io::Error> {
        let timeout = match args.timeout {
            None => None,
            Some(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "timeout 必须大于 0 秒",
                ))
            }
            Some(seconds) if seconds > MAX_TIMEOUT_SECONDS => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("timeout 最大为 {MAX_TIMEOUT_SECONDS} 秒"),
                ))
            }
            Some(seconds) => Some(Duration::from_secs(seconds)),
        };
        Ok(ToolOutput::text(
            self.execute(&args.command, timeout).await?,
        ))
    }
}

/// 持有 wrapper 的 shell 进程：drop 即杀掉整个进程组（取消、超时、会话结束都走这条路）。
struct ShellProcess {
    child: Box<dyn ChildWrapper>,
    stdin: ChildStdin,
    stdout: ChildStdout,
    stderr: ChildStderr,
}

impl ShellProcess {
    fn spawn(cwd: &Path) -> io::Result<Self> {
        let mut wrapper = CommandWrap::with_new("sh", |line| {
            line.arg("-s")
                .current_dir(cwd)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
        });
        wrapper.wrap(KillOnDrop);
        #[cfg(unix)]
        wrapper.wrap(process_wrap::tokio::ProcessGroup::leader());
        #[cfg(windows)]
        wrapper.wrap(process_wrap::tokio::JobObject);
        let mut child = wrapper.spawn()?;
        let stdin = child
            .stdin()
            .take()
            .ok_or_else(|| io::Error::other("stdin 不可用"))?;
        let stdout = child
            .stdout()
            .take()
            .ok_or_else(|| io::Error::other("stdout 不可用"))?;
        let stderr = child
            .stderr()
            .take()
            .ok_or_else(|| io::Error::other("stderr 不可用"))?;
        Ok(Self {
            child,
            stdin,
            stdout,
            stderr,
        })
    }

    /// 执行一条命令，返回模型可见文本与 shell 是否还能复用。
    async fn run(&mut self, command: &str, timeout: Option<Duration>) -> io::Result<Outcome> {
        let marker = Marker::new();
        self.stdin
            .write_all(wrap(command, &marker).as_bytes())
            .await?;
        self.stdin.flush().await?;

        let mut out = StreamOutput::new(&marker);
        let mut err = StreamOutput::new(&marker);
        let mut out_buf = [0; 8192];
        let mut err_buf = [0; 8192];
        let (mut out_done, mut err_done) = (false, false);
        let mut status: Option<ExitStatus> = None;
        let mut timed_out = false;
        let idle = tokio::time::sleep(EXIT_STDIO_GRACE);
        tokio::pin!(idle);
        // 没配超时时给一个足够远的 deadline，超时分支再靠 guard 关掉。
        let deadline = timeout
            .map(|value| Instant::now() + value)
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(365 * 24 * 60 * 60));

        while !(out_done && err_done) {
            tokio::select! {
                result = self.child.wait(), if status.is_none() => {
                    status = Some(result?);
                    reset_idle(idle.as_mut());
                }
                _ = tokio::time::sleep_until(deadline), if timeout.is_some() && !timed_out => {
                    timed_out = true;
                    // 杀掉整个进程组：直接子进程死亡后，后代持有的管道也会随之关闭。
                    let _ = self.child.start_kill();
                    reset_idle(idle.as_mut());
                }
                result = self.stdout.read(&mut out_buf), if !out_done => {
                    let read = result?;
                    if read == 0 {
                        out_done = true;
                    } else {
                        out_done = out.push(&out_buf[..read]);
                        reset_idle(idle.as_mut());
                    }
                }
                result = self.stderr.read(&mut err_buf), if !err_done => {
                    let read = result?;
                    if read == 0 {
                        err_done = true;
                    } else {
                        err_done = err.push(&err_buf[..read]);
                        reset_idle(idle.as_mut());
                    }
                }
                // shell 已退出（或已超时被杀）但后代仍持有管道时，等输出空闲再收尾。
                _ = &mut idle, if (status.is_some() || timed_out) && !(out_done && err_done) => break,
            }
        }

        out.finish();
        err.finish();
        // 两个流都到 EOF（shell 退出、重定向走掉）时可能还没轮到 wait 分支：补收一次退出码
        if status.is_none() && !timed_out && !(out.complete && err.complete) {
            status = tokio::time::timeout(EXIT_STDIO_GRACE, self.child.wait())
                .await
                .ok()
                .transpose()?;
        }
        // 两个结束标记都到齐才算命令跑完；否则 shell 已不可信，下一次调用重建。
        let reusable = !timed_out && status.is_none() && out.complete && err.complete;
        Ok(Outcome {
            text: render(&out, &err, timed_out.then_some(timeout), status),
            reusable,
        })
    }
}

impl Drop for ShellProcess {
    fn drop(&mut self) {
        // Unix 的 KillOnDrop 只杀直接子进程；取消 future 时必须经 wrapper 杀整个组。
        let _ = self.child.start_kill();
    }
}

/// 一次命令的结果。
struct Outcome {
    text: String,
    /// shell 是否还能继续复用（未超时、未退出、两个结束标记都到齐）。
    reusable: bool,
}

fn reset_idle(mut idle: std::pin::Pin<&mut Sleep>) {
    idle.as_mut().reset(Instant::now() + EXIT_STDIO_GRACE);
}

/// 一次命令的包裹标记。
struct Marker {
    start: String,
    end: String,
}

impl Marker {
    fn new() -> Self {
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        Self {
            start: format!("{MARKER_PREFIX}_start_{nonce}"),
            end: format!("{MARKER_PREFIX}_end_{nonce}"),
        }
    }
}

/// 把命令包进标记里：开始标记写到两个流，结束标记带出退出码；命令的 stdin 是 /dev/null。
///
/// 命令先塞进一个变量再 `eval`，因此命令里的引号、换行、heredoc 都不会破坏包裹本身；
/// 变量与标记都用 POSIX `sh` 的单引号转义（`$'...'` 是 bash 扩展，dash 不支持）。
/// `eval " $var"` 里那个前导空格不能去掉：`eval` 会把以 `-` 开头的第一个参数当成自己的选项。
fn wrap(command: &str, marker: &Marker) -> String {
    let start = quote_for_shell(&marker.start);
    let end = quote_for_shell(&marker.end);
    let command = quote_for_shell(command);
    format!(
        "__amux_command={command}\n\
         printf '%s\\n' {start} 1>&2\n\
         printf '%s\\n' {start}\n\
         eval \" $__amux_command\" </dev/null\n\
         __amux_shell_status=$?\n\
         printf '%s%s\\n' {end} \"$__amux_shell_status\"\n\
         printf '%s%s\\n' {end} \"$__amux_shell_status\" 1>&2\n\
         unset __amux_command __amux_shell_status\n"
    )
}

/// `sh` 单引号字符串：内容里的单引号用 `'\''` 断开再转义。
fn quote_for_shell(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// 组装模型可见的结果：stdout、`[stderr]` 区段、结尾状态标记。
fn render(
    out: &StreamOutput,
    err: &StreamOutput,
    timed_out: Option<Option<Duration>>,
    shell_status: Option<ExitStatus>,
) -> String {
    // `timed_out` 是 `Some(配置的超时)`；缺省超时时为 `Some(None)`。
    let mut body = out.text();
    let stderr = err.text();
    if !stderr.is_empty() {
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str("[stderr]\n");
        body.push_str(&stderr);
    }
    if body.is_empty() {
        body.push_str("(no output)");
    }
    let mut markers: Vec<String> = Vec::new();
    match (timed_out, shell_status) {
        (Some(timeout), _) => {
            let seconds = timeout.map(|value| value.as_secs()).unwrap_or_default();
            markers.push(format!("[timed out after {seconds}s]"));
            markers.push(RESET_NOTICE.to_string());
        }
        (None, Some(status)) => {
            markers.push(shell_exit_marker(&status));
            markers.push(RESET_NOTICE.to_string());
        }
        (None, None) => {
            if let Some(code) = out.exit_code.filter(|code| *code != 0) {
                markers.push(format!("[exit code: {code}]"));
            } else if !(out.complete && err.complete) {
                // 结束标记缺失且 shell 状态未知：本轮输出不可信，重置后重来
                markers.push(RESET_NOTICE.to_string());
            }
        }
    }
    for marker in markers {
        body.push('\n');
        body.push_str(&marker);
    }
    body
}

/// shell 的退出方式：正常退出给退出码，被信号杀掉给信号。
fn shell_exit_marker(status: &ExitStatus) -> String {
    if let Some(code) = status.code() {
        return format!("[shell exited: code {code}]");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return format!("[shell killed by signal: {signal}]");
        }
    }
    "[shell exited]".to_string()
}

/// 单个输出流的收集：丢掉上一次调用残留的输出，只取开始与结束标记之间的内容。
struct StreamOutput {
    start: Vec<u8>,
    end: Vec<u8>,
    /// 开始标记之后、尚未交给 `capture` 的字节（可能含跨读边界的结束标记）。
    pending: Vec<u8>,
    /// 结束标记之后的字节（退出码）。
    status: Vec<u8>,
    capture: Capture,
    started: bool,
    found_end: bool,
    complete: bool,
    exit_code: Option<i32>,
}

impl StreamOutput {
    fn new(marker: &Marker) -> Self {
        // 开始标记连同它自己的换行一起丢掉，剩下的才是本次命令的输出
        let mut start = marker.start.as_bytes().to_vec();
        start.push(b'\n');
        Self {
            start,
            end: marker.end.as_bytes().to_vec(),
            pending: Vec::new(),
            status: Vec::new(),
            capture: Capture::default(),
            started: false,
            found_end: false,
            complete: false,
            exit_code: None,
        }
    }

    /// 追加读到的字节，返回该流是否已完成（结束标记与退出码都到齐）。
    fn push(&mut self, bytes: &[u8]) -> bool {
        if self.complete {
            return true;
        }
        if self.found_end {
            self.status.extend_from_slice(bytes);
            return self.finish_status();
        }
        self.pending.extend_from_slice(bytes);
        if !self.started {
            let Some(index) = find_bytes(&self.pending, &self.start) else {
                // 还没看到开始标记：只保留可能构成标记的尾部
                let keep = self.start.len() - 1;
                if self.pending.len() > keep {
                    let cut = self.pending.len() - keep;
                    self.pending.drain(..cut);
                }
                return false;
            };
            self.pending.drain(..index + self.start.len());
            self.started = true;
        }
        let Some(index) = find_bytes(&self.pending, &self.end) else {
            // 保留可能构成结束标记的尾部，其余交给 capture
            let keep = self.end.len() - 1;
            if self.pending.len() > keep {
                let cut = self.pending.len() - keep;
                let ready: Vec<u8> = self.pending.drain(..cut).collect();
                self.capture.append(&ready);
            }
            return false;
        };
        let ready: Vec<u8> = self.pending.drain(..index).collect();
        self.capture.append(&ready);
        self.pending.drain(..self.end.len());
        self.status = std::mem::take(&mut self.pending);
        self.found_end = true;
        self.finish_status()
    }

    fn finish(&mut self) {
        if self.started && !self.complete {
            // 结束标记没到（shell 退出、被杀）：已读到的部分仍是命令的输出
            let ready = std::mem::take(&mut self.pending);
            self.capture.append(&ready);
        }
        self.capture.finish();
    }

    /// 供模型阅读的文本（超限时只给末尾并附完整输出路径）。
    fn text(&self) -> String {
        self.capture.text()
    }

    /// 解析结束标记之后跟随的退出码（`<状态>\n`）。
    fn finish_status(&mut self) -> bool {
        let Some(newline) = self.status.iter().position(|byte| *byte == b'\n') else {
            return false;
        };
        self.exit_code = std::str::from_utf8(&self.status[..newline])
            .ok()
            .and_then(|text| text.trim().parse().ok());
        self.status.clear();
        self.complete = true;
        true
    }
}

/// 在字节流里查找子串。
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// 单个输出流的收集：内存里只留尾部，超限后把完整输出落到临时文件。
#[derive(Default)]
struct Capture {
    tail: Vec<u8>,
    completed_lines: usize,
    open_line_bytes: usize,
    total_bytes: usize,
    spill: Option<Spill>,
    /// 尚未写入 spill 的原始输出（只在未超限时积累）。
    buffered: Vec<u8>,
    truncated: bool,
}

struct Spill {
    path: PathBuf,
    file: File,
}

impl Capture {
    fn append(&mut self, bytes: &[u8]) {
        for byte in bytes {
            if *byte == b'\n' {
                self.completed_lines += 1;
                self.open_line_bytes = 0;
            } else {
                self.open_line_bytes += 1;
            }
        }
        self.total_bytes += bytes.len();
        self.tail.extend_from_slice(bytes);
        if self.tail.len() > ROLLING_BYTES {
            self.trim_tail();
        }
        if self.exceeds_limits() {
            self.ensure_spill();
        }
        match self.spill.as_mut() {
            Some(spill) => {
                let _ = spill.file.write_all(bytes);
            }
            None => self.buffered.extend_from_slice(bytes),
        }
    }

    fn finish(&mut self) {
        if self.truncated {
            self.ensure_spill();
        }
        if let Some(spill) = self.spill.as_mut() {
            let _ = spill.file.flush();
        }
    }

    /// 供模型阅读的文本：未截断即全部，截断后只给末尾 2000 行 / 50 KiB 并附完整输出路径。
    fn text(&self) -> String {
        if !self.truncated {
            return String::from_utf8_lossy(&self.tail).into_owned();
        }
        let start =
            char_boundary_at_or_after(&self.tail, self.tail.len().saturating_sub(MAX_BYTES));
        let mut tail = &self.tail[start..];
        if start > 0 {
            // 从行边界开始显示：丢掉被字节上限切开的半行。
            if let Some(index) = tail.iter().position(|byte| *byte == b'\n') {
                tail = &tail[index + 1..];
            }
        }
        let text = String::from_utf8_lossy(tail).into_owned();
        let lines: Vec<&str> = text.lines().collect();
        let shown = if lines.len() > MAX_LINES {
            lines[lines.len() - MAX_LINES..].join("\n")
        } else {
            text
        };
        let path = self
            .spill
            .as_ref()
            .map(|spill| spill.path.display().to_string())
            .unwrap_or_default();
        format!(
            "[输出已截断：仅显示末尾 {} 行，共 {} 行（上限 {MAX_LINES} 行或 50 KiB）；完整输出：{path}]\n{}",
            shown.lines().count(),
            self.total_lines(),
            shown
        )
    }

    fn total_lines(&self) -> usize {
        self.completed_lines + usize::from(self.open_line_bytes > 0)
    }

    fn exceeds_limits(&self) -> bool {
        self.total_bytes > MAX_BYTES || self.total_lines() > MAX_LINES
    }

    /// 只保留末尾 ROLLING_BYTES：从 UTF-8 字符边界开始，避免解码出替换符。
    fn trim_tail(&mut self) {
        let start = char_boundary_at_or_after(&self.tail, self.tail.len() - ROLLING_BYTES);
        self.tail.drain(..start);
    }

    fn ensure_spill(&mut self) {
        if self.spill.is_some() {
            return;
        }
        let path = std::env::temp_dir().join(format!("amux-shell-{}.log", uuid::Uuid::new_v4()));
        let Ok(mut file) = File::create(&path) else {
            // 建不出临时文件就只保留内存里的尾部，不再提示路径。
            self.truncated = true;
            self.buffered.clear();
            return;
        };
        let _ = file.write_all(&self.buffered);
        self.buffered.clear();
        self.spill = Some(Spill { path, file });
        self.truncated = true;
    }
}

/// 找到 `index` 之后（含）的第一个 UTF-8 字符边界。
fn char_boundary_at_or_after(bytes: &[u8], index: usize) -> usize {
    let mut index = index.min(bytes.len());
    while index < bytes.len() && bytes[index] & 0xc0 == 0x80 {
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 从结果文本里取出完整输出文件路径。
    fn spill_path(text: &str) -> PathBuf {
        const MARKER: &str = "完整输出：";
        let rest = &text[text.find(MARKER).expect("应给出完整输出路径") + MARKER.len()..];
        PathBuf::from(&rest[..rest.find(']').expect("路径应以 ] 结束")])
    }

    fn shell(cwd: &Path) -> Shell {
        Shell::new(cwd.to_path_buf())
    }

    #[test]
    fn capture_keeps_tail_and_saves_full_output() {
        let mut capture = Capture::default();
        for index in 0..MAX_LINES + 100 {
            capture.append(format!("{index}\n").as_bytes());
        }
        capture.finish();
        let text = capture.text();
        assert!(text.contains("输出已截断"));
        assert!(text.trim_end().ends_with(&(MAX_LINES + 99).to_string()));
        assert!(!text.contains("\n0\n"));

        let path = spill_path(&text);
        let full = std::fs::read_to_string(&path).unwrap();
        assert_eq!(full.lines().count(), MAX_LINES + 100);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn capture_preserves_split_utf8_and_bounds_display() {
        let mut capture = Capture::default();
        let text = "中文输出\n";
        for byte in text.bytes() {
            capture.append(&[byte]);
        }
        assert_eq!(capture.text(), text);

        for _ in 0..MAX_BYTES / 3 + 10 {
            capture.append("中".as_bytes());
        }
        capture.finish();
        let text = capture.text();
        assert!(!text.contains('\u{fffd}'));
        assert!(text.trim_end().ends_with('中'));
        std::fs::remove_file(spill_path(&text)).ok();
    }

    /// 工作目录与环境变量跨调用保留，stderr 单独成段，退出码只在非 0 时给出。
    #[tokio::test]
    async fn shell_keeps_state_and_labels_streams() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let shell = shell(dir.path());

        let output = shell
            .execute(
                "cd sub; export AMUX_NANO_TEST=kept; printf '到子目录\\n'",
                None,
            )
            .await
            .unwrap();
        assert_eq!(output, "到子目录\n");

        let output = shell
            .execute(
                "pwd; printf '%s' \"$AMUX_NANO_TEST\"; printf 'error' >&2",
                None,
            )
            .await
            .unwrap();
        let expected = format!(
            "{}\nkept\n[stderr]\nerror",
            dir.path().join("sub").display()
        );
        assert_eq!(output, expected);

        let output = shell.execute("exit 7", None).await.unwrap();
        assert_eq!(
            output,
            format!("(no output)\n[shell exited: code 7]\n{RESET_NOTICE}")
        );

        // shell 退出后下一次调用重建
        let output = shell.execute("printf 重新开始", None).await.unwrap();
        assert_eq!(output, "重新开始");
    }

    /// 命令的 stdin 是 /dev/null，不会吞掉后续命令。
    #[tokio::test]
    async fn shell_commands_do_not_read_the_command_stream() {
        let dir = tempfile::tempdir().unwrap();
        let shell = shell(dir.path());
        assert_eq!(shell.execute("cat", None).await.unwrap(), "(no output)");
        assert_eq!(shell.execute("printf 后续", None).await.unwrap(), "后续");
    }

    #[tokio::test]
    async fn shell_truncates_and_points_to_full_output() {
        let dir = tempfile::tempdir().unwrap();
        let output = shell(dir.path()).execute("seq 1 5000", None).await.unwrap();
        assert!(output.contains("输出已截断"), "{output}");
        assert!(output.trim_end().ends_with("5000"), "{output}");

        let path = spill_path(&output);
        let full = std::fs::read_to_string(&path).unwrap();
        assert_eq!(full.lines().count(), 5000);
        std::fs::remove_file(path).unwrap();
    }

    /// 命令里的引号、换行、heredoc 与以 `-` 开头的文本都不会破坏包裹协议。
    #[tokio::test]
    async fn shell_wraps_quotes_newlines_and_heredocs() {
        let dir = tempfile::tempdir().unwrap();
        let shell = shell(dir.path());
        assert_eq!(
            shell
                .execute("printf '引号: %s' \"it's\"", None)
                .await
                .unwrap(),
            "引号: it's"
        );
        assert_eq!(
            shell
                .execute("printf '多行\\n第二行\\n'", None)
                .await
                .unwrap(),
            "多行\n第二行\n"
        );
        assert_eq!(
            shell
                .execute("cat <<EOF\n一段文本\nEOF", None)
                .await
                .unwrap(),
            "一段文本\n"
        );
        // 以 - 开头的文本按命令执行，不被当作 eval 的选项
        let output = shell.execute("-n", None).await.unwrap();
        assert!(output.contains("[exit code: 127]"), "{output}");
    }

    /// 标记被拆在多次读取里也能识别，且上一次调用残留的输出不进本次结果。
    #[test]
    fn stream_output_matches_markers_split_across_reads() {
        let marker = Marker::new();
        let mut out = StreamOutput::new(&marker);
        let mut stream = Vec::new();
        stream.extend_from_slice("上一次调用残留\n".as_bytes());
        stream.extend_from_slice(marker.start.as_bytes());
        stream.push(b'\n');
        stream.extend_from_slice("正文".as_bytes());
        stream.extend_from_slice(marker.end.as_bytes());
        stream.extend_from_slice(b"7\n");

        let mut complete = false;
        for byte in stream {
            complete |= out.push(&[byte]);
        }
        out.finish();
        assert!(complete);
        assert_eq!(out.exit_code, Some(7));
        assert_eq!(out.text(), "正文");
    }

    /// shell 在命令中途退出时，已写出的 stdout/stderr 仍在结果里。
    #[tokio::test]
    async fn shell_reports_partial_output_when_the_shell_exits() {
        let dir = tempfile::tempdir().unwrap();
        let shell = shell(dir.path());
        let output = shell
            .execute("printf 输出; printf 错误 >&2; exit 5", None)
            .await
            .unwrap();
        assert!(output.contains("输出"), "{output}");
        assert!(output.contains("[stderr]\n错误"), "{output}");
        assert!(output.contains("[shell exited: code 5]"), "{output}");
        assert!(output.ends_with(RESET_NOTICE), "{output}");

        // 重置后仍可继续用
        assert_eq!(shell.execute("printf 恢复", None).await.unwrap(), "恢复");
    }

    #[tokio::test]
    async fn shell_honours_timeout_and_resets() {
        let dir = tempfile::tempdir().unwrap();
        let shell = shell(dir.path());
        let started = Instant::now();
        let output = shell
            .execute("sleep 30", Some(Duration::from_secs(1)))
            .await
            .unwrap();
        assert!(
            output.starts_with("(no output)\n[timed out after 1s]"),
            "{output}"
        );
        assert!(output.ends_with(RESET_NOTICE), "{output}");
        assert!(started.elapsed() < Duration::from_secs(10));

        // 超时后 shell 已重建，可以继续用
        assert_eq!(shell.execute("printf 继续", None).await.unwrap(), "继续");
    }

    /// 后台进程留下的输出不会串进下一次调用的结果。
    #[tokio::test]
    async fn shell_ignores_output_left_by_background_commands() {
        let dir = tempfile::tempdir().unwrap();
        let shell = shell(dir.path());
        let output = tokio::time::timeout(
            Duration::from_secs(5),
            shell.execute("(sleep 0.3; printf 迟到) & printf 立刻", None),
        )
        .await
        .expect("后台进程持有管道时不应挂住")
        .unwrap();
        assert_eq!(output, "立刻");

        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(
            shell.execute("printf 第二次", None).await.unwrap(),
            "第二次"
        );
    }

    /// 取消调用（future 被 drop）后 shell 重置，下一次调用重新拉起。
    #[tokio::test]
    async fn cancelled_command_resets_the_shell() {
        let dir = tempfile::tempdir().unwrap();
        let shell = shell(dir.path());
        let task = tokio::spawn({
            let shell = shell.clone();
            async move { shell.execute("sleep 30", None).await }
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());

        assert_eq!(shell.execute("printf 恢复", None).await.unwrap(), "恢复");
    }

    /// 工作目录不存在时启动失败，工具按错误返回。
    #[tokio::test]
    async fn shell_spawn_failure_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        assert!(shell(&missing).execute("true", None).await.is_err());
    }
}
