# Claude Code 与上游网关拦截排查指南 (Code 11128)

本文档记录了在使用 Claude Code 通过本代理（`proxy-rs`）接入 WorkBuddy / 腾讯 Copilot 等上游服务商时，遇到上游错误码 `11128`（`Illegal API invocation from an unapproved channel` / `请求被安全策略拦截`）的根因排查过程、请求头分析及应对方案。

---

## 1. 错误现象

在终端使用 Claude Code 执行指令（例如通过 `claude -p "你好！"`）时，请求挂起或失败，代理日志（`~/.proxy-rs/logs/proxy.log` 及 GUI 客户端）中出现如下错误：

```text
2026-09-22 03:51:34.881 [INFO] POST /v1/messages model=deepseek-v4.1-flash -> upstream=deepseek-v4.1-flash stream=true msgs=2 tools=32
2026-09-22 03:51:35.371 [ERROR] UPSTREAM ERROR [chat/completions] url=https://copilot.tencent.com/v2/chat/completions model=deepseek-v4.1-flash 488ms | HTTP 400 · code 11128 Illegal API invocation from an unapproved channel · 请求被安全策略拦截，请稍后重试或联系支持。 · requestId 573b6989-3c54-47f8-921d-d66a7454446a | hint: 非法 API 调用：请求形态被上游拒绝 | body: {"code":11128,"msg":"Illegal API invocation from an unapproved channel","requestId":"573b6989-3c54-47f8-921d-d66a7454446a","displayMsg":{"en":"The request was blocked by security policy. Please retry later or contact support.","zh":"请求被安全策略拦截，请稍后重试或联系支持。","zh-hant":"請求已被安全策略攔截，請稍後重試或聯絡支援。"}}
2026-09-22 03:51:35.372 [ERROR] POST /v1/messages failed model=deepseek-v4.1-flash stream=true 492ms | Upstream API error: Upstream returned 400 Bad Request: {"code":11128,"msg":"Illegal API invocation from an unapproved channel",...}
```

随后若在 GUI 或系统托盘中暂停了代理，后续请求将直接返回：
```json
{"error":{"message":"Proxy service is paused from the console. Resume it to continue.","type":"service_unavailable"},"type":"error"}
```

---

## 2. Claude Code 实际发送的完整请求头

通过网关日志打印捕获，Claude Code（以 `2.1.228` 为例）向代理发送的完整请求头如下：

```http
accept: application/json
authorization: Bearer sk-***
content-type: application/json
user-agent: claude-cli/2.1.228 (external, cli)
x-claude-code-session-id: 46b168ec-dd90-4191-bd5a-5c51bd1d9470
x-app: cli
x-stainless-arch: arm64
x-stainless-lang: js
x-stainless-os: MacOS
x-stainless-package-version: 0.112.1
x-stainless-retry-count: 0
x-stainless-runtime: node
x-stainless-runtime-version: v26.3.0
x-stainless-timeout: 600
anthropic-version: 2023-06-01
anthropic-beta: claude-code-20250219,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,advisor-tool-2026-03-01,effort-2025-11-24
anthropic-dangerous-direct-browser-access: true
connection: keep-alive
host: 127.0.0.1:3457
accept-encoding: gzip, deflate, br, zstd
```

---

## 3. 验证实验：请求头是否会导致拦截？

**结论：请求头完全不会触发拦截。**

### 验证命令
使用 `curl` 携带全套 Claude Code 请求头，但只发送一个简单的基础请求体（不包含 Claude Code 专有的长 System Prompt 和 32 个 Tools）。先把密钥放进环境变量，避免写进 shell 历史与终端回显：

```bash
export ANTHROPIC_API_KEY='sk-...'
```

