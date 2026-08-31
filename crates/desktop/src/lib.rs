//! amux 桌面应用 crate。二进制入口在 `main.rs`；模块导出供
//! 集成测试（tests/，GPUI 真实布局渲染回归）复用实现。

#![recursion_limit = "512"]

pub mod aggregate;
pub mod app;
pub mod config;
pub mod diff;
pub mod diff_review;
pub mod display;
pub mod logic;
pub mod machine;
pub mod machines;
pub mod panels;
pub mod sessions;
pub mod settings;
pub mod terminal;
pub mod text;
pub mod theme;
pub mod wfstore;
pub mod workflow;
pub mod workflow_view;
pub mod ws;

#[cfg(test)]
mod terminal_tab_test;
