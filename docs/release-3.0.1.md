# proxy-rs v3.0.1 发布说明：网关访问 key 兼容标准客户端头

> 版本：**3.0.1**（上一版 3.0.0）
> v3.0.0 引入了入站访问账户，但要求客户端额外配置 `x-proxy-rs-key` 请求头。
> 本版让**标准客户端头**也能携带网关 key：用户只需填 **key + host**，不必改任何请求头配置。

---

## 为什么要这一版

v3.0.0 的门禁只认 `x-proxy-rs-key`。安全上没问题，但可用性有代价：

- Claude Code 的 `ANTHROPIC_AUTH_TOKEN` 走 `Authorization: Bearer`；
- Claude Code / 多数 SDK 的 `api_key` 走 `x-api-key`；
- 要让它们带自定义头，得逐个客户端确认支持情况——而这件事在 3.0.0 里还没验证完。

对「只想填个 key 和地址就能用」的场景，这是不必要的摩擦。

---

## 改法：接受标准头，但**只在形状匹配时**消费

门禁现在按优先级看三个位置：

| 优先级 | 头 | 谁在用 |
|---|---|---|
| 1 | `x-proxy-rs-key` | 显式形式，始终优先 |
| 2 | `authorization: Bearer <k>` | `ANTHROPIC_AUTH_TOKEN`、OpenAI SDK `api_key` |
| 3 | `x-api-key: <k>` | Claude Code `ANTHROPIC_API_KEY` |

**关键约束**：标准头里的值，只有**形状像我们的 key** 时才被当作网关 key 消费。判定是 `looks_like_gateway_key`：`prs_` 前缀 **且** 总长恰好 47（43 个 base64url 字符 = 32 字节随机）。

这个条件是整个设计的核心，两个方向都错不得：

- **太松** → 真正的上游 key 被这里吞掉，请求带着空凭据去上游；
- **太严** → 用户只填了网关 key 却被拒。

上游 key（OpenAI `sk-…`、Anthropic `sk-ant-…`、WorkBuddy 登录态 JWT）都不可能同时满足「`prs_` 前缀 + 47 长度」，所以两套凭据系统不会互相干扰。

---

## 语义澄清：网关 key 只属于本网关

这是一个容易混淆、但本版**必须说清**的规则：

> 网关访问 key 只用于**本网关鉴权**。它在门口被消费掉，**绝不会转发给上游**。
> 上游仍然由**账号池**（或静态上游 key）认证。

即两件事完全分离：

| | 谁提供 | 去哪 |
|---|---|---|
| 网关访问 key | 客户端填在 key 字段里 | 只到本网关，用完即弃 |
| 上游凭据 | 账号池 / 静态 key / 登录态 | 网关 → 上游 |

被消费掉的头会从请求里移除，且上游凭据走的是独立路径（`upstream_auth_headers` 白名单构造），因此网关 key 在结构上不可能出现在出站请求里。

### 一个需要留意的组合

如果部署启用了 `UPSTREAM_API_KEY_PASSTHROUGH=true`，**而上游 key 恰好靠标准头透传**，那么客户端把网关 key 放进标准头会被消费掉，导致上游没有凭据。这是配置冲突，不是 bug。本版对此会打 `WARN` 日志明确提示：把上游 key 放进账号池，或改用 `x-proxy-rs-key` 发送网关 key。

---

## 客户端配置示例

配置好之后，客户端**只需两样东西**：

```bash
# Claude Code
export ANTHROPIC_BASE_URL=https://你的域名
export ANTHROPIC_AUTH_TOKEN=prs_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx

# 或直接用官方 SDK：api_key 填同一个值即可
```

Codex / OpenAI SDK 同理，`base_url` 指向网关、`api_key` 填网关 key。

---

## 测试

新增覆盖（`tests/client_auth.rs` 15 项、`client_accounts` 单测 13 项）：

| 用例 | 断言 |
|---|---|
| `authorization: Bearer <网关key>` | 通过 |
| `x-api-key: <网关key>` | 通过 |
| 显式头与标准头同时存在 | 显式头优先 |
| 标准头里的**上游** key | 不被消费，留给透传 |
| 标准头里的网关 key | **不出现在出站请求** |
| **网关 key 鉴权 + 上游走账号池** | 上游收到的是账号池 token，网关 key 零泄漏 |
| 形状判定 | 长度差一位、含非 base64url 字符、上游 key 全部拒绝 |

全量：`cargo test` **646 项全绿**；`clippy -D warnings`、`fmt`、`check-ui`（含 `--runtime`）、`smoke.mjs` 均干净。

---

## 兼容性

- 已用 `x-proxy-rs-key` 的客户端**不受影响**，该头仍是最高优先级。
- 未开启入站鉴权时，标准头的处理逻辑完全不执行（门禁第一行就返回）。
- 上游透传语义未改：非网关形状的值原样保留。

---

## 升级建议

1. 升级后在 GUI「访问账户」或 `POST /api/admin/client-accounts` 创建账户；
2. 把返回的 key 填进客户端（推荐 `ANTHROPIC_AUTH_TOKEN` / SDK 的 `api_key`）；
3. 若同时用 `UPSTREAM_API_KEY_PASSTHROUGH`，注意把上游 key 放账号池，避免标准头被网关 key 占用。
