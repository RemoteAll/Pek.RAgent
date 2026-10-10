# 应用安装进星尘（通用脚本说明）

> 脚本：`install-app.sh`（本目录）。
> 参考实现：`DHDeploy.Agent.Rust/deploy/install.sh`、`Pek.RPanlServer/install.sh`（本脚本为通用版）。

用于把任意应用（单文件二进制）安装到本机**星尘**（Pek.RAgent，或 C# 版 StarAgent）中，由其守护托管（守护拉起、影子目录升级、面板可视化管理）。应用打包时建议直接随包携带本脚本，固定参数封装为自身的一行安装入口。

## 1. 快速使用

```bash
# 就地安装（脚本与程序在同一目录）并注册进星尘
sudo sh install-app.sh --name myapp --bin ./myapp

# 安装到指定目录 + 启动参数 + 就绪检查
sudo sh install-app.sh --name myapp --bin ./myapp --dir /opt/myapp \
     --args "urls=http://*:8080" --health http://127.0.0.1:8080/ping

# 从星尘注销（停止并移除条目，保留文件）
sudo sh install-app.sh --name myapp --unregister
```

## 2. 脚本流程

1. **文件就位**：必要时复制到 `--dir`（默认就地安装），补可执行位；
2. **停止旧实例**：`-StopService`（不存在/未运行静默忽略），保证覆盖安装幂等；
3. **注册**：`-AddService <名称> <程序> <目录> [参数]`——写入星尘配置并启用；星尘运行中则立即重载并启动，未运行时星尘启动后自动拉起；
4. **就绪检查**（`--health`）：轮询等待（最长约 60 秒）。

## 3. 星尘探测顺序

`--agent-exe` 显式指定 → systemd 单元（`StarAgentRust` / `StarAgent`）→ 运行中进程（`pidof pek-ragent`）→ 常见安装路径（`/www/Agent`、`/opt/staragent` 等）。
探测不到时打印星尘面板手动注册指引（名称 / 程序 / 目录三项）。

## 4. C# 版星尘

Rust 版（Pek.RAgent）走 `-AddService` 自动注册；C# 版无该命令，脚本按 DHDeploy 同款方式打印**面板手动注册**指引（两种星尘的配置项同名同格式，可相互接管）。

## 5. 与各应用自有 install.sh 的关系

- 现有应用脚本（DHDeploy Agent、RPanlServer 等）功能与之一致，可按需迁移到本脚本（少维护一份探测逻辑）；
- 新应用：直接随包携带 `install-app.sh`，在自身发布脚本中固定参数调用即可。
