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
    /// ACP agent 可执行（如 codex-acp / `kimi acp`）；缺省时由运行期自动发现
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
    let mut port: u16 = match get("AMUX_PORT") {
        Some(value) => value
            .parse()
            .map_err(|_| format!("AMUX_PORT 不是有效端口: {value}"))?,
        None => 34567,
    };
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
                token = Some(
                    args.get(i)
                        .cloned()
                        .ok_or_else(|| "--token 缺少参数".to_string())?,
                );
            }
            "--host" => {
                i += 1;
                host = args
                    .get(i)
                    .cloned()
                    .ok_or_else(|| "--host 缺少参数".to_string())?;
            }
            "--port" => {
                i += 1;
                let value = args.get(i).ok_or_else(|| "--port 缺少参数".to_string())?;
                port = value
                    .parse()
                    .map_err(|_| format!("--port 不是有效端口: {value}"))?;
            }
            "--data-dir" => {
                i += 1;
                data_dir = PathBuf::from(
                    args.get(i)
                        .ok_or_else(|| "--data-dir 缺少参数".to_string())?,
                );
            }
            "--agent" => {
                i += 1;
                let value = args.get(i).ok_or_else(|| "--agent 缺少参数".to_string())?;
                // 支持 `--agent "kimi acp"`（bin 与子命令参数空格分隔），
                // 也支持纯路径（`--agent /path/codex-acp`）
                let mut parts = value.split_whitespace();
                let bin = parts
                    .next()
                    .ok_or_else(|| "--agent 参数不能为空".to_string())?;
                agent_bin = Some(bin.to_string());
                agent_args = parts.map(str::to_string).collect();
            }
            other => return Err(format!("未知参数: {other}")),
        }
        i += 1;
    }

    let token = token.filter(|t| !t.trim().is_empty()).ok_or_else(|| {
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
        assert_eq!(
            cfg.host, "0.0.0.0",
            "默认监听地址应为 0.0.0.0（docs/DESIGN.md）"
        );
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
    fn invalid_arguments_are_rejected() {
        let env = env_of(&[("AMUX_TOKEN", "t")]);
        assert!(parse_config(&env, &["--port".into(), "not-a-port".into()])
            .unwrap_err()
            .contains("有效端口"));
        assert!(parse_config(&env, &["--token".into()]).is_err());
        assert!(parse_config(&env, &["--unknown".into()])
            .unwrap_err()
            .contains("未知参数"));
    }
}
