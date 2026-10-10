#!/bin/sh
# =============================================================================
# Pek.RAgent 通用「应用安装进星尘」脚本（Linux）
# -----------------------------------------------------------------------------
# 把任意应用安装到本机星尘（Pek.RAgent / C# StarAgent）中，由其守护托管。
# 流程：文件就位 → 停止旧实例 → 注册子服务（-AddService，注册即启用并拉起）
#       → 就绪检查（可选）。
#
# 参考实现：DHDeploy.Agent.Rust/deploy/install.sh、Pek.RPanlServer/install.sh
# 维护：新应用打包时建议直接携带本脚本（固定 --name/--bin/--dir/--health 参数封装）。
#
# 用法：
#   sudo sh install-app.sh --name <服务名> --bin <程序文件> [选项]
#
# 示例：
#   sudo sh install-app.sh --name dhdeploy-agent-rust --bin ./dhdeploy-agent-rust \
#        --dir /www/DeployRust --health http://127.0.0.1:8282/api/panel/status
#
# 选项：
#   --name <名称>        子服务名称（必填，全局唯一）
#   --bin <路径>         程序文件（必填；相对路径先按当前目录、再按脚本目录解析）
#   --dir <目录>         安装目录（默认：程序文件所在目录，即就地安装）
#   --args "<参数>"      启动参数（可选，原样透传给星尘）
#   --health <URL>       就绪检查地址（可选，如 http://127.0.0.1:8080/ping）
#   --agent-exe <路径>   指定星尘程序路径（跳过自动探测）
#   --no-register        只就位文件，不注册到星尘（自行以 systemd/前台方式运行）
#   --unregister         从星尘注销该服务（先停止再移除条目；不删除文件）
#   -h | --help          显示帮助
#
# 星尘探测顺序：--agent-exe → systemd 单元（StarAgentRust/StarAgent）→ 运行中
# 进程（pek-ragent）→ 常见安装路径。自动注册仅支持 Rust 版星尘（-AddService），
# 检测到 C# 版时打印面板手动注册指引。幂等：重复执行 = 更新注册并确保运行。
# =============================================================================
set -e

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

NAME=""
BIN=""
DIR=""
ARGS=""
HEALTH=""
AGENT_EXE=""
DO_REGISTER=1
DO_UNREGISTER=0

usage() {
    cat <<'EOF'
用法：
  sudo sh install-app.sh --name <服务名> --bin <程序文件> [选项]

选项：
  --name <名称>        子服务名称（必填，全局唯一）
  --bin <路径>         程序文件（必填）
  --dir <目录>         安装目录（默认：程序文件所在目录，即就地安装）
  --args "<参数>"      启动参数（可选，原样透传给星尘）
  --health <URL>       就绪检查地址（可选，如 http://127.0.0.1:8080/ping）
  --agent-exe <路径>   指定星尘程序路径（跳过自动探测）
  --no-register        只就位文件，不注册到星尘
  --unregister         从星尘注销该服务（先停止再移除条目；不删除文件）
  -h | --help          显示帮助
EOF
    exit 0
}

while [ $# -gt 0 ]; do
    case "$1" in
        --name) [ -n "${2:-}" ] || { echo "错误：--name 缺少参数" >&2; exit 2; }; NAME=$2; shift 2 ;;
        --bin)  [ -n "${2:-}" ] || { echo "错误：--bin 缺少参数" >&2; exit 2; };  BIN=$2;  shift 2 ;;
        --dir)  [ -n "${2:-}" ] || { echo "错误：--dir 缺少参数" >&2; exit 2; };  DIR=$2;  shift 2 ;;
        --args) ARGS=${2:-}; shift 2 ;;
        --health) HEALTH=${2:-}; shift 2 ;;
        --agent-exe) AGENT_EXE=${2:-}; shift 2 ;;
        --no-register) DO_REGISTER=0; shift ;;
        --unregister) DO_UNREGISTER=1; shift ;;
        -h|--help) usage ;;
        *) echo "未知参数：$1（-h 查看用法）" >&2; exit 2 ;;
    esac
done

[ -n "$NAME" ] || { echo "错误：缺少 --name（-h 查看用法）" >&2; exit 2; }

