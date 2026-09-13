#!/usr/bin/env bash
# 一键更新：拉代码 → 编译 release → 启动/重启 → 健康检查
#
#   ./deploy/update.sh              拉代码并更新
#   ./deploy/update.sh --no-pull    只编译并重启（代码已经是最新的）
#   ./deploy/update.sh --restart    不编译，只重启（改了 .env 之后）
#
# 编译失败会直接退出，不会去动正在运行的服务。
set -euo pipefail

cd "$(dirname "$0")/.."
REPO="$(pwd)"
SERVICE="wherebus"
BINARY="$REPO/target/release/wherebus"
PID_FILE="$REPO/wherebus.pid"
LOG_FILE="$REPO/wherebus.log"

do_pull=1
do_build=1
for arg in "$@"; do
    case "$arg" in
        --no-pull) do_pull=0 ;;
        --restart) do_pull=0; do_build=0 ;;
        -h|--help) sed -n '2,9p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "未知参数：$arg（用 --help 查看用法）" >&2; exit 2 ;;
    esac
done

step() { printf '\n\033[1m==> %s\033[0m\n' "$1"; }
fail() { printf '\033[31m%s\033[0m\n' "$1" >&2; exit 1; }

as_root() {
    if [ "$(id -u)" -eq 0 ]; then "$@"; else sudo "$@"; fi
}

# .env 里的监听地址决定健康检查打哪儿；没写就是默认值
read_bind() {
    local value=""
    if [ -f "$REPO/.env" ]; then
        value="$(sed -n 's/^[[:space:]]*\(export[[:space:]]\+\)\?WHEREBUS_BIND[[:space:]]*=[[:space:]]*//p' "$REPO/.env" | tail -n 1)"
        value="${value%%#*}"                       # 去掉行尾注释
        value="$(printf '%s' "$value" | tr -d '"'\''[:space:]')"
    fi
    [ -n "$value" ] || value="127.0.0.1:8080"
    # 监听 0.0.0.0 时本机仍然走 127.0.0.1 探活
    printf '%s' "${value/#0.0.0.0:/127.0.0.1:}"
}

has_service() {
    command -v systemctl >/dev/null 2>&1 && systemctl cat "$SERVICE" >/dev/null 2>&1
}

if [ "$do_pull" -eq 1 ]; then
    step "拉取代码"
    [ -d "$REPO/.git" ] || fail "$REPO 不是 git 仓库"
    if [ -n "$(git status --porcelain)" ]; then
        git status --short
        fail "工作区有未提交的改动，先处理掉再更新（或用 --no-pull 跳过拉取）"
    fi
    git pull --ff-only
    echo "当前版本：$(git log --oneline -1)"
fi

if [ "$do_build" -eq 1 ]; then
    step "编译 release（首次会比较慢）"
    cargo build --release
    [ -x "$BINARY" ] || fail "没找到编译产物：$BINARY"
fi

step "启动服务"
if has_service; then
    as_root systemctl restart "$SERVICE"
    echo "已重启 systemd 服务 $SERVICE"
else
    echo "没有安装 systemd 服务，改为后台运行（装成服务见 README「部署」一节）"
    if [ -f "$PID_FILE" ] && kill -0 "$(cat "$PID_FILE")" 2>/dev/null; then
        old="$(cat "$PID_FILE")"
        echo "停止旧进程 $old"
        kill "$old" 2>/dev/null || true
        for _ in $(seq 1 20); do
            kill -0 "$old" 2>/dev/null || break
            sleep 0.5
        done
        kill -9 "$old" 2>/dev/null || true
    fi
    nohup "$BINARY" >> "$LOG_FILE" 2>&1 &
    echo $! > "$PID_FILE"
    echo "已在后台启动，PID $(cat "$PID_FILE")，日志：$LOG_FILE"
fi

step "健康检查"
bind="$(read_bind)"
url="http://$bind/api/health"
probe() {
    if command -v curl >/dev/null 2>&1; then
        curl -fsS --max-time 2 "$url" >/dev/null 2>&1
    elif command -v wget >/dev/null 2>&1; then
        wget -q -T 2 -O /dev/null "$url" 2>/dev/null
    else
        return 2
    fi
}

for attempt in $(seq 1 20); do
    if probe; then
        echo "服务已就绪：http://$bind"
        echo "管理控制台：http://$bind/admin"
        exit 0
    fi
    status=$?
    if [ "$status" -eq 2 ]; then
        echo "没有 curl 或 wget，跳过健康检查"
        exit 0
    fi
    sleep 1
done

if has_service; then
    fail "服务没有在 20 秒内就绪，看日志：journalctl -u $SERVICE -n 50"
else
    fail "服务没有在 20 秒内就绪，看日志：tail -n 50 $LOG_FILE"
fi
