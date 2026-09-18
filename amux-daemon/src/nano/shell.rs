//! Nano 的 shell 工具执行。
//!
//! 行为参考 pi 的 bash 工具（badlogic/pi-mono：`packages/coding-agent/src/core/tools/bash.ts`、
//! `utils/child-process.ts`、`core/tools/output-accumulator.ts`）：
//! - 每个输出流最多保留末尾 2000 行或 50 KiB，超出部分丢弃，并在结果里说明；
//! - 输出被截断时，把该流的完整输出写入临时文件，并把路径放进结果，模型可用 shell 自行查看；
//! - 支持可选超时（秒）：超时杀掉整个进程组，并如实报告；
//! - shell 退出但后代仍持有输出管道时，等输出空闲 100ms 后返回，避免永久挂起；
//! - future 被 drop（用户取消）时经 process-wrap wrapper 杀掉整个进程组，不误杀 Daemon。

use std::{
    fs::File,
    io::{self, Write},
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    time::Duration,
};

use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use tokio::io::AsyncReadExt;
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

/// 执行命令，返回 `exit: <状态>\n<stdout>\n<stderr>` 形式的工具输出文本。
pub(super) async fn execute(
    cwd: &Path,
    command: &str,
    timeout: Option<Duration>,
) -> io::Result<String> {
    let mut process = ShellProcess::spawn(cwd, command)?;
    let mut stdout = process
        .child
        .stdout()
        .take()
        .ok_or_else(|| io::Error::other("stdout 不可用"))?;
    let mut stderr = process
        .child
        .stderr()
        .take()
        .ok_or_else(|| io::Error::other("stderr 不可用"))?;
    let mut out = Capture::default();
    let mut err = Capture::default();
    let mut out_buf = [0; 8192];
    let mut err_buf = [0; 8192];
    let (mut out_eof, mut err_eof) = (false, false);
    let mut status: Option<ExitStatus> = None;
    let mut timed_out = false;
    let idle = tokio::time::sleep(EXIT_STDIO_GRACE);
    tokio::pin!(idle);
    // 没配超时时给一个足够远的 deadline，超时分支再靠 guard 关掉。
    let deadline = timeout
        .map(|value| Instant::now() + value)
        .unwrap_or_else(|| Instant::now() + Duration::from_secs(365 * 24 * 60 * 60));

    loop {
        if status.is_some() && out_eof && err_eof {
            break;
        }
        tokio::select! {
            result = process.child.wait(), if status.is_none() => {
                status = Some(result?);
                reset_idle(idle.as_mut());
            }
            _ = tokio::time::sleep_until(deadline), if timeout.is_some() && !timed_out => {
                timed_out = true;
                // 杀掉整个进程组：直接子进程死亡后，后代持有的管道也会随之关闭。
                let _ = process.child.start_kill();
                reset_idle(idle.as_mut());
            }
            result = stdout.read(&mut out_buf), if !out_eof => {
                let read = result?;
                out_eof = read == 0;
                out.append(&out_buf[..read]);
                if read > 0 {
                    reset_idle(idle.as_mut());
                }
            }
            result = stderr.read(&mut err_buf), if !err_eof => {
                let read = result?;
                err_eof = read == 0;
                err.append(&err_buf[..read]);
                if read > 0 {
                    reset_idle(idle.as_mut());
                }
            }
            // shell 已退出（或已超时被杀）但后代仍持有管道时，等输出空闲再收尾。
            _ = &mut idle, if status.is_some() || timed_out => break,
        }
    }
    process.completed = true;
    out.finish();
    err.finish();

    let status_text = match (timed_out, status) {
        (true, _) => format!(
            "timeout: 超过 {} 秒已终止进程组",
            timeout.map(|value| value.as_secs()).unwrap_or_default()
        ),
        (false, Some(status)) => format!("exit: {status}"),
        (false, None) => "exit: 进程未正常结束".to_string(),
    };
    Ok(format!("{status_text}\n{}\n{}", out.text(), err.text()))
}

fn reset_idle(mut idle: std::pin::Pin<&mut Sleep>) {
    idle.as_mut().reset(Instant::now() + EXIT_STDIO_GRACE);
}

/// 持有 wrapper 的 shell 进程：未正常结束时 drop 即杀掉整个进程组。
struct ShellProcess {
    child: Box<dyn ChildWrapper>,
    completed: bool,
}

impl ShellProcess {
    fn spawn(cwd: &Path, command: &str) -> io::Result<Self> {
        let mut wrapper = CommandWrap::with_new("sh", |line| {
            line.arg("-c")
                .arg(command)
                .current_dir(cwd)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
        });
        wrapper.wrap(KillOnDrop);
        #[cfg(unix)]
        wrapper.wrap(process_wrap::tokio::ProcessGroup::leader());
        #[cfg(windows)]
        wrapper.wrap(process_wrap::tokio::JobObject);
        Ok(Self {
            child: wrapper.spawn()?,
            completed: false,
        })
    }
}

impl Drop for ShellProcess {
    fn drop(&mut self) {
        // Unix 的 KillOnDrop 只杀直接子进程；取消 future 时必须经 wrapper 杀整个组。
        if !self.completed {
            let _ = self.child.start_kill();
        }
    }
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

    #[tokio::test]
    async fn execute_reports_cwd_stdin_exit_and_both_streams() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("input"), "from cwd").unwrap();
        let output = execute(dir.path(), "cat input; cat; printf error >&2; exit 7", None)
            .await
            .unwrap();
        assert_eq!(output, "exit: exit status: 7\nfrom cwd\nerror");
        assert!(execute(&dir.path().join("missing"), "true", None)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn execute_truncates_and_points_to_full_output() {
        let output = execute(Path::new("/tmp"), "seq 1 5000", None)
            .await
            .unwrap();
        assert!(output.contains("输出已截断"), "{output}");
        assert!(output.trim_end().ends_with("5000"), "{output}");

        let path = spill_path(&output);
        let full = std::fs::read_to_string(&path).unwrap();
        assert_eq!(full.lines().count(), 5000);
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn execute_honours_timeout() {
        let started = Instant::now();
        let output = execute(Path::new("/tmp"), "sleep 30", Some(Duration::from_secs(1)))
            .await
            .unwrap();
        assert!(
            output.starts_with("timeout: 超过 1 秒已终止进程组"),
            "{output}"
        );
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn execute_does_not_wait_forever_for_inherited_pipe() {
        let output = tokio::time::timeout(
            Duration::from_secs(5),
            execute(Path::new("/tmp"), "sleep 60 & printf done", None),
        )
        .await
        .expect("空闲的继承管道阻塞了返回")
        .unwrap();
        assert_eq!(output, "exit: exit status: 0\ndone\n");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_shell_kills_descendants_holding_pipes() {
        let mut process =
            ShellProcess::spawn(Path::new("/tmp"), "sleep 60 & printf ready; wait").unwrap();
        let mut stdout = process.child.stdout().take().unwrap();
        let mut ready = [0; 5];
        stdout.read_exact(&mut ready).await.unwrap();
        assert_eq!(&ready, b"ready");
        drop(process);

        let mut rest = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), stdout.read_to_end(&mut rest))
            .await
            .expect("后代仍持有输出管道")
            .unwrap();
    }
}
