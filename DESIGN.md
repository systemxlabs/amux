# amux — 系统设计

版本: 0.1
日期: 2026-07-21

---

## Client-Server 架构

采用 Client-Server 架构，Client 与 Server 之间通过 WebSocket 通信，参考 [raft.build](https://raft.build) 的设计模式。

## Skills 管理

Skills 管理仅存储 skills 仓库位置（URL + 本地目录），不存储 skills 具体内容。Skills 的安装和更新由 agent 执行。

## 数据存储

几乎所有数据都存储在 Server 端，Client 仅作展示。

## Server 与 Agent Harness 通信

Server 与 Agent Harness 之间通过 AHAL 层通信。AHAL 提供统一的 Driver/Session 接口，屏蔽不同 harness 的差异。

## 工作流

工作流实现方案待定。
