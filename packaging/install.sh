#!/bin/sh
# Pek.RAgent Linux 一键安装（解压后执行：bash install.sh [额外参数]）
#
# 解决场景：通过 scp/网盘中转等不保留权限的方式获取文件后，
# pek-ragent 缺少可执行位导致 "./pek-ragent: Permission denied"。
# 本脚本自动补可执行位并安装系统服务；用 `bash install.sh` 运行时不依赖脚本自身权限。
set -e
cd "$(dirname "$0")"

if [ ! -f pek-ragent ]; then
    echo "未找到 pek-ragent（请在解压目录内执行本脚本）" >&2
    exit 1
fi

chmod +x pek-ragent

if [ "$(id -u)" -ne 0 ]; then
    echo "安装系统服务需要 root 权限，请改用：sudo bash install.sh" >&2
    exit 1
fi

# 运行时升级支持：若存在上传的新版本（pek-ragent.new），先原子替换（运行中也可替换）。
# 说明：Linux 内核禁止直接覆盖"正在执行的 ELF"（ETXTBSY），请上传为 pek-ragent.new 后执行本脚本；
# 服务模式下代理自身也会在约 10 秒内自动完成替换并重启（无需本脚本）。
if [ -f pek-ragent.new ]; then
    echo "检测到 pek-ragent.new，正在替换程序文件……"
    chmod +x pek-ragent.new
    mv -f pek-ragent.new pek-ragent
    echo "已替换，继续安装/重启服务。"
fi

exec ./pek-ragent -install "$@"
