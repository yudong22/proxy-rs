# proxy-rs v2.1.8 发布说明：为清洗改写表补空操作守护测试

> 版本：**2.1.8**（上一版 2.1.7）
> 只加测试，无运行时行为改动。

---

## 背景

`pipeline::sanitize_text()` 依赖 `rewrite_table()` 里的 `(from, to)` 改写对。
如果某一对的 `from` 与 `to` 完全相同（例如原本用零宽字符打断字面匹配，
而写入时该字符丢失），这一对就变成**静默空操作**：

- `sanitize_text()` 照常执行、返回原串；
- 请求依旧被上游以 `400 / 11-128` 拒绝；
- 测试全绿，因为没有任何断言校验"改写确实改变了文本"。

v2.1.5 的 commit message 记录过这个现象（"rewrite 对 from/to 曾被写成完全相同，
等于空操作且测试无法察觉"）。

## 改动

新增两个守护测试，覆盖**整张表**而不是某一句：

| 测试 | 断言 |
|---|---|
| `every_rewrite_pair_actually_rewrites` | 表内不存在 `from == to` 的对 |
| `every_rewrite_pair_is_applied_by_sanitize_text` | 每个 `from` 经 `sanitize_text()` 后确实变成 `to` |

第二个测试同时守住 `has_fingerprint()` 预检：若某个 `from` 不在
`FINGERPRINT_FEATURES` 里，`sanitize_text()` 会在预检处提前返回，
改写永远不执行——这是另一种形式的静默失效，测试会直接报出来。

## 验证

**变异测试**：把 `Default branch (you will usually use this for PRs)` 这一对的
`to` 字节级改成与 `from` 完全相同（复现 v2.1.5 的缺陷形态），两个测试均失败：

```
assertion `left != right` failed: no-op rewrite pair (from == to): ...
assertion `left != right` failed: sanitize_text left ... untouched — has_fingerprint may not cover it
```

还原后全部恢复通过。测试：500 单测 + 23 集成全绿，`fmt` 与 `clippy -D warnings` 干净。

## 说明

2.1.7 的发布说明里写到"工作区有 4 处把既有清洗规则降级成空操作的改动"。
经逐版本核实，该说法**不成立**：`c5c33b4`（v2.1.6）及更早版本的
`rewrite_table` 中所有改写对均为真实生效，且 2.1.7 相对 2.1.6 对
`pipeline.rs` 的删除行数为 0（纯新增）。当时的判断源于未提交工作区的
中间态，并非仓库既有问题。本版把该误判对应到真正的预防措施上：
用测试守住"改写必须真的改写"。
