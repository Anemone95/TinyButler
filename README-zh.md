# TinyButler (小帮手)

"小帮手" 是一个非常简单的个人助理。它不像 OpenClaw 或 Hermes 那样有沉重的框架，而只专注于设置定时任务和按时执行任务。
它只是调用本机已经安装并登录好的 code agent CLI，所以不需要再接入 provider API，整体也很轻便快速。

## 为什么小帮手会存在?

本人已经试过 OpenClaw 和 Hermes，其定时任务令我着迷，我经常会让其做一些如监控家里摄像头、监控便宜机票、打折商品等任务。
但是其框架沉重，一方面造成程序不稳定，另一方面大量消耗了我的 token。

经过仔细思考，我发现实际上我只是需要一个能够自动设置任务、自动执行任务的个人助理。记忆等功能自然会由 Claude、Codex 等大公司帮我做，我完全不需要操心。
对于一些单次的重量级任务，我想今天的 Codex/Claude 远程模式更加合适。

Codex/Claude automation 功能可能满足了我的需求，但是：(1) 他们至今还没有支持 Linux 环境，导致我无法在我的小型单片主机，如树莓派，上运行他们；(2) 他们无法支持调用别的模型，如目前我购买了 Codex 和 Gemini 模型，不同任务我想让它们执行不同模型，但目前他们不支持，我也不认为今后他们会支持；(3) 他们不支持执行简单 Bash 指令，有些简单任务传统程序更为可靠，如从传感器中读取房屋温湿度信息，但是它们并不支持。

## 小帮手设计哲学

1. 直接使用本机的 code agent CLI，省去重新登录或者需要 API 的麻烦。
2. 提供 skills，鼓励用户直接使用 Telegram chat 连接 code agent 配置定时任务或管理小帮手配置。
3. 只提供少量 CLI 和 Telegram bridge 接口，用户使用 code agent 或直接通过文件管理定时任务。

## 当前状态

- 适合运行在 Linux 服务器、树莓派等小型常开机器上。
- 定时任务可以是普通 Bash 脚本，也可以调用本地 code agent CLI。
- Codex, `agy` 交互式 streaming 已接入；Claude streaming 仍在 TODO。
- TinyButler 不提供 Web UI，也不托管模型。

## 前置条件

- Linux 环境。
- Rust/Cargo 和 `make`。
- user-level systemd，用于 `make install` 安装守护进程。
- 如果要使用 agent 任务，需要提前安装并登录对应的本地 CLI，例如 `codex`、Gemini 使用的 Antigravity CLI（`agy`）或未来的 `claude`。Gemini chat streaming 需要 `agy` 1.1.15 或更高版本。
- 如果要使用 Telegram，需要准备 bot token 和允许操作的 chat id。

## 安装

从源码构建：

```bash
make
```

安装 release 版本二进制文件，并启用用户级 systemd 守护进程：

```bash
make install
```

`make install` 遵循 Cargo 约定，会运行 `cargo install --path . --force`，通常会把 release 版本二进制文件安装到 `~/.cargo/bin/tinybutler`。它会运行 `tinybutler init`，在不覆盖用户配置或任务的前提下创建缺失的 home 文件，并刷新 `~/.tinybutler/.agents/skills/` 下的 TinyButler 操作 skill。如果 `~/.tinybutler` 缺少 `.git`，它会把该目录初始化为 git 仓库，写入 `~/.config/systemd/user/tinybutler.service`，启用并重启用户服务，让服务可以在开机时启动。

## 快速开始

```bash
make
make install
tinybutler check
tinybutler tasks
```

配置 Telegram 后，在 Telegram 中使用 `/new` 新建一个会话。之后可以通过与 Telegram 机器人对话部署任务。小帮手会根据任务描述选择写一个脚本程序执行任务，或者使用 agent 执行任务，随后你会在固定时间收到任务执行结果。

## 配置目录

小帮手的配置目录在 `~/.tinybutler/`，目录结构如下：

