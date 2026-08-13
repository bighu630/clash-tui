#!/usr/bin/env bash
# mihomo-apply: 从 stdin 接收 config.yaml，校验 → 原子替换 → 重启 → 健康检查（失败自动回滚）
# 由 /etc/sudoers.d/99-mihomo 授权 %mihomo-admin 组免密调用。以 root 运行。
set -euo pipefail

CONFIG_DIR=/etc/mihomo
CONFIG="$CONFIG_DIR/config.yaml"
BACKUP="$CONFIG_DIR/config.yaml.bak"
TMP="$CONFIG_DIR/.config.yaml.tmp"

# 1. 读取 stdin（与目标同文件系统，保证 mv 原子）
rm -f "$TMP"
cat > "$TMP"

# 2. 校验（失败退出码 1，错误输出到 stderr 回传给调用方）
if ! mihomo -t -f "$TMP"; then
  echo "ERROR: mihomo -t validation failed" >&2
  rm -f "$TMP"
  exit 1
fi

# 3. 备份当前配置
if [ -f "$CONFIG" ]; then
  cp -a "$CONFIG" "$BACKUP"
fi

# 4. 原子替换
chown root:root "$TMP"
chmod 600 "$TMP"
mv -f "$TMP" "$CONFIG"

# 4.5 直接进程模式守卫：若有运行中的进程实例（PID 文件 + cmdline 校验），先停止。
# 背景：用户切换到 systemd 模式后进程停止是异步的，紧接应用配置存在竞态窗口；
# PID 文件位于 root 拥有的 /run/mihomo-tui（仅 root 可写，普通用户无法伪造），
# cmdline 含 mihomo 即视为本模式实例，此处为兜底（防端口冲突）。
PROC_RUN_DIR=${MIHOMO_TUI_TEST_RUN_DIR:-/run/mihomo-tui}
PROC_PID_FILE="$PROC_RUN_DIR/mihomo.pid"
if [ -f "$PROC_PID_FILE" ]; then
  pid=$(cat "$PROC_PID_FILE" 2>/dev/null || true)
  case "$pid" in
    ''|*[!0-9]*)
      rm -f "$PROC_PID_FILE"
      ;;
    *)
      if [ -r "/proc/$pid/cmdline" ]; then
        c=$(tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null || true)
        case "$c" in
          *mihomo*)
            echo "INFO: 停止直接进程模式实例 (PID $pid)" >&2
            kill -TERM "$pid" 2>/dev/null || true
            for _ in $(seq 1 10); do
              [ -d "/proc/$pid" ] || break
              sleep 0.5
            done
            if [ -d "/proc/$pid" ]; then
              kill -KILL "$pid" 2>/dev/null || true
            fi
            rm -f "$PROC_PID_FILE"
            ;;
          *) rm -f "$PROC_PID_FILE" ;;
        esac
      else
        rm -f "$PROC_PID_FILE"
      fi
      ;;
  esac
fi

# 5. 重启
systemctl restart mihomo

# 6. 健康检查：轮询最多 10 次（每次 0.5s，共 5s），服务未就绪则回滚上一份配置并重启
for _ in $(seq 1 10); do
  if systemctl is-active --quiet mihomo; then
    echo "OK: config applied, mihomo restarted"
    exit 0
  fi
  sleep 0.5
done

echo "ERROR: mihomo failed to start after apply, rolling back to previous config" >&2
if [ -f "$BACKUP" ]; then
  mv -f "$BACKUP" "$CONFIG"
  systemctl restart mihomo
fi
exit 1