# ---- 星尘程序探测 ----
detect_agent_exe() {
    # ① 显式指定
    if [ -n "$AGENT_EXE" ]; then
        if [ -f "$AGENT_EXE" ]; then
            printf '%s\n' "$AGENT_EXE"
            return 0
        fi
        echo "警告：--agent-exe 指定的文件不存在：$AGENT_EXE" >&2
    fi
    # ② systemd 单元（StarAgentRust = Rust 版；StarAgent = C# 版/旧名）
    if command -v systemctl >/dev/null 2>&1; then
        for unit in StarAgentRust StarAgent staragent; do
            exe=$(systemctl show -p ExecStart --value "$unit" 2>/dev/null | sed -n 's/.*path=\([^ ;]*\).*/\1/p' | head -n 1)
            if [ -n "$exe" ] && [ -f "$exe" ]; then
                printf '%s\n' "$exe"
                return 0
            fi
        done
    fi
    # ③ 运行中进程
    if command -v pidof >/dev/null 2>&1; then
        pid=$(pidof pek-ragent 2>/dev/null | awk '{print $1}')
        if [ -n "$pid" ]; then
            exe=$(readlink -f "/proc/$pid/exe" 2>/dev/null || true)
            if [ -n "$exe" ] && [ -f "$exe" ]; then
                printf '%s\n' "$exe"
                return 0
            fi
        fi
    fi
    # ④ 常见安装路径
    for p in /www/Agent/pek-ragent /opt/staragent/pek-ragent /opt/StarAgentRust/pek-ragent /usr/local/bin/pek-ragent; do
        if [ -f "$p" ]; then
            printf '%s\n' "$p"
            return 0
        fi
    done
    return 1
}

# ---- 本地控制接口调用（Rust 星尘 5501 / C# 星尘 5500）----
agent_http() {
    # $1=动作（可带查询串） $2=非空则静默输出
    command -v curl >/dev/null 2>&1 || return 1
    for port in 5501 5500; do
        resp=$(curl -s -m 15 "http://127.0.0.1:$port/$1" 2>/dev/null) || continue
        [ -n "$resp" ] || continue
        if [ -z "$2" ]; then
            printf '%s\n' "$resp"
        fi
        case "$resp" in
            *'"Success":true'*) return 0 ;;
            *) return 1 ;;
        esac
    done
    return 1
}

# ---- 注销模式 ----
if [ "$DO_UNREGISTER" = 1 ]; then
    echo "注销子服务 [$NAME]（保留文件）…"
    agent_http "StopService?serviceName=$NAME" quiet \
        || echo "  （停止请求未受理：星尘可能未运行或服务不存在；继续尝试移除条目）"
    REMOVED=0
    if command -v curl >/dev/null 2>&1; then
        for port in 5501 5500; do
            resp=$(curl -s -m 15 -X POST -H 'Content-Type: application/json' \
                   -d "{\"serviceName\":\"$NAME\"}" \
                   "http://127.0.0.1:$port/star/removeService" 2>/dev/null) || continue
            [ -n "$resp" ] || continue
            REMOVED=1
            printf '%s\n' "$resp"
            break
        done
    fi
    if [ "$REMOVED" = 1 ]; then
        echo "完成：已请求星尘移除条目（可在面板「子服务」页确认）。"
    else
        echo "未完成自动移除（星尘未运行或接口不可达）。请在星尘面板「子服务」页删除 [$NAME]，"
        echo "或编辑星尘配置 Config/StarAgent.config 移除对应条目。"
    fi
    exit 0
fi

# ---- 解析程序文件路径（相对路径：先当前目录、再脚本目录）----
if [ ! -f "$BIN" ] && [ -f "$SCRIPT_DIR/$BIN" ]; then
    BIN="$SCRIPT_DIR/$BIN"
fi
[ -f "$BIN" ] || { echo "错误：未找到程序文件 $BIN（请在包目录内运行，或用 --bin 指定路径）" >&2; exit 1; }
BIN_ABS=$(CDPATH= cd -- "$(dirname -- "$BIN")" && pwd)/$(basename -- "$BIN")

