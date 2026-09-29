# ui/ — 桌面端前端

Tauri 2 桌面应用的前端：原生 ES 模块 + 原生 CSS，**没有打包器、没有框架、没有构建步骤**。
`ui/` 目录整体即 `src-tauri/tauri.conf.json` 中的 `frontendDist`，改动后重新构建/刷新即生效。

```bash
task dev                              # 启动 Tauri 开发窗口（端口 3477，独立数据目录）
task ui-check                         # 静态一致性检查（级联顺序、模块图、id）
task ui-smoke                         # 无头 Chrome 启动真实页面 + mock 后端，断言行为
task ui-test                          # 上面两个一起跑；已并入 task check
```

## 目录结构

```
ui/
  index.html           外壳：窗口 chrome + 每个视图一个空挂载点 + 样式表 <link>
  style/               样式，按「层次」拆分（见下）
  src/
    core/              基础设施：dom（$ / html 模板 / 转义）、ipc（invoke/listen）、state
    lib/               format 等纯函数（无 DOM）
    components/        可复用 UI 部件（modal / confirm / toast / overlay / table / pills）
    views/             每个标签页一个视图；settings/ 因规模较大拆成目录
  tools/               检查脚本：check-ui.mjs（静态）、smoke.mjs（无头浏览器行为回归）
```

## 一、CSS 分层约定

`index.html` 中样式表**按以下顺序**加载，后者可覆盖前者。这个顺序是设计决策，不是偶然：

| 顺序 | 文件 | 职责 | 允许覆盖谁 |
|---|---|---|---|
| 1 | `tokens.css` | 设计令牌（颜色 / 圆角 / 阴影 / 尺寸），含深色模式 | 无——所有层都读它 |
| 2 | `base.css` | reset、`[hidden]`、body/窗口 chrome、焦点环 | 无 |
| 3 | `controls.css` | `.btn*`、表单控件、开关 `.toggle` | `base` |
| 4 | `badges.css` | `.badge-pill`、状态 / 流式 / override / 客户端 pill | `base` |
| 5 | `overlays.css` | 模态、确认框、Toast、命令面板，以及通用工具类 | `base` |
| 6 | `layout.css` | 应用外壳：`.app` / `.header` / `.tabs` / `.tab-content` | `base` |
| 7 | `cards.css` | `.metric-card`、`.section`、`.kv`、`.speed-sessions` | `controls`、`badges` |
| 8 | `logs.css` | 日志面板 chrome、`.data-table`、控制台、详情表 | `cards` |
| 9 | `views.css` | 视图级规则：设置表单、日志视图等 | 以上全部 |
| 10 | `workbuddy.css` | WorkBuddy 账号池 / 扫码 / 打卡报告 | 以上全部 |

### 硬性规则

1. **颜色只能来自令牌。** 写 `var(--accent)`，不要写 `#0071e3`。唯一的例外有两处，且都必须在注释里写明理由：per-client **品牌色**（`.client-codex` / `.client-claude` / `.client-dsh`）与二维码白底（扫码必须白底深码）。需要新的语义色或分类色时，先在 `tokens.css` 里**同时**定义浅色与深色两个值——只定义浅色会让该元素在深色模式下保持浅色值，这正是本目录历史上颜色不一致的主要来源。
2. **小字号文字用 `-ink`，不要用填充色。** `--success` / `--warning` / `--error` / `--accent` / `--identity-*` 是**填充色**，用作 10–11px 文字时对比度只有 2.3–4.1:1（低于 WCAG AA 的 4.5:1）。凡是"小字压在自身浅色 tint 上"的场景（状态 pill、分类 pill、`.btn-active`），文字一律取对应的 `--*-ink`——浅色模式下它们是加深值，深色模式下是提亮值。新增语义色时请一并提供 ink 变体。
2. **不要写 `var(--x, fallback)`。** 令牌已全部定义，fallback 是死代码，而且会把调色板复制到第二处、与令牌悄悄分叉。`check-ui` 之后新增的校验会拦截未定义的 `var()`。
3. **不要在 JS 里写内联样式或颜色。** `style="color:var(--success)"` 之类会绕过层叠并在 JS 里重复调色板；改为在 CSS 里加类（如 `.hint.ok` / `.wb-tone-err`），用 `className` 切换。`ui/src` 中不应再出现 `style="`。
4. **不要新增 CSS 文件。** 需要新样式时，放进上表对应职责的文件。确实需要新增一类时，必须同时更新 `index.html` 的 `<link>` 与本表的顺序，`check-ui.mjs` 会拦截未登记的样式表。
5. **选择器用类，不用元素 / id。** 元素选择器（`input { … }`）会渗到所有视图，这是历史上 UI 差异的主要来源；`.form-group input` 这类**作用域限定**才是允许的写法。
6. **同一外观只有一处定义。** 卡片边框/圆角/阴影一律复用 `.section` / `.form-section` / `.logs-panel` 的模式，不要为某个视图另写一套"看起来差不多"的卡片。新增前先 `grep` 是否已有可复用类。
7. **按钮尺寸由层级决定，不由内容决定。** `.btn` 固定 34px、`.btn-small` 固定 29px，且用 `inline-flex` 居中——否则以 emoji 开头的标签（emoji 行盒更高）会比同行按钮高出 2–3px。新增按钮只用既有层级 + 色调组合（`.btn` / `.btn-primary` / `.btn-danger`），不要新开尺寸。
8. **选中态统一用背景表达，不用彩色描边。** 参照 `.metric-card.expandable.active` 与身份池的 `.wb-credential-row--default`：都是 `background: var(--surface2)`。蓝色描边会让一个元素看起来像"另一类对象"，并与行内的 accent 色调 pill 抢视觉。
9. **颜色只用于"状态"，不用于"身份"或"模式"。** 一行表格里最多只应有一种**语义色**在表达信息。判断标准：这个属性会出错吗？
   - 会出错（HTTP 状态、账号是否冷却、override 是否失败）→ 用语义色（绿/琥珀/红）。
   - 只是分类（哪个客户端、流式还是非流式）→ 用中性灰 + 字重区分。
   历史上客户端 pill 一个 client 一个品牌色、流式 pill 用 accent 蓝，于是一行里同屏 5 种色相，而其中 3 种重复表达了文字已经说清的事。新增 pill 前先问：这条信息是否已由文字或相邻 pill 表达？