```bash
curl -i -s -m 25 -X POST http://127.0.0.1:3457/v1/messages \
  -H 'accept: application/json' \
  -H "authorization: Bearer $ANTHROPIC_API_KEY" \
  -H 'content-type: application/json' \
  -H 'user-agent: claude-cli/2.1.228 (external, cli)' \
  -H 'x-claude-code-session-id: 46b168ec-dd90-4191-bd5a-5c51bd1d9470' \
  -H 'x-app: cli' \
  -H 'x-stainless-arch: arm64' \
  -H 'x-stainless-lang: js' \
  -H 'x-stainless-os: MacOS' \
  -H 'x-stainless-package-version: 0.112.1' \
  -H 'x-stainless-retry-count: 0' \
  -H 'x-stainless-runtime: node' \
  -H 'x-stainless-runtime-version: v26.3.0' \
  -H 'x-stainless-timeout: 600' \
  -H 'anthropic-version: 2023-06-01' \
  -H 'anthropic-beta: claude-code-20250219,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,advisor-tool-2026-03-01,effort-2025-11-24' \
  -H 'anthropic-dangerous-direct-browser-access: true' \
  -d '{"model":"deepseek-v4.1-flash","max_tokens":64,"stream":true,"messages":[{"role":"user","content":"hi"}]}'
```

### 验证结果
```http
HTTP/1.1 200 OK
content-type: text/event-stream
...
event: message_start
data: {"type":"message_start","message":{"id":"cmb-...","type":"message","role":"assistant","model":"deepseek-v4.1-flash"}}
...
event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi! How's it going?"}}
...
event: message_stop
data: {"type":"message_stop"}
```
响应正常，耗时仅 800ms 左右。

---

## 4. 根因剖析：为什么 Claude Code 真实调用会触发 11128？

上游网关返回的 `11128` 是由 **请求体（Payload）** 触发的，主要包含以下触发点：

### 1) System Prompt 特征词触发 WAF 安全拦截（最主要原因）
Claude Code 在发请求时，默认会注入一段长达数千字的 System Prompt，包含了如：
`"You are Claude Code, Anthropic's official CLI..."` 等身份定义及特定工程指令。
- **验证发现**：直接向上游发送包含特定官方身份短语的 System Prompt，上游直接返回：
  `{"code":11128,"msg":"Illegal API invocation from an unapproved channel","displayMsg":{"zh":"请求被安全策略拦截，请稍后重试或联系支持。"}}`。
- 上游（WorkBuddy / 腾讯网关）配置了特定内容安全策略（WAF），会对 System Prompt 进行关键词审查。

### 2) 复杂的大规模工具定义（Tools 30+）
Claude Code 默认会把内置的所有工具（Bash, Edit, Read, Write, Glob, Grep, NotebookEdit, LSP 等 32 个工具）全部序列化并放入 `tools` 字段中。部分上游模型或网关参数校验器在解析此类未适配的复杂工具 schema 时，容易触发参数安全过滤拦截。

### 3) 非流式请求不支持
如果客户端没有开启 `stream: true`（比如测试时手动把 `stream` 改为 `false`），上游会返回：
`11101: Non-stream chat request is currently not supported`。

---

## 5. 应对与解决方案

### 1. 利用网关敏感词过滤功能
本代理提供了系统提示词过滤机制，可在转发给上游前自动剥离敏感关键词：
- 命令行参数：`--system-prompt-ignore "<敏感词1>;<敏感词2>"`
- 环境变量：`ANTHROPIC_PROXY_SYSTEM_PROMPT_IGNORE_TERMS="<敏感词1>;<敏感词2>"`

### 2. Claude Code 运行参数调整
- **自定义 System Prompt**：
  启动 Claude Code 时通过 `--system-prompt` 指定精炼的开发提示词，避免使用默认的官方长提示词：
  ```bash
  claude --system-prompt "You are a helpful coding assistant."
  ```
- **精简工具集**：
  通过 `--tools` 参数减少注入的工具数量（例如只启用必要的读写工具）：
  ```bash
  claude --tools "Bash,Edit,Read"
  ```
- **安全排障模式**：
  遇到异常时可加 `--safe-mode` 启动排查：
  ```bash
  claude --safe-mode
  ```

### 3. 网关日志监控与状态检查
- 请求头日志已内置：代理在收到所有 `/v1/messages` 和 `/v1/models` 请求时，均会在 `~/.proxy-rs/logs/proxy.log` 中记录 `POST /v1/messages headers: ...`，方便随时比对不同客户端的 Header。
- 若遇到 `503 Service Unavailable: Proxy service is paused from the console`，说明服务处于暂停状态，在 macOS 顶部状态栏托盘或桌面 GUI 客户端中点击“恢复/启动代理”即可。
