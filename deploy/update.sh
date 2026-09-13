#!/bin/sh
# 更新并重启：拉代码 → 编译 release → 重启服务
# 用法：./deploy/update.sh   （在仓库根目录执行）
set -eu

cd "$(dirname "$0")/.."

echo "==> 拉取代码"
git pull --ff-only

echo "==> 编译（release）"
cargo build --release

echo "==> 重启服务"
if systemctl is-enabled --quiet wherebus 2>/dev/null; then
    sudo systemctl restart wherebus
    sudo systemctl --no-pager --lines=10 status wherebus
else
    echo "没有安装 wherebus 服务，跳过重启（安装方法见 README「部署」一节）"
fi
