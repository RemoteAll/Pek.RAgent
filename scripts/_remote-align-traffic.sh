#!/bin/bash
# 一次性对账：停 StarAgent -> 读出与宝塔 site_total 的差额 -> 补进我们的流量状态 -> 重启 -> 验证
# 用法: bash _remote-align-traffic.sh <站点名> <日期 YYYY-MM-DD>
# 安全阀：任何异常退出都会自动把 StarAgent 拉回来（trap EXIT）。
set -e
SITE=${1:?用法: $0 <站点名> <日期 YYYY-MM-DD>}
DATE=${2:?用法: $0 <站点名> <日期 YYYY-MM-DD>}
PY=/www/server/panel/pyenv/bin/python3
STATE=/www/Agent/Data/web_traffic.json
BT=/www/server/site_total/data/total/$SITE/$DATE.json

echo '[1] 停止 StarAgent（停止时会把内存计数 flush 到状态文件）'
systemctl stop StarAgent
sleep 1
systemctl is-active StarAgent || true

# 从现在起，无论脚本怎么退出，都保证服务被拉起
trap 'systemctl start StarAgent 2>/dev/null || true' EXIT

echo '[2] 读取两边当前值'
W=$(grep -o '"bytes": [0-9]*' "$STATE" | head -1 | grep -o '[0-9]*$')
B=$(grep -o '"traffic": [0-9]*' "$BT" | grep -o '[0-9]*$')
DELTA=$((B - W))
echo "W(today.bytes)=$W  B(宝塔traffic)=$B  DELTA=$DELTA"

if [ "$DELTA" -le 0 ]; then
  echo '无差额（或异常），直接恢复服务'
  exit 0
fi

echo '[3] 补差'
$PY /tmp/_patch-traffic-state.py "$DELTA" "$SITE"

echo '[4] 启动 StarAgent'
systemctl start StarAgent
sleep 3

echo '[5] 验证：两边当前值'
curl -s http://127.0.0.1:5501/star/webTraffic | grep -o 'todayBytes[^,]*' || true
grep -E 'traffic|requests' "$BT"
echo '[6] done'
