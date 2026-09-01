//! 终端服务行为测试：PTY 生命周期、输入回显、连接绑定释放。
//! 直接驱动 TerminalService（真实 PTY），输出帧经 ConnScope 的 mpsc 校验。

use std::time::Duration;

use protocol::notify;
use protocol::{TerminalInputParams, TerminalOpenParams};

use amux_server::rpc::RpcError;
use amux_server::terminal::{ConnScope, TerminalService};
use base64::Engine as _;

struct TestConn {
    conn_id: u64,
    rx: tokio::sync::mpsc::Receiver<String>,
    tx: tokio::sync::mpsc::Sender<String>,
}

fn test_conn(conn_id: u64) -> TestConn {
    let (tx, rx) = tokio::sync::mpsc::channel(256);
    TestConn { conn_id, rx, tx }
}

/// 收集帧直到谓词命中（限时兜底；慢机器上 shell 启动可能需要数百 ms）。
async fn frames_until(
    rx: &mut tokio::sync::mpsc::Receiver<String>,
    mut pred: impl FnMut(&str, &serde_json::Value) -> bool,
) -> Vec<(String, serde_json::Value)> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut frames = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            panic!("等待终端帧超时");
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(frame)) => {
                let v: serde_json::Value = serde_json::from_str(&frame).unwrap();
                let method = v["method"].as_str().unwrap_or_default().to_string();
                let params = v["params"].clone();
                let done = pred(&method, &params);
                frames.push((method, params));
                if done {
                    return frames;
                }
            }
            Ok(None) => panic!("终端帧通道提前关闭"),
            Err(_) => panic!("等待终端帧超时"),
        }
    }
}

