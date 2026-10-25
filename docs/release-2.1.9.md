# proxy-rs v2.1.9 发布说明：拖拽排序仅在拖到顶部时同步默认身份

> 版本：**2.1.9**（上一版 2.1.8）
> 修复账号池拖拽排序会静默改写默认身份的问题，无其它行为改动。

---

## 背景

`ordered_credentials()` 会把"当前默认身份"提到列表最前面再返回（见其单测
`ordered_credentials_hoists_the_pinned_default_above_store_order`）。因此拖拽
把某个账号排到首位时，若不把默认身份一起改掉，刷新后就会被顶回去——用户看到
"拖了但没生效"。

v2.1.3 引入的修法是在 `wb_reorder_identities` 里判断：只要"当前默认不在首位"
就把默认改成首位。这个条件太宽：

- 它没有校验"这次拖拽是否真的把某行放到了顶部"，任何一次拖拽都会触发；
- 当默认是密钥（`k-…`）时，`__none__`/密钥并非被 `ordered_credentials` 提升的
  对象，账号重排本不该动它，却仍被顶层账号覆盖。

## 改动

把判定抽成纯函数 `workbuddy_auth::reorder_pin_target()`，语义与文档注释一致，
只在**确实有另一个身份被拖到顶部**时返回新默认：

| 当前默认 | 拖到顶部的是 | 结果 |
|---|---|---|
| 账号 `wb-a` | 它自己 | 不动 |
| 账号 `wb-a` | 另一个账号 `wb-c` | 默认改为 `wb-c` |
| 密钥 `k-1` | 任意账号 | 不动（密钥不在账号提升链路里） |
| `__none__` | 任意账号 | 不动（排序不等于同意启用账号池） |
| 未设置（旧版） | 旧版 `default_credential_id` 之外的行 | 同步，否则刷新会被顶回 |

`__none__` 这一条是刻意的：它表示"不使用账号池，只走密钥/单 Key 路径"
（`config.rs` 里据此 `force_no_pool`）。若拖拽顺手把默认改成某个账号，等于用
一次排序操作默认重新启用了用户明确关闭的账号池。

旧版 `default_credential_id` 只在统一选择器**从未设置**时才算"当前默认"，
不会被过期的历史字段遮蔽——否则密钥默认仍会被悄悄替换掉。

## 验证

`cargo test --lib`：**501 通过，0 失败**；`cargo fmt --check`、`cargo check --workspace` 干净。

新增单测 `reorder_pin_follows_only_a_new_top`（8 个断言）覆盖上表，包含
回归用例：密钥默认旁挂了过期的 `default_credential_id = wb-a` 时，把 `wb-c`
拖到顶部不得改写默认。

## 说明

本次改动最初被判断为"本机 Rust 工具链 miscompilation 导致 `cargo test` 无法通过"。
经复现核实，该判断**不成立**：同机 `rustc 1.99.1` 上 `cargo test --lib` 稳定
跑到 500 通过 / 1 失败，失败的是新加的测试本身；`--emit=mir` 报的
`/dev/rmeta…` 错误只是 `-o /dev/null` 与 macOS 不允许在 `/dev` 建临时目录所致，
去掉该参数即可正常产出 MIR。真实原因是 `reorder_pin_target()` 的当前默认判定
有误（见上），测试抓到的正是它。无需重装 Rust。
