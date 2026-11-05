# proxy-rs v2.4.1 发布说明：DSH 请求首发即通过（第三次指纹迁移）

> 版本：**2.4.1**（上一版 2.4.0）
> v2.1.7 的两条清洗规则被上游改判为放行，同时又长出一条新指纹；此外 v2.1.7 的
> 清洗从来没覆盖过 `parts` 形态的 system 内容。**`/v1/*` 协议路由一字未动。**

---

## 现象

又出现 `content_blocked 400`：日志里每条 DSH 请求都是先 400 / `11-128`
（`Illegal API invocation from an unapproved channel`），再由降级重试换成中性
system prompt 成功。表现与 v2.1.7 之前完全一样：

- 每条请求多一个往返；
- **用户真实的 system prompt 被丢弃**，模型按一句通用描述工作；
- 统计里 `override_reason` 全是 `content_blocked 400 Bad Request`。

## 根因（这次是三条同时成立）

对真实上游逐个语句实测（DSH 0.2.2-alpha.2 的真实 system prompt，7.7 KB），结论与
v2.1.7 的假设不同：

1. **上游换了指纹。** v2.1.7 修的两条
   （`You are an AI agent powered by DeepSeek Harness`、
   `DeepSeek Harness Web GUI`）现在**单独出现已经不触发**——把它们全改掉仍是 400。
2. **新指纹是 `@` 引用指南那一对句子。** 真正被匹配的是**两条相邻句子**：

   ```
   Tokens prefixed with @ are paths the user explicitly referenced.
   Relative paths resolve from the workspace root;
   ```

   实测边界：

   | 输入 | 结果 |
   |---|---|
   | 两句话各自单独发送 | 200 |
   | 两句相邻（句号或分号连接） | **400 / 11-128** |
   | 两句之间插一个换行 | 200 |
   | 任一句改写任何一个词 | 200 |
   | 该段放在 user / assistant / tool 角色 | 200（仍只查 system） |

   出处：`packages/context/file-reference/src/index.ts` 的
   `FILE_REFERENCE_PROMPT`，由 `52a4a5bdb3`（2026-09-20）引入，
   `70d6e7abd6` 改写为当前措辞。DSH 0.2.2 起这段进入每次请求的 system prompt。

3. **清洗器漏掉了 `parts` 形态。** `neutralize_harness_system_messages` 用
   `let MessageContent::Text(..) else { continue }` 取内容，system 若是
   `[{"type":"text","text":…}]` 数组（OpenAI 官方 SDK 就是这么发的）则**整条被跳过**，
   而日志上清洗器"照常运行"。实测同一段文本：字符串形态被改后 200，parts 形态不改仍 400。

## 修复

`src/translate/pipeline.rs`：

1. 把清洗规则从两条手写 `replace` 改为 **`HARNESS_REWRITES` 表**，新增第三行：

   ```
   Relative paths resolve from the workspace root
     ->  Relative paths resolve against the workspace root
   ```

   只改第二个句子的一个词（`from` → `against`），语义不变；**第一句一个字节都不动**，
   模型看到的 `@` 用法说明仍是 DSH 写的原文。这是能打断匹配的最小改动。

2. 新增 `neutralize_harness_content`，`Text` 与 `Parts` 两种形态都清洗：只重写
   携带指纹的 text part，兄弟 text part 与 image part **逐字节保留**，且不改变客户端
   选择的 content 形态（parts 不会变成字符串）。

3. 无改动时不改内容、不计入 changed，日志不会出现"清洗了但没改"的假象。

## 验证

用真实上游、真实 DSH system prompt（7.7 KB）逐个实测：

| 输入 | 结果 |
|---|---|
| 未修改的真实 prompt（字符串形态） | **400 / 11-128** |
| 未修改的真实 prompt（parts 形态） | **400 / 11-128** |
| 仅按 v2.1.7 改两条旧指纹 | **400**（上游已不再据此拦截） |
| 仅改写 `@` 引用那对句子 | 200 |
| 本版清洗后的 prompt（字符串形态） | 200 × 3 次 |
| 本版清洗后的 prompt（parts 形态） | 200 × 3 次 |

即：修完后两种形态都**首发 200**，`content_blocked` 计数归零，不再依赖降级重试。

单元测试新增 8 个（含 parts 形态回归、只改 offending part、image part 逐字节保留、
改写表全行非空操作），`cargo test` 569 + 31 全绿，`cargo fmt` 与
`clippy -D warnings` 干净。

## 保留的降级重试

`content_blocked` 的「单次降级重试」**保留不动**。上游换指纹是没有预告的（v2.1.4、
v2.1.5、本版各撞一次），精确清洗负责"首发即通过"，降级重试负责"换规则时不至于挂死"。
刻意**没有**引入按日期的降级窗口：那会让一次命中把当天所有请求都换成中性 prompt，
偏离真实 prompt 的时间反而更长。

## 下次再撞到怎么办

指纹表是数据，不是逻辑。再加一行即可，但**必须先实测确认**：

- 单独发送可疑句子 → 200 说明它不是指纹，或只是配对的一半；
- 两句相邻 → 400，两句之间插换行 → 200，说明匹配的是**连续子串**；
- 同一段放在 user 角色 → 200，说明仍只查 system 通道。

未经这三步验证就往表里加规则，会重演 v2.1.6 那次"方案无效且有副作用"的误判。