# ---- [1/4] 文件就位 ----
if [ -n "$DIR" ]; then
    TARGET_DIR=$DIR
else
    TARGET_DIR=$(dirname "$BIN_ABS")
fi
mkdir -p "$TARGET_DIR"
TARGET_DIR=$(CDPATH= cd -- "$TARGET_DIR" && pwd)
BIN_NAME=$(basename -- "$BIN_ABS")
TARGET="$TARGET_DIR/$BIN_NAME"
if [ "$BIN_ABS" != "$TARGET" ]; then
    cp -f "$BIN_ABS" "$TARGET"
fi
chmod +x "$TARGET"
echo "[1/4] 文件就位：$TARGET"

# ---- [2/4~3/4] 星尘探测与注册 ----
READY=0
REGISTERED=0
if [ "$DO_REGISTER" = 1 ]; then
    STAR_EXE=$(detect_agent_exe || true)
    if [ -z "$STAR_EXE" ]; then
        echo "[2/4] 未探测到星尘（Pek.RAgent / StarAgent）"
        echo "[3/4] 跳过注册：如服务器装有星尘，可用 --agent-exe <路径> 指定后重跑；"
        echo "      或在星尘面板手动注册：名称 $NAME，程序 $TARGET，目录 $TARGET_DIR"
    else
        case "$STAR_EXE" in
            *pek-ragent*)
                echo "[2/4] 检测到星尘（Pek.RAgent）：$STAR_EXE"
                # 覆盖安装/升级：先停旧实例（不存在时静默忽略）
                "$STAR_EXE" -StopService "$NAME" >/dev/null 2>&1 || true
                echo "[3/4] 注册并启动子服务：$NAME"
                if [ -n "$ARGS" ]; then
                    "$STAR_EXE" -AddService "$NAME" "$TARGET" "$TARGET_DIR" "$ARGS" || { code=$?; echo "注册失败（退出码 $code）：详见上方星尘输出" >&2; exit $code; }
                else
                    "$STAR_EXE" -AddService "$NAME" "$TARGET" "$TARGET_DIR" || { code=$?; echo "注册失败（退出码 $code）：详见上方星尘输出" >&2; exit $code; }
                fi
                REGISTERED=1
                ;;
            *)
                echo "[2/4] 检测到非 Rust 版星尘：$STAR_EXE"
                echo "[3/4] 跳过自动注册：C# 版请在星尘面板注册 ——"
                echo "      名称 $NAME，程序 $TARGET，目录 $TARGET_DIR"
                ;;
        esac
    fi
else
    echo "[2/4] --no-register：跳过星尘探测"
    echo "[3/4] 跳过注册（请自行以 systemd/前台方式运行）"
fi

# ---- [4/4] 就绪检查 ----
if [ -n "$HEALTH" ]; then
    echo "[4/4] 等待就绪（$HEALTH）…"
    if command -v curl >/dev/null 2>&1; then
        i=0
        while [ "$i" -lt 30 ]; do
            if curl -s -m 2 -o /dev/null "$HEALTH" 2>/dev/null; then
                READY=1
                break
            fi
            i=$((i + 1))
            sleep 2
        done
    else
        echo "      未安装 curl，跳过就绪检查"
    fi
else
    echo "[4/4] 未提供 --health，跳过就绪检查"
fi

echo ""
echo "=============================================================="
echo " [$NAME] 安装完成"
echo "   程序：$TARGET"
echo "   目录：$TARGET_DIR"
if [ -n "$ARGS" ]; then
    echo "   参数：$ARGS"
fi
if [ "$REGISTERED" = 1 ]; then
    echo "   托管：由星尘守护（覆盖程序文件后自动重启升级；面板「子服务」页可管理）"
elif [ "$DO_REGISTER" = 0 ]; then
    echo "   托管：未注册（自行管理）"
else
    echo "   托管：未注册进星尘（见上方 [2/4][3/4] 提示：面板手动注册或指定 --agent-exe 重跑）"
fi
if [ -n "$HEALTH" ]; then
    if [ "$READY" = 1 ]; then
        echo "   状态：就绪"
    else
        echo "   状态：暂未响应（可能仍在启动，或等待星尘拉起）"
    fi
fi
echo "=============================================================="
