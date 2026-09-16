//! Web 应用的静态资源托管（docs/DESIGN.md「Server 启动」：`--web` 为 web 静态文件目录，
//! 未传则静态资源请求返回 404）。
//!
//! 静态路由不带鉴权：浏览器加载页面与静态资源时无法携带 `Authorization` 头，
//! 而 DESIGN「Client-Server 通信 - 认证」只要求对 Client API 请求做验证，因此鉴权中间件
//! 只挂在 API 路由上（见 `main.rs`），本模块的 fallback 永远在其之外。

use axum::Router;
use tower_http::services::ServeDir;

/// 把静态资源作为 API 路由的 fallback：未命中 API 的请求交给目录服务。
///
/// 未传 `--web` 时不设置 fallback，未匹配请求由 axum 返回 404。
pub fn routes(api: Router, web_dir: Option<&str>) -> Router {
    match web_dir {
        Some(dir) => {
            api.fallback_service(ServeDir::new(dir).append_index_html_on_directories(true))
        }
        None => api,
    }
}
