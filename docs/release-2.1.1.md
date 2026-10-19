# proxy-rs v2.1.1 发布说明：修复账号池会话粘滞错误回绑与粘滞计数显示

> 版本：**2.1.1**（上一版 2.1.0）
> 两个问题都出在账号池：一个是会真实影响请求走向的粘滞回绑，一个是 GUI 显示。

---

## 1. 会话粘滞回绑到已耗尽账号

### 现象

账号池里有一个账号额度用完、会话切到别的账号后，次日零点额度重置，或在 GUI 里把该账号重新启用，会话就**又回到那个账号**上。把第一个账号禁用时正常，重新启用就立刻"恢复粘滞"。

### 根因

`CredentialPool::pick()`（`src/session_pool.rs`）在粘滞绑定的账号不可用时，直接落到"默认 = 第一个可用账号"，**但旧绑定原样保留**：

1. 账号 A 额度耗尽 → 冷却到次日零点，会话 s1 由 `switch()` 切到 B，绑定记为 `s1 → A`。
2. 冷却期间对 s1 的请求：绑定账号 A 不可用 → 请求走默认（此时即 B），`s1 → A` 仍在表里。
3. A 恢复（零点重置 / 刷新成功 / 手动启用）后重新成为"第一个可用账号"，下一次 `pick()` 命中旧绑定，s1 悄悄回到 A。

绑定过期、账号被删除时同样是"落回默认但留着旧绑定"。

### 修复

| 情形 | 修复后的行为 |
|---|---|
| 粘滞账号不可用（冷却到零点、刷新失败 60 s 冷却） | **当前请求内**重新选号：`rehome()` 走 `switch()` 换到其他健康账号并重写绑定，与一次失败切换完全一致 |
| 粘滞账号被**手动重新启用** | `reload()` 立即清除指向该账号的绑定，会话重新纳入默认源优先策略 |
| 粘滞账号自动恢复 | 已切换会话**不**改道，缓存不受影响（保持原有约定） |
| 账号被禁用 | 绑定的会话在下一次请求时重新选号 |
| 绑定未刷新且离开超过 **10 分钟**，或已过期 | 直接丢弃，避免绑定在表里永久残留 |
| 没有可换的账号 | 回到默认源，等默认恢复后的下一次请求再选号 |

### 测试

`src/session_pool.rs` 新增/改写：

- `session_does_not_return_to_an_account_that_recovers_from_quota` —— 复现报告场景
- `reenabled_account_does_not_pull_sessions_back`
- `disabled_then_deleted_binding_rehomes_on_pick`
- `expired_sticky_binding_is_dropped`
- `stale_dead_binding_is_dropped_after_the_grace_period`

原 `expired_sticky_binding_is_ignored` 固化的正是旧的错误行为，已改写为 `..._is_dropped`。

---

## 2. 账号池列表每行都显示"1 个会话粘滞"

### 根因

两个问题叠加：

1. `wb_credentials_list`（`src-tauri/src/main.rs`）给**每一行**都填了**全池粘滞总数**，而不是按账号统计。
2. 前端（`ui/src/views/settings/credential-pool.js`）只要数字 > 0 就显示。

于是有一个会话粘滞时，每个账号都显示"1 个会话粘滞"。

### 修复

- 新增 `CredentialPool::sticky_sessions_for(id)`，按账号返回粘滞会话数。
- 前端只对**状态正常、且不是默认账号**的行显示粘滞数量：默认账号本来就是会话常驻的地方，冷却/禁用账号已经不服务任何会话。

---

## 3. 兼容性

- **无配置变更、无数据迁移**：粘滞表本来就不持久化，重启后全部会话回到默认源。
- **行为变更**：依赖"粘滞账号失效后自动落回默认源并保持旧绑定"的场景不会再出现回绑；这正是本次修复的目的。
- **版本号**：`proxy-rs`、`proxy-rs-gui`、`tauri.conf.json` 统一升到 2.1.1。

---

## 4. 验证清单

- [x] `cargo test` 全绿：471 单测 + 全部集成测试
- [x] `cargo clippy --all-targets -- -D warnings`（根 crate 与 src-tauri）无 warning
- [x] `cargo fmt --check` 通过
- [x] `node --check ui/src/views/settings/credential-pool.js` 通过
- [ ] 桌面端实际观察：跨零点切换后会话仍留在替换账号；账号池列表只在实际承接粘滞会话的非默认账号上显示数量
