#!/usr/bin/env bash
# mihomo-proc: 直接进程模式的 mihomo 生命周期控制（root 运行）。
# 由 /etc/sudoers.d/99-mihomo 授权 %mihomo-admin 组免密调用（无参，一切输入走 stdin）。
#
# stdin 协议：首行命令（apply|start|stop|restart|status），apply 时后续为 config.yaml 全文。
#
# 安全模型：
# - 二进制路径唯一事实源 /etc/mihomo-tui/mihomo.conf（root:root 0600，仅交互式 sudo 可写）。
#   本脚本不接收 TUI 传来的路径；启动时只读该文件并二次校验（绝对路径+字符集+存在+可执行）。
# - 防误杀：kill 前校验 /proc/<pid>/cmdline 首字段 == 配置路径。
# - 防多实例：PID 文件存在且进程存活时 start 拒绝；残留 PID 文件（进程已死）自动清理。
# - 守护化：setsid 启动（新会话，脱离调用方进程组/控制终端），TUI/终端退出不影响服务。
#   非交互脚本（sudo 调用）无 job control：后台子进程与脚本同进程组，setsid 检测到非组长
#   直接 exec（不 fork），$! 即 mihomo PID；下方 start_proc 有 cmdline 防御校验兜底。
# - 测试钩子：MIHOMO_TUI_TEST_CONFIG_DIR/_RUN_DIR/_CONF_FILE/_LOG_FILE 覆盖固定路径。
#   sudo env_reset 清空环境，真实调用（sudo -n）恒用固定路径，钩子不构成注入面。
set -euo pipefail

CONFIG_DIR=${MIHOMO_TUI_TEST_CONFIG_DIR:-/etc/mihomo}
CONFIG="$CONFIG_DIR/config.yaml"
BACKUP="$CONFIG_DIR/config.yaml.bak"
TMP="$CONFIG_DIR/.config.yaml.tmp"
RUN_DIR=${MIHOMO_TUI_TEST_RUN_DIR:-/run/mihomo-tui}
PID_FILE="$RUN_DIR/mihomo.pid"
CONF_FILE=${MIHOMO_TUI_TEST_CONF_FILE:-/etc/mihomo-tui/mihomo.conf}
LOG_FILE=${MIHOMO_TUI_TEST_LOG_FILE:-/var/log/mihomo/mihomo.log}

# 1. 读 stdin 首行命令（白名单匹配，非白即拒）
IFS= read -r cmd || { echo "ERROR: empty stdin" >&2; exit 1; }
case "$cmd" in
  apply|start|stop|restart|status) ;;
  *) echo "ERROR: unknown command: $cmd" >&2; exit 1 ;;
esac

