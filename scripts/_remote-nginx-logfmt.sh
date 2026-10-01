#!/bin/bash
# Pek.RAgent: 为宝塔站点启用扩展日志格式（实际发送字节 $bytes_sent $request_length）
# 用法：ssh root@host "bash -s -- <站点名>" < this_script.sh   （幂等，带备份）
set -e
SITE=${1:?用法: bash -s -- <站点名>}
CONF=/www/server/panel/vhost/nginx/$SITE.conf
cp -a "$CONF" "$CONF.bak-bw"

# 1) 顶层插入 log_format（已存在则跳过）
grep -q agent_bw "$CONF" || sed -i "1i log_format agent_bw '\$remote_addr - \$remote_user [\$time_local] \"\$request\" \$status \$body_bytes_sent \"\$http_referer\" \"\$http_user_agent\" \$bytes_sent \$request_length';" "$CONF"

# 2) access_log 挂上新格式
sed -i -E 's|(access_log[^;]*\.log)[[:space:]]*;|\1 agent_bw;|' "$CONF"

echo '--- relevant lines after edit ---'
grep -nE 'access_log|agent_bw' "$CONF"

echo '--- nginx -t ---'
nginx -t
nginx -s reload
echo '=== NGINX-FORMAT-DONE ==='
