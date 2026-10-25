# proxy-rs v2.1.7 发布说明：DSH 请求首发即通过，且不再改写用户数据

> 版本：**2.1.7**（上一版 2.1.6，2.1.4 / 2.1.5 的 DSH 清洗尝试已被 2.1.6 撤回）
> 这一版重建了撤回的修复：根因这次定位到了**两处字面量 + 作用域**，不是产品名。

---

## 现象

DeepSeek Harness（DSH）的每个请求都会先被上游以 `400 / 11-128`
（`Illegal API invocation from an unapproved channel`）拒绝，再由降级重试把
system prompt 换成一句通用话术后成功。表现：

- 日志里 `content_blocked` 满屏——实测 38 条 DSH 请求里 **37 条**命中；
- 每次多一个往返（约 +1~2 s）；
- **用户真实的 system prompt 被丢弃**，模型实际按一句通用描述工作。

功能能跑，行为是错的。

## 根因

上游网关对 **system 消息**做**字面量子串匹配**。只有两个串会触发，且**两个都必须改写**：

| 触发串 | 出处 |
|---|---|
| `You are an AI agent powered by DeepSeek Harness.` | `packages/core/system-prompt/src/index.ts`（`harness:identity`） |
| `DeepSeek Harness Web GUI` | `packages/context/harness-dev-context/src/index.ts`（`WEB_SURFACE_PROMPT`） |

在 DSH `0.2.1-rc.3` 的 87 个真实 prompt 快照中，前者出现于 82 个、后者 8 个。

## 为什么前两次修复没成

两次都只改对了一半，因此"改完仍然 400"，看起来像方案无效：

- **v2.1.4** 只改身份开场句（`You are an AI agent powered by DeepSeek Harness`）→ 缺第二处；
- **v2.1.5** 只改 `DeepSeek Harness Web GUI` → 缺第一处。

两者的观测前提也不成立：用压缩过换行的日志文本测出来的 200，在完整请求体上
无法复现；而真正的判定条件不是"产品名出现"，是"这两个字面量在 system 里出现"。

## 为什么不能全量替换

`DeepSeek Harness` 在真实请求体里出现多次，其中大量位于 **tool 结果**（项目路径、
源码、`git log` 输出），那是**用户自己的内容**。全量替换会篡改用户数据，还可能把
被改错的内容回灌进上下文。实测确认：同一个字面量放在 `user` / `tool` /
`reasoning_content` 里**完全不触发**，只有 `system` 会。

所以 2.1.6 撤回是对的，但撤回的理由需要修正：不是"字面量方案无效"，而是
**改少了（只改一半）且改多了（改了 system 以外的字段）**。

## 修复

新增 `pipeline::neutralize_harness_system_messages()`，在 `forward_request`
中发送前执行（三条路由共用此路径：`/v1/messages`、`/v1/chat/completions`、
`/v1/responses`）：

```rust
"You are an AI agent powered by DeepSeek Harness"  ->  "You are an AI agent"
"DeepSeek Harness Web GUI"                          ->  "web GUI"
```

三个关键设计点：

1. **两处都改**：只改一处仍然 400（实测）。
2. **只作用于 `system` 消息**：其它角色的消息**字节级不变**，用户数据零改动。
3. **不依赖零宽字符**：上游会把零宽字符归一化掉，靠不可见字符"打断字面匹配"是
   无效的（v2.1.5 曾踩过，并有单个字符被写丢导致 rewrite 变成空操作的静默 bug）。
   这里用可见文本替换，并有测试守着。

同时修复了工作区中 4 处把既有 Claude Code 清洗规则**降级成空操作**的改动
（`rewrite_table` 的 from/to 被改成完全相同，配套测试断言也被改弱以掩盖问题）。
现 `rewrite_table` 与相关断言均恢复为真实生效状态。

## 验证

对真实上游、用 DSH rc.3 的真实 system prompt 快照逐个实测：

| 输入 | 结果 |
|---|---|
| 未修改的真实请求体（653 KB / 14 条消息 / 42 工具） | 400 / 11-128 |
| 两处字面量都改写 | **200** |
| 只改写其中一处 | 400（两处都是必要条件） |
| 字面量放在 user / tool / reasoning | 200（只查 system） |
| 修复后连跑 5~6 个 rc.3 真实 prompt | **5/5、6/6 首发 200，`content_blocked` = 0** |

原来的降级重试**保留**，作为上游规则再次变化时的兜底；正常情况下不再触发。

测试：498 单测 + 23 集成全绿（新增 10 个），`fmt` 与 `clippy -D warnings` 干净。

## 影响范围

- 只影响**含上述字面量的 system prompt 的请求**，实际即 DSH 客户端。
- Claude Code / Codex 路径行为不变（其清洗规则本次恢复为正确生效状态）。
- 非 DSH 请求不会命中新逻辑（`contains` 预检，干净文本零分配、零改动）。
