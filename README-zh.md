# TinyButler (小帮手)

"小帮手" 是一个非常简单的人工助理, 它不像openclaw或Hermes有沉重的框架, 而只专注于实现一个设置定时任务, 以及定时执行任务的功能. 
其只是简单调用本地的code agent CLI, 所以不需要任何额外的身份认证, 整个框架也很轻便快速.

# 为什么小帮手会存在?
本人已经试过openclaw和Hermes, 其定时任务令我着迷, 我经常会让其做一些如监控家里摄像头, 监控便宜机票, 打折商品等任务.
但是其框架沉重, 一方面造成程序不稳定, 另一方面大量消耗了我的token.

经过仔细思考, 我发现实际上我只是需要一个能够自动设置任务, 自动执行任务的个人助理. 记忆等功能自然会由想claude, codex等大公司帮我做, 我完全不需要操心.
对于一些单次的重量级任务, 我想如今天的codex/claude 远程模式更加合适.

Codex/Claude automation功能可能满足了我的需求, 但是 (1) 他们至今还没有支持linux环境, 导致我无法在我的小型单片主机 (如树莓派) 上运行他们. (2) 他们无法支持调用别的模型, 如目前我购买了codex和gemini模型, 不同任务我想让他们执行不同模型, 但目前他们不支持 (我不认为今后他们会支持). (3) 他们不支持执行简单bash指令, 有些简单任务, 传统程序更为可靠 (如从传感器中读取房屋温湿度信息), 但是它们并不支持.

# 小帮手设计哲学
1. 直接使用本机的code agent CLI, 省去重新登录或者需要API的麻烦.
2. 提供skills, 鼓励用户直接使用telegram chat 连接 code agent配置定时任务或是管理小帮手配置.
3. 只提供少量cli和telegram bridge接口, 用户使用code agent或是直接通过文件管理定时任务.

# 安装

从源码构建:

```bash
make
```

安装 release 版本二进制文件, 并启用用户级 systemd 守护进程:

```bash
make install
```

`make install` 遵循 Cargo 约定, 会运行 `cargo install --path . --force`, 通常会把 release 版本二进制文件安装到 `~/.cargo/bin/tinybutler`. 它会运行 `tinybutler init`, 在不覆盖用户配置或任务的前提下创建缺失的 home 文件, 并刷新 `~/.tinybutler/.agents/skills/` 下的 TinyButler 操作 skill. 如果 `~/.tinybutler` 缺少 `.git`, 它会把该目录初始化为 git 仓库, 写入 `~/.config/systemd/user/tinybutler.service`, 启用并重启用户服务, 让服务可以在开机时启动. 

# 配置目录
小帮手的配置目录在`~/.tinybutler/`, 目录结构如下:

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

config.yaml小帮手的主要配置, 在这里配置telegram桥接的机器人账号以及允许处理消息的个人账号:

```yaml
telegram:
  bot_token: "123456789:..."
  chat_id: "123456789"
```
详细配置方法请让你的LLM阅读`docs/chatbridge.md`文件后解释给你听!

以及配置code agent信息:
```yaml
code_agents:
  codex:
    command: /usr/bin/codex
    ...
```
详细配置请让你的LLM阅读`docs/configuration.md`文件后解释给你听!

# telegram 支持的指令
* `/tasks`, 列出定时任务, 状态, 以及试运行一次任务
* `/new`, 启动一个新session
* `/session`, 使用一个之前的session
* `/restart`, 重启服务.

# 二次开发
开发文档详见`AGENTS.md`.

# TODO
- 支持claude/gemini stream
- 支持微信, discord等其他聊天接入方式