# 2. 读 root 侧配置文件输出二进制路径。allow_missing=1 时 conf 缺失/为空输出空串（status 用）。
read_bin() {
  local allow_missing=$1
  if [ ! -f "$CONF_FILE" ]; then
    if [ "$allow_missing" = 1 ]; then echo ""; return 0; fi
    echo "ERROR: $CONF_FILE 不存在（请先在设置页保存 mihomo 路径）" >&2
    exit 1
  fi
  local line bin
  line=$(cat "$CONF_FILE") || { echo "ERROR: 读取 $CONF_FILE 失败" >&2; exit 1; }
  bin=${line#mihomo_bin=}
  if [ "$bin" = "$line" ]; then
    echo "ERROR: $CONF_FILE 格式错误（期望单行 mihomo_bin=<path>）" >&2
    exit 1
  fi
  if [ "$allow_missing" = 1 ] && [ -z "$bin" ]; then echo ""; return 0; fi
  case "$bin" in
    /*) ;;
    *) echo "ERROR: 非法路径（必须为绝对路径）: $bin" >&2; exit 1 ;;
  esac
  case "$bin" in
    *[!A-Za-z0-9_.+/-]*)
      echo "ERROR: 非法路径（仅允许字母数字与 _ . + - / 字符）: $bin" >&2; exit 1 ;;
  esac
  if [ ! -x "$bin" ]; then
    echo "ERROR: 二进制不存在或不可执行: $bin" >&2
    exit 1
  fi
  echo "$bin"
}

# 进程是否存活且确为配置的 mihomo（cmdline 首字段精确匹配，防误杀）
proc_alive() {
  local pid=$1 bin=$2
  [ -d "/proc/$pid" ] || return 1
  local cmdline
  cmdline=$(tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null) || return 1
  case "$cmdline" in
    "$bin "*) return 0 ;;
    "$bin") return 0 ;;
    *) return 1 ;;
  esac
}

# 读 PID 文件：无文件 → 空；有文件但进程不在/格式坏 → 清理残留并返回空。
read_pid() {
  local bin=$1
  [ -f "$PID_FILE" ] || return 0
  local pid
  pid=$(cat "$PID_FILE" 2>/dev/null) || { rm -f "$PID_FILE"; return 0; }
  case "$pid" in
    ''|*[!0-9]*) rm -f "$PID_FILE"; return 0 ;;
  esac
  if ! proc_alive "$pid" "$bin"; then
    echo "INFO: 清理残留 PID 文件（进程 $pid 已不存在或非 mihomo）" >&2
    rm -f "$PID_FILE"
    return 0
  fi
  echo "$pid"
}

# 停止进程（PID 文件 + cmdline 校验 → SIGTERM → 超时 SIGKILL）。未运行视为成功。
stop_proc() {
  local bin=$1
  local pid
  pid=$(read_pid "$bin")
  if [ -z "$pid" ]; then
    echo "OK: mihomo 未在运行"
    return 0
  fi
  kill -TERM "$pid" 2>/dev/null || true
  for _ in $(seq 1 10); do
    if ! proc_alive "$pid" "$bin"; then
      rm -f "$PID_FILE"
      echo "OK: mihomo 已停止 (PID $pid)"
      return 0
    fi
    sleep 0.5
  done
  echo "WARN: SIGTERM 超时，发送 SIGKILL (PID $pid)" >&2
  kill -KILL "$pid" 2>/dev/null || true
  for _ in $(seq 1 10); do
    if ! proc_alive "$pid" "$bin"; then
      rm -f "$PID_FILE"
      echo "OK: mihomo 已停止 (PID $pid)"
      return 0
    fi
    sleep 0.5
  done
  echo "ERROR: 无法停止进程 (PID $pid)" >&2
  return 1
}

# systemd 服务守卫：服务 active 时先停（防端口冲突；切换模式后的兜底）
ensure_service_stopped() {
  if systemctl is-active --quiet mihomo 2>/dev/null; then
    echo "INFO: systemd 服务 mihomo 运行中，先停止（防端口冲突）" >&2
    systemctl stop mihomo
  fi
}

# 健康检查：进程存活且 cmdline 匹配；失败输出日志尾部
health_check() {
  local bin=$1 pid=$2
  if proc_alive "$pid" "$bin"; then return 0; fi
  echo "ERROR: mihomo 进程未存活，日志尾部:" >&2
  tail -n 5 "$LOG_FILE" >&2 || true
  return 1
}

# 启动进程：setsid 新会话 + 日志重定向 + PID 文件 + 初检。
# 失败统一 return 1（顶层调用由 set -e 退出；apply 分支捕获后执行回滚）。
start_proc() {
  local bin=$1
  if [ ! -d "$RUN_DIR" ]; then
    mkdir -p "$RUN_DIR" || { echo "ERROR: 创建 $RUN_DIR 失败" >&2; return 1; }
    chmod 755 "$RUN_DIR" || return 1
  fi
  mkdir -p "$(dirname "$LOG_FILE")" || { echo "ERROR: 创建日志目录失败" >&2; return 1; }
  touch "$LOG_FILE" || { echo "ERROR: 无法创建日志文件 $LOG_FILE" >&2; return 1; }
  chmod 600 "$LOG_FILE" || return 1
  setsid "$bin" -d "$CONFIG_DIR" </dev/null >>"$LOG_FILE" 2>&1 &
  local pid=$!
  echo "$pid" > "$PID_FILE"
  sleep 0.5
  if ! health_check "$bin" "$pid"; then
    rm -f "$PID_FILE"
    return 1
  fi
  echo "OK: mihomo 已启动 (PID $pid, 配置 $CONFIG)"
}

case "$cmd" in
  apply)
    # 读 stdin 剩余内容（config.yaml）→ 校验 → 备份 → 原子替换 → 停旧 → 启新 → 健康轮询 → 回滚
    rm -f "$TMP"
    cat > "$TMP"
    if ! mihomo -t -f "$TMP"; then
      echo "ERROR: mihomo -t validation failed" >&2
      rm -f "$TMP"
      exit 1
    fi
    bin=$(read_bin 0)
    if [ -f "$CONFIG" ]; then
      cp -a "$CONFIG" "$BACKUP"
    fi
    # 生产（sudo root）下锁属主 root:root；测试钩子非 root 跑时跳过（chown 会 EPERM）
    if [ "$(id -u)" = 0 ]; then
      chown root:root "$TMP"
    fi
    chmod 600 "$TMP"
    mv -f "$TMP" "$CONFIG"
    ensure_service_stopped
    stop_proc "$bin" >/dev/null
    if ! start_proc "$bin" >/dev/null 2>&1; then
      echo "ERROR: mihomo 启动失败，回滚上一份配置" >&2
      tail -n 5 "$LOG_FILE" >&2 || true
      rm -f "$PID_FILE"
      if [ -f "$BACKUP" ]; then
        mv -f "$BACKUP" "$CONFIG"
        start_proc "$bin" >/dev/null 2>&1 || true
      fi
      exit 1
    fi
    pid=$(cat "$PID_FILE")
    for _ in $(seq 1 9); do
      sleep 0.5
      if proc_alive "$pid" "$bin"; then
        echo "OK: config applied, mihomo restarted (PID $pid)"
        exit 0
      fi
    done
    echo "ERROR: mihomo failed to start after apply, rolling back to previous config" >&2
    tail -n 5 "$LOG_FILE" >&2 || true
    # 先 stop_proc 再回滚：read_pid 自行处理残留/缺失 PID 文件；
    # 若先删 PID 文件，运行中的实例将无法定位停止（防双实例）。
    stop_proc "$bin" >/dev/null 2>&1 || true
    if [ -f "$BACKUP" ]; then
      mv -f "$BACKUP" "$CONFIG"
      start_proc "$bin" >/dev/null 2>&1 || true
    fi
    exit 1
    ;;
  start)
    bin=$(read_bin 0)
    pid=$(read_pid "$bin")
    if [ -n "$pid" ]; then
      echo "ERROR: mihomo 已在运行 (PID $pid)" >&2
      exit 1
    fi
    ensure_service_stopped
    start_proc "$bin"
    ;;
  stop)
    bin=$(read_bin 0)
    stop_proc "$bin"
    ;;
  restart)
    bin=$(read_bin 0)
    stop_proc "$bin"
    start_proc "$bin"
    ;;
  status)
    bin=$(read_bin 1) || exit 1
    echo "bin=$bin"
    if [ -n "$bin" ]; then
      pid=$(read_pid "$bin")
      if [ -n "$pid" ]; then
        echo "pid=$pid"
        echo "running=true"
      else
        echo "pid="
        echo "running=false"
      fi
    else
      echo "pid="
      echo "running=false"
    fi
    echo "config=$CONFIG"
    ;;
esac