```text
~/.tinybutler/
  config.yaml
  tasks/
    smoke-task/
      data/
      task.yaml
      agent.md
      state.json
    regular-check/
      data/
      task.yaml
      run.sh
      logs/
      state.json
```

`config.yaml` 是小帮手的主要配置。在这里配置 Telegram bridge 的机器人账号以及允许处理消息的个人账号：

```yaml
telegram:
  bot_token: "123456789:..."
  chat_id: "123456789"
```

以及配置 code agent 信息。下面是一个简化示例：

```yaml
code_agents:
  gemini:
    command: agy
    models:
      - gemini-3.7-flash-low
    new_args:
      - "--new-project"
      - "--model"
      - "{model}"
      - "--dangerously-skip-permissions"
      - "--output-format"
      - json
      - "--print={prompt}"
    resume_args:
      - "--model"
      - "{model}"
      - "--dangerously-skip-permissions"
      - "--conversation"
      - "{sessionId}"
      - "--output-format"
      - json
      - "--print={prompt}"
    stream_args:
      - "--new-project"
      - "--model"
      - "{model}"
      - "--dangerously-skip-permissions"
      - "--input-format"
      - stream-json
      - "--output-format"
      - stream-json
      - "{stdin}"

  codex:
    command: /usr/bin/codex
    models:
      - gpt-5.3-codex-spark
      - gpt-5.5
    new_args:
      - exec
      - "--json"
      - "-m"
      - "{model}"
      - "{prompt}"
    stream_args:
      - app-server
      - "-c"
      - model="{model}"
      - "-c"
      - sandbox_mode="danger-full-access"
      - "--listen"
      - "stdio://"
      - "{stdin}"
```

`models` 中的模型会在任务和聊天会话中写成 `group/model`，例如 `codex/gpt-5.5` 或 `gemini/gemini-3.7-flash-low`。非空 `stream_args` 启用交互式 chat，并包含 fresh process 的完整参数。TinyButler 展开占位符、移除 `{stdin}`，其余参数保持原样。Agy 和 Codex 模块各自识别配置的 `command`。

详细配置请让你的 LLM 阅读 `docs/configuration.md`、`docs/chatbridge.md` 后解释给你听。

## 任务示例

普通脚本任务`task.yaml`：

```yaml
name: regular-check
enabled: true
schedule: "*/10 * * * *"
type: command
timeout: 300
```

对应的 `run.sh`：

```bash
#!/usr/bin/env bash
set -euo pipefail

printf '**regular-check ok:** `%s`\n' "$(date --iso-8601=seconds)"
```

Agent 任务`task.yaml`：

```yaml
name: smoke-task
enabled: false
schedule: "0 9 * * *"
agents:
  - codex/gpt-5.3-codex-spark
type: agent
session: independent
timeout: 3600
```

对应的 `agent.md` 中写任务说明，例如：

```md
Inspect this task directory and summarize whether the task setup is healthy.
```

## 本地 CLI

```bash
tinybutler init
tinybutler daemon
tinybutler check
tinybutler restart
tinybutler tasks
tinybutler task list
tinybutler task status <task>
tinybutler chat new
tinybutler chat session
tinybutler telegram '<message>'
tinybutler telegram --attachment <path>
```

`tinybutler check` 用于验证配置和任务文件。修改 `config.yaml` 后使用 `tinybutler restart` 让守护进程重新加载配置。修改任务目录或 `task.yaml` 通常不需要 restart，因为 daemon 会定期重新扫描任务。

## Telegram 支持的指令

* `/tasks`：列出定时任务、状态，以及试运行一次任务
* `/new`：启动一个新 session
* `/session`：使用一个之前的 session
* `/abort`：中止当前正在执行的交互式 agent turn
* `/restart`：重启服务

## 二次开发

开发文档详见 `AGENTS.md`。

## License

BSD-3-Clause，详见 `LICENSE`。

## TODO

- 支持 Claude streaming
- 支持微信、Discord 等其他聊天接入方式
