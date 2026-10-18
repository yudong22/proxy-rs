# proxy-rs v2.1.0 发布说明：上游终态错误透传 + Codex 压缩门限

> 版本：**2.1.0**（上一版 2.0.9）
> 两个改动都来自同一个真实故障：一个 1.18M token 的 Codex 会话撞上上游 1.05M 硬上限后，**连续重试 37 次**而没有压缩上下文。

---

## 1. 故障回顾

日志证据（`~/.proxy-rs/logs/proxy.log`，2026-10-18）：

```
HTTP 400 · code 11115 prompt is too long: 1187714 tokens > 1048576 maximum · 内容过长，请精简或新建任务
```

同一条提示词的相邻两跳：

| 时间 | Codex 看到的 input+cache | 结果 |
|---|---|---|
| 08:28:32 | 883,786 | 200 |
| 08:28:34 起 | 上游判定 1,187,714 | **400 → 代理记为 502** |

代理侧 `request_logs` 里，这 37 次失败全部是 `status 502`。

### 两条独立的失效链

**链 1：状态码被抹平。** `ProxyError::Upstream` 无条件映射为 502（`src/error.rs`）。上游 400 是「你这条请求本身不行」，502 是「网关暂时挂了」——语义完全相反。Codex 按后者处理，于是重发同一条 prompt。

**链 2：压缩门限没有余量。** 目录里声明 `context_window = 1,000,000`（上游的 `maxInputTokens`），但上游实际硬上限是 1,048,576。Codex 默认按 95% 触发压缩 = 950,000，只剩 66k 余量；而单次工具结果实测让 prompt 一步涨了 **303,928** token，压缩根本没有机会发生。

---

## 2. 改动一：上游终态错误原样透传

### 新增 `ProxyError::UpstreamRejected`

```rust
UpstreamRejected { status: u16, body: String, message: String }
```

- `status()` 返回上游自己的状态码
- `into_response()` 把上游错误体**原样**回传（JSON 走 JSON，非 JSON 走 text/plain），不再套一层 `{"error":{"type":"proxy_error"}}` 信封

### 判定规则

`is_terminal_rejection(status)` = **400 / 413 / 422**

| 状态 | 处理 | 理由 |
|---|---|---|
| 400 | 透传 | 请求本身有问题；Codex 的 `context_length_exceeded` 走这里 |
| 413 | 透传 | 请求体超上游上限 |
| 422 | 透传 | 形状不合法 |
| 401/402/403 | 502 | 凭据故障，代理内部已做失败转移 |
| 429 | 502 | 可重试，换池在前一步已发生 |
| 404 | 502 | 路由/模型配置错，不该算成客户端错误 |
| 5xx | 502 | 真正的网关故障，客户端重试是对的 |

### 接入点

| 位置 | 说明 |
|---|---|
| `src/proxy.rs` forward_request | 主路径：403 处改为 `ProxyError::from_upstream(...)` |
| `src/proxy.rs` list_models | `/v1/models` 上游错误 |
| `src/proxy/upstream_errors.rs` retry_as_stream | 11101 流式重试路径 |

### 测试

- `src/error.rs` 单测 4 个：终态保留状态、非终态仍 502、body 原样、非 JSON body 不炸
- `tests/upstream_status_relay.rs` **端到端 3 个**：真起一个本地 upstream，走完 `build_app_router` → reqwest → 响应，断言客户端看到的 status 与 body

> 端到端测试是必要的：如果只断言 `ProxyError::status()`，而埋点回归发生在 `forward_request` 构造错误那一刻，单测不会发现。

---

## 3. 改动二：Codex 目录的压缩门限

`CatalogModel` 新增 `effective_context_window_percent: u32`，全目录统一取 **85**。

```
1,000,000 × 85% = 850,000   ← Codex 压缩点
1,048,576                    ← 上游硬上限
850,000 → 1,048,576 余量 = 198,576，单步涨幅 303,928 仍在同量级
```

对比默认 95% 的 950,000（仅剩 98,576）。

该字段经 `codex debug models` 验证可被 Codex 读取并保留。

### 为什么用百分比而不是改 `context_window`

`context_window` 是「这个模型多大」的事实陈述，改小会同时误导 UI、token 计量与用户预期。百分比明确表达「这是安全边界，不是容量」。

---

## 4. 兼容性

- **无破坏性 API 变更**：`ProxyError` 加了变体，但它是内部错误类型，不对外暴露为稳定接口。
- **行为变更**：之前依赖「所有上游失败都是 502」的下游需要知道 400/413/422 现在会原样出现。这是有意的修正——用 502 掩盖客户端错误本身就是 bug。
- **配置无需迁移**：重新点一次「写入 Codex 模型目录」即可获得新字段；DSH profile 不受影响。

---

## 5. 验证清单

- [x] `cargo test` 全绿：467 单测 + 18 集成
- [x] `cargo clippy --all-targets` 无新增 warning
- [x] `codex debug models` 解析 20 个模型，`effective_context_window_percent = 85` 生效
- [x] DSH `resolveRouteModels(..., 'strict')` 通过，DeepSeek 三档 `low/high/max` 保留
- [x] 端到端：上游 400 → 客户端 400 且 body 含 `context_length_exceeded`；上游 500 → 客户端 502