fn decoded(params: &serde_json::Value) -> String {
    let data = params["data"].as_str().unwrap_or_default();
    String::from_utf8_lossy(
        &base64::engine::general_purpose::STANDARD
            .decode(data)
            .unwrap_or_default(),
    )
    .into_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn open_echo_exit_roundtrip() {
    let service = std::sync::Arc::new(TerminalService::new());
    let conn = test_conn(1);
    let scope = ConnScope {
        conn_id: conn.conn_id,
        frame_tx: conn.tx.clone(),
    };
    let mut rx = conn.rx;
    let tmp = std::env::temp_dir();
    let terminal_id = service
        .open(
            TerminalOpenParams {
                cwd: tmp.to_string_lossy().into(),
                cols: 80,
                rows: 24,
            },
            &scope,
        )
        .expect("打开终端应成功");

    // 输入 `echo <mark>`：回显内容必须出现在输出帧中（真实 PTY + shell 路径）
    let mark = format!("amux-echo-{}", uuid::Uuid::new_v4().simple());
    service
        .input(
            TerminalInputParams {
                terminal_id: terminal_id.clone(),
                data: base64::engine::general_purpose::STANDARD.encode(format!("echo {mark}\n")),
            },
            conn.conn_id,
        )
        .expect("写入输入应成功");

    let frames = frames_until(&mut rx, |m, p| {
        m == notify::TERMINAL_OUTPUT && decoded(p).contains(&mark)
    })
    .await;
    assert!(
        frames
            .iter()
            .any(|(m, p)| m == notify::TERMINAL_OUTPUT && decoded(p).contains(&mark)),
        "输出帧应包含回显的 {mark}"
    );

    // 交互式 shell 输入 exit 退出 → 收到 terminal.exit 通知
    service
        .input(
            TerminalInputParams {
                terminal_id: terminal_id.clone(),
                data: base64::engine::general_purpose::STANDARD.encode("exit\n"),
            },
            conn.conn_id,
        )
        .expect("写入 exit 应成功");
    let frames = frames_until(&mut rx, |m, _| m == notify::TERMINAL_EXIT).await;
    let (_, exit_params) = frames.last().expect("已确认 exit 存在");
    assert_eq!(exit_params["terminalId"], terminal_id.as_str());

    // 退出后条目已摘除：再写入输入应返回 TERMINAL_NOT_FOUND
    let err = service
        .input(
            TerminalInputParams {
                terminal_id,
                data: String::new(),
            },
            conn.conn_id,
        )
        .unwrap_err();
    assert_eq!(err.code, protocol::server_error::TERMINAL_NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn close_and_conn_binding() {
    let service = std::sync::Arc::new(TerminalService::new());
    let conn = test_conn(7);
    let scope = ConnScope {
        conn_id: conn.conn_id,
        frame_tx: conn.tx,
    };
    let _ = conn;

    let tmp = std::env::temp_dir();
    let id = service
        .open(
            TerminalOpenParams {
                cwd: tmp.to_string_lossy().into(),
                cols: 40,
                rows: 10,
            },
            &scope,
        )
        .expect("打开终端应成功");

    // 其他连接不能操控或关闭本连接的终端
    let err = service
        .input(
            TerminalInputParams {
                terminal_id: id.clone(),
                data: String::new(),
            },
            999,
        )
        .unwrap_err();
    assert_eq!(err.code, protocol::server_error::TERMINAL_NOT_FOUND);
    let err = service
        .resize(
            protocol::TerminalResizeParams {
                terminal_id: id.clone(),
                cols: 20,
                rows: 5,
            },
            999,
        )
        .unwrap_err();
    assert_eq!(err.code, protocol::server_error::TERMINAL_NOT_FOUND);
    let err = service
        .close(
            protocol::TerminalIdParams {
                terminal_id: id.clone(),
            },
            999,
        )
        .unwrap_err();
    assert_eq!(err.code, protocol::server_error::TERMINAL_NOT_FOUND);

    // 所属连接显式关闭 → 后续操作报终端不存在
    service
        .close(
            protocol::TerminalIdParams {
                terminal_id: id.clone(),
            },
            7,
        )
        .expect("所属连接关闭应成功");
    let err = service
        .resize(
            protocol::TerminalResizeParams {
                terminal_id: id,
                cols: 20,
                rows: 5,
            },
            42,
        )
        .unwrap_err();
    assert_eq!(err.code, protocol::server_error::TERMINAL_NOT_FOUND);
}

/// 连接断开：release_conn 释放该连接全部终端（杀 PTY → 条目被输出泵摘除）。
#[tokio::test(flavor = "multi_thread")]
async fn release_conn_kills_terminals() {
    let service = std::sync::Arc::new(TerminalService::new());
    let conn = test_conn(42);
    let scope = ConnScope {
        conn_id: conn.conn_id,
        frame_tx: conn.tx,
    };
    let _ = conn;
    let tmp = std::env::temp_dir();
    let id = service
        .open(
            TerminalOpenParams {
                cwd: tmp.to_string_lossy().into(),
                cols: 40,
                rows: 10,
            },
            &scope,
        )
        .expect("打开终端应成功");
    service.release_conn(42);

    // 杀进程是异步生效（输出泵经 EOF 摘除条目），轮询直到不可用
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        assert!(tokio::time::Instant::now() < deadline, "终端未被释放");
        match service.resize(
            protocol::TerminalResizeParams {
                terminal_id: id.clone(),
                cols: 20,
                rows: 5,
            },
            42,
        ) {
            Ok(()) => tokio::time::sleep(Duration::from_millis(50)).await,
            Err(RpcError { code, .. }) => {
                assert_eq!(code, protocol::server_error::TERMINAL_NOT_FOUND);
                break;
            }
        }
    }
}

/// cwd 不存在 / 行列为零 → 参数错误（docs/DESIGN.md：open 指定 cwd 和 size）。
#[tokio::test]
async fn open_rejects_bad_params() {
    let service = std::sync::Arc::new(TerminalService::new());
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let scope = ConnScope {
        conn_id: 1,
        frame_tx: tx,
    };
    let err = service
        .open(
            TerminalOpenParams {
                cwd: "/nonexistent-amux-path".into(),
                cols: 80,
                rows: 24,
            },
            &scope,
        )
        .unwrap_err();
    assert_eq!(err.code, protocol::server_error::INVALID_INPUT);
    let err = service
        .open(
            TerminalOpenParams {
                cwd: std::env::temp_dir().to_string_lossy().into(),
                cols: 0,
                rows: 0,
            },
            &scope,
        )
        .unwrap_err();
    assert_eq!(err.code, protocol::rpc_error::INVALID_PARAMS);
}