## 二、JS 约定

1. **视图负责渲染，main.js 负责装配。** 每个视图导出 `render*()`（建 DOM）与 `init*()`（绑事件），由 `main.js` 在启动时各调用一次。视图不自行启动，也不互相 `init`。
2. **不引入框架。** 渲染用 `core/dom.js` 的 `` html`` `` 模板标签——插值默认转义；需要嵌套标记时显式 `raw()`。不要用 `innerHTML` 拼字符串绕过它。
3. **`appState` 只放跨模块共享的值。** 视图内部状态（选中项、定时器、已加载标记）留在该模块的闭包里。加字段前先确认真的有第二个模块要读。
4. **IPC 只走 `core/ipc.js`。** 不要在视图里直接访问 `window.__TAURI__`；`invoke` 在浏览器里会降级为日志 no-op，这正是 `ui/tools/smoke.mjs` 能脱离 Tauri 跑完整启动流程的原因。
5. **打破循环依赖用注入，不用反向 import。** 已有两例：`palette.js` 接收 `switchTab`，`settings/oauth-qr.js` 接收 `repaint`。反向 import 会让模块初始化顺序变成隐式契约。
6. **`settings/` 的内部划分**：`form.js` 只搭骨架，`fields.js` 是纯标记构建器，`badges.js` 管「已修改」基线，`load.js` / `save.js` 读写后端，`credential-pool.js` / `oauth-qr.js` / `checkin.js` 管身份池，`dsh.js` 管 DSH 一键写入，`index.js` 是唯一对外接口。外部（`main.js` / `palette.js`）只从 `settings/index.js` 导入。

## 三、`id` 的命名与使用

代码大量依赖 `#id`，因此 **id 是接口**，不是实现细节。

- 一个 id 只在一个模块里创建；读取可以来自多处。
- 命名前缀标出归属：`tab-` 视图挂载点、`stat-` 概览指标、`setting-` 设置字段、`table-` 请求表、`log-` 控制台、`wb-` 身份池、`btn-` 按钮。
- 只属于某一个视图的行/组也要显式收拢，便于按视图整组显隐。例：`#table-actions` 与 `#table-filter-row` 都只在表格视图显示，`setLogsViewMode` 一并切换——把表格专用的操作放进共享的工具栏时，要保证切到控制台时它们一起隐藏。
- `setText(id, v)` 对不存在的元素是**静默 no-op**（这是刻意的降级设计），所以拼错 id 不会有任何报错——`ui/tools/check-ui.mjs` 就是为这类错误存在的：静态比对会拦截「只在变量里定义 id」，`--runtime` 则会加载真实页面核对每个静态 id 查找是否命中（按需创建的弹窗 id 会被识别并列出，不算失败）。

## 四、修改 UI 后的自检清单

```bash
task ui-test          # = check-ui（静态）+ smoke（无头浏览器行为回归）
```

- [ ] 级联顺序与文件清单未变（或已同步更新本文件与 `index.html`）
- [ ] 新样式没写死颜色，或用的是既有令牌
- [ ] 新 `id` 有归属前缀，且只创建一次
- [ ] 页面无 console 报错，三个标签页均正常渲染
- [ ] 深色模式下目视确认（`prefers-color-scheme: dark`）

`smoke.mjs` 会在无头 Chrome 里对着 mock 后端把真实页面完整启动一遍，断言概览指标、设置表单与身份池的渲染值，并验证两个跨模块集成点（命令面板的动态导入、扫码成功后身份池重绘）。改动这些路径后它应当仍然全绿；若断言值确实需要变，请一并更新脚本里的 `EXPECTED`，让改动是显式的而不是被忽略的。

新增/调整视觉效果时，**改令牌或复用既有类**，不要为单个视图新开一套样式——这正是本目录各项约定要防的事。
