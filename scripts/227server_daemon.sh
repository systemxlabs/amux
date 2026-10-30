#!/bin/sh
# 构建并重启 227server 的 amux-daemon。
# 用法：sh scripts/227server_daemon.sh
set -e

cd "$(dirname "$0")/.."

echo "构建 amux-daemon..."
cargo build -r --bin amux-daemon

echo "停止旧 amux-daemon 进程..."
pkill -f 'amux-daemon --machine 227server' || true

# 等待旧进程退出，避免新进程因 ~/.amux/daemon/227server.lock 持锁失败
for _ in $(seq 1 20); do
  pgrep -f 'amux-daemon --machine 227server' >/dev/null || break
  sleep 0.3
done

echo "启动 amux-daemon..."
nohup ./target/release/amux-daemon \
  --machine 227server \
  --token amuxade \
  --server ws://192.168.99.199:34567/daemon \
  </dev/null >/tmp/amux-daemon.nohup.log 2>&1 &

sleep 1
pgrep -f 'amux-daemon --machine 227server' >/dev/null \
  || { echo "amux-daemon 启动失败，日志：/tmp/amux-daemon.nohup.log" >&2; exit 1; }

echo "amux-daemon 已启动: $(pgrep -f 'amux-daemon --machine 227server' | head -1)"
