#!/bin/bash
# 把 dhdeploy-agent-rust 子服务的 ReloadOnChange 改为 true（配置热重载后生效）
# 用法: bash _remote-enable-reload.sh
set -e
CONF=/www/Agent/Config/StarAgent.config
sed -i '/dhdeploy-agent-rust/ s/ReloadOnChange="false"/ReloadOnChange="true"/' "$CONF"
echo '--- 修改后的配置行 ---'
grep -n 'dhdeploy-agent-rust' "$CONF"
echo '=== ENABLE-RELOAD-DONE ==='
