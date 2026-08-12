#!/usr/bin/env bash
# 打包 mihomo-tui 便携副本：二进制 + 配置文件（含订阅缓存）+ README
# 用法: ./pack.sh [输出文件名]   （默认 mihomo-tui-portable.tar.gz）
set -euo pipefail
cd "$(dirname "$0")"
ROOT="$PWD"

OUT="${1:-mihomo-tui-portable.tar.gz}"
BIN="target/release/mihomo-tui"
CFG="${MIHOMO_TUI_SETTINGS_DIR:-$HOME/.config/mihomo-tui}"

[ -x "$BIN" ] || { echo "错误: 未找到 $BIN，请先 cargo build --release" >&2; exit 1; }
[ -d "$CFG" ] || { echo "错误: 配置目录不存在: $CFG" >&2; exit 1; }
for f in settings.toml subscriptions.toml overrides.toml; do
  [ -f "$CFG/$f" ] || { echo "警告: 缺少 $CFG/$f（会跳过）" >&2; }
done

tar -czf "$OUT" \
  -C "$(dirname "$BIN")" "$(basename "$BIN")" \
  -C "$CFG" settings.toml subscriptions.toml overrides.toml \
  -C "$ROOT" README.md

echo "已生成 $OUT"
echo ""
echo "新机器步骤:"
echo "  1. sudo pacman -S mihomo && sudo mkdir -p /etc/mihomo"
echo "  2. tar -xzf $OUT && ./mihomo-tui   (首启安装引导, 输一次 sudo 密码)"
echo "  3. 重登录终端, Enter 激活订阅即可（缓存已随包携带，无需重新拉取）"
