//! server 配置与 token（docs/DESIGN.md §4）。
//! 认证 token 不落盘：每次启动由用户指定（`--token` / `AMUX_TOKEN`），未指定则拒绝启动。

use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub data_dir: PathBuf,
    pub token: String,
    /// ACP agent 可执行（如 codex-acp / `kimi acp`）；缺省时用内存 Stub（演示）
    pub agent_bin: Option<String>,
    /// ACP agent 子命令参数（如 `kimi acp` 的 `["acp"]`）
    pub agent_args: Vec<String>,
}

/// 解析配置（纯函数，env 与 args 可注入便于测试）。
/// token 缺失或为空时返回 Err（启动入口拒绝启动）。
pub fn parse_config(
    env: &HashMap<String, String>,
    args: &[String],
) -> Result<ServerConfig, String> {
    let get = |k: &str| env.get(k).cloned();
    let mut host = get("AMUX_HOST").unwrap_or_else(|| "0.0.0.0".into());
    let mut port: u16 = get("AMUX_PORT")
        .and_then(|p| p.parse().ok())
        .unwrap_or(34567);
    let mut data_dir = get("AMUX_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs_data_dir(env).join(".amux").join("server"));
    let mut agent_bin = get("AMUX_AGENT_BIN");
    let mut agent_args: Vec<String> = Vec::new();

    let mut token = get("AMUX_TOKEN");
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--token" => {
                i += 1;
                token = args.get(i).cloned();
            }
            "--host" => {
                i += 1;
                if let Some(v) = args.get(i) {
                    host = v.clone();
                }
            }
            "--port" => {
                i += 1;
                if let Some(v) = args.get(i).and_then(|v| v.parse().ok()) {
                    port = v;
                }
            }
            "--data-dir" => {
                i += 1;
                if let Some(v) = args.get(i) {
                    data_dir = PathBuf::from(v);
                }
            }
            "--agent" => {
                i += 1;
                if let Some(v) = args.get(i) {
                    // 支持 `--agent "kimi acp"`（bin 与子命令参数空格分隔），
                    // 也支持纯路径（`--agent /path/codex-acp`）
                    let mut parts = v.split_whitespace();
                    if let Some(bin) = parts.next() {
                        agent_bin = Some(bin.to_string());
                        agent_args = parts.map(str::to_string).collect();
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }

    let token = token.filter(|t| !t.is_empty()).ok_or_else(|| {
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

fn dirs_data_dir(env: &HashMap<String, String>) -> PathBuf {
    env.get("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 从真实进程环境加载配置。
pub fn load_config() -> Result<ServerConfig, String> {
    let env = std::env::vars().collect();
    let args: Vec<String> = std::env::args().skip(1).collect();
    parse_config(&env, &args)
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
        assert_eq!(cfg.host, "0.0.0.0", "默认监听地址应为 0.0.0.0（docs/DESIGN.md）");
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
}
