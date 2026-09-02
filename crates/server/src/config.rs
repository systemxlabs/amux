//! server 配置与 token。
//! 认证 token 不落盘：每次启动由用户指定（`--token` / `AMUX_TOKEN`），未指定则拒绝启动。

use std::collections::HashMap;
use std::path::PathBuf;

use clap::Parser;

#[derive(Debug)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub data_dir: PathBuf,
    pub token: String,
    /// ACP agent 可执行（如 codex-acp / `kimi acp`）；缺省时由运行期自动发现
    pub agent_bin: Option<String>,
    /// ACP agent 子命令参数（如 `kimi acp` 的 `["acp"]`）
    pub agent_args: Vec<String>,
}

#[derive(Debug, Parser)]
#[command(name = "amux-server", about = "amux server")]
struct CliArgs {
    /// WebSocket 监听地址
    #[arg(long, value_name = "HOST")]
    host: Option<String>,
    /// WebSocket 监听端口
    #[arg(long, value_parser = parse_port, value_name = "PORT")]
    port: Option<u16>,
    /// WebSocket 认证 token
    #[arg(long, value_name = "TOKEN")]
    token: Option<String>,
    /// Server 数据目录（主要用于测试和演示）
    #[arg(long, value_name = "PATH")]
    data_dir: Option<PathBuf>,
    /// 显式指定 ACP agent，可写为 `kimi acp` 或可执行文件路径
    #[arg(long, value_name = "COMMAND")]
    agent: Option<String>,
}

fn parse_port(value: &str) -> Result<u16, String> {
    value.parse().map_err(|_| format!("不是有效端口: {value}"))
}

/// 使用 clap 解析命令行，再与环境变量合并生成配置。
/// CLI 参数优先于环境变量；token 缺失或为空时返回 Err。
pub fn parse_config(
    env: &HashMap<String, String>,
    args: &[String],
) -> Result<ServerConfig, String> {
    let cli = CliArgs::try_parse_from(
        std::iter::once("amux-server".to_string()).chain(args.iter().cloned()),
    )
    .map_err(|error| error.to_string())?;
    resolve_config(env, cli)
}

fn resolve_config(env: &HashMap<String, String>, cli: CliArgs) -> Result<ServerConfig, String> {
    let get = |key: &str| env.get(key).cloned();
    let host = cli
        .host
        .or_else(|| get("AMUX_HOST"))
        .unwrap_or_else(|| "0.0.0.0".into());
    let port = match cli.port {
        Some(port) => port,
        None => match get("AMUX_PORT") {
            Some(value) => parse_port(&value).map_err(|error| format!("AMUX_PORT {error}"))?,
            None => 34567,
        },
    };
    let data_dir = cli
        .data_dir
        .or_else(|| get("AMUX_DATA_DIR").map(PathBuf::from))
        .unwrap_or_else(|| dirs_data_dir(env).join(".amux").join("server"));

    let (agent_bin, agent_args) = match cli.agent.or_else(|| get("AMUX_AGENT_BIN")) {
        Some(value) => parse_agent(&value)?,
        None => (None, Vec::new()),
    };

    let token = cli
        .token
        .or_else(|| get("AMUX_TOKEN"))
        .filter(|token| !token.trim().is_empty())
        .ok_or_else(|| {
            "未指定认证 token：请用 --token <值> 或环境变量 AMUX_TOKEN 指定后启动（token 不落盘，每次启动需重新指定）".to_string()
        })?;

    Ok(ServerConfig {
        host,
        port,
        data_dir,
        token,
        agent_bin,
        agent_args,
    })
}

fn parse_agent(value: &str) -> Result<(Option<String>, Vec<String>), String> {
    let mut parts = value.split_whitespace();
    let bin = parts
        .next()
        .ok_or_else(|| "--agent 参数不能为空".to_string())?;
    Ok((Some(bin.to_string()), parts.map(str::to_string).collect()))
}

fn dirs_data_dir(env: &HashMap<String, String>) -> PathBuf {
    env.get("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 从真实进程环境加载配置。clap 会负责处理 `--help` 和 `--version`。
pub fn load_config() -> Result<ServerConfig, String> {
    let env = std::env::vars().collect();
    resolve_config(&env, CliArgs::parse())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn token_from_env() {
        let cfg = parse_config(&env_of(&[("AMUX_TOKEN", "secret")]), &[]).unwrap();
        assert_eq!(cfg.token, "secret");
        assert_eq!(cfg.port, 34567);
        assert_eq!(cfg.host, "0.0.0.0");
    }

    #[test]
    fn token_from_arg_overrides_env() {
        let cfg = parse_config(
            &env_of(&[("AMUX_TOKEN", "env-key")]),
            &["--token".into(), "arg-key".into()],
        )
        .unwrap();
        assert_eq!(cfg.token, "arg-key");
    }

    #[test]
    fn missing_token_rejected() {
        let err = parse_config(&env_of(&[]), &[]).unwrap_err();
        assert!(err.contains("token"), "错误信息应提示 token：{err}");
    }

    #[test]
    fn empty_token_rejected() {
        let err = parse_config(&env_of(&[("AMUX_TOKEN", "")]), &[]).unwrap_err();
        assert!(err.contains("token"));
    }

    #[test]
    fn port_and_data_dir() {
        let cfg = parse_config(
            &env_of(&[
                ("AMUX_TOKEN", "t"),
                ("AMUX_PORT", "4000"),
                ("AMUX_DATA_DIR", "/tmp/x"),
            ]),
            &[],
        )
        .unwrap();
        assert_eq!(cfg.port, 4000);
        assert_eq!(cfg.data_dir, PathBuf::from("/tmp/x"));
        let cfg2 = parse_config(
            &env_of(&[("AMUX_TOKEN", "t")]),
            &["--port".into(), "5000".into()],
        )
        .unwrap();
        assert_eq!(cfg2.port, 5000);
    }

    #[test]
    fn clap_parses_agent_command() {
        let cfg = parse_config(
            &env_of(&[("AMUX_TOKEN", "t")]),
            &[
                "--agent".into(),
                "kimi acp".into(),
                "--host".into(),
                "127.0.0.1".into(),
            ],
        )
        .unwrap();
        assert_eq!(cfg.agent_bin.as_deref(), Some("kimi"));
        assert_eq!(cfg.agent_args, ["acp"]);
        assert_eq!(cfg.host, "127.0.0.1");
    }

    #[test]
    fn invalid_arguments_are_rejected_by_clap() {
        let env = env_of(&[("AMUX_TOKEN", "t")]);
        let err = parse_config(&env, &["--port".into(), "not-a-port".into()]).unwrap_err();
        assert!(err.contains("不是有效端口"), "错误信息应提示端口：{err}");
        assert!(parse_config(&env, &["--token".into()]).is_err());
        assert!(parse_config(&env, &["--unknown".into()]).is_err());
    }
}
