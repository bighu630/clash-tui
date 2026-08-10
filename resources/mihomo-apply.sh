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

# 5. 重启
systemctl restart mihomo

# 6. 健康检查：失败则回滚上一份配置并重启
sleep 1
if ! systemctl is-active --quiet mihomo; then
  echo "ERROR: mihomo failed to start after apply, rolling back to previous config" >&2
  if [ -f "$BACKUP" ]; then
    mv -f "$BACKUP" "$CONFIG"
    systemctl restart mihomo
  fi
  exit 1
fi

echo "OK: config applied, mihomo restarted"
