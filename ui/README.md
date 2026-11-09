# ui/ — 桌面端前端

Tauri 2 桌面应用的前端：原生 ES 模块 + 原生 CSS + Tailwind utilities，**没有框架**。
`ui/` 是**源码**目录；Vite 把它构建到 `ui-dist/`，后者才是 `tauri.conf.json` 的
`frontendDist`，也是 `build.rs` 编进二进制的资产。改完 `ui/` 后运行 `npm run build:ui`
生成 `ui-dist/`。

```bash
task dev-server        # 起一个真实 proxy-rs-server（ui-dev 需要）
task ui-dev            # 浏览器热更新：Vite + 代理到真实后端
npm run build:ui       # 构建 ui/ → ui-dist/
npm run check:ui       # 静态一致性检查（级联顺序、模块图、id）
npm run check:dist     # 构建产物检查（路由前缀、入口标签形状、preflight）
npm run smoke          # 无头 Chrome 启动源码页面 + mock 后端，断言行为
npm run smoke:dist     # 同一套断言，对 ui-dist/ 产物跑
```

> **Taskfile 只保留日常用的任务**（`install` / `ui-dev` / `dev-server`，以及
> `install` 依赖的 `setup` / `build`）。前端检查走上面的 `npm run` 脚本，
> 它们与 CI 用的是同一套命令。

> **`ui-dev` 连的是真实后端。** 它不再自带 mock：Vite 托管 `ui/`，把 `/admin/*` 与
> `/api/*` 代理到 `task dev-server` 起的网关，登录、会话、RPC 全部是真实的。旧版
> `ui/tools/dev.mjs` 里那份 JS 会话解析器已删除——它总会和 `src/claude_sessions.rs`
> 差一个字段，且 mock 能通过、真实网关会失败的 bug 在这里永远复现不出来。

## 目录结构

```
ui/
  index.html           外壳：窗口 chrome + 每个视图一个空挂载点 + 样式表 <link>
  style/               样式，按「层次」拆分（见下）
  src/                 源码（Vite 的 root）
    core/              基础设施：dom（$ / html 模板 / 转义）、ipc（invoke/listen）、api（HTTP + IPC 回退）、state
    lib/               format 等纯函数（无 DOM）
    components/        可复用 UI 部件（modal / confirm / toast / overlay / table / pills）
    views/             每个标签页一个视图；settings/ 因规模较大拆成目录
  tools/               检查脚本：check-ui.mjs（静态）、check-dist.mjs（构建产物）、smoke.mjs（无头浏览器行为回归）、dev.mjs（ui-dev）
```

构建产物 `ui-dist/`（gitignore）的布局是**契约**：`src/`、`style/`、`assets/` 三个前缀由
`src/console.rs` 的路由和 `build.rs` 的 served roots 使用，`vite.config.js` 负责把产物放进
这三个目录，`ui/tools/check-dist.mjs` 负责验证。

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
| 8 | `logs.css` | 日志面板 chrome、工具条、视图切换 | `cards` |
| 9 | `logs-data-table.css` | 日志列表表格（`.data-table`）与外层滚动 | `logs` |
| 10 | `logs-console.css` | 控制台日志视图 | `logs` |
| 11 | `detail-table.css` | 请求详情弹窗内的键值表 | `logs` |
| 12 | `view-overview.css` | 概览页下钻 | 以上全部 |
| 13 | `view-settings.css` | 设置表单（分组、字段、chips、保存条） | 以上全部 |
| 14 | `view-logs.css` | 日志视图层 | 以上全部 |
| 15 | `view-sessions.css` | 会话双栏、列表行、徽标、分页、顶栏 | 以上全部 |
| 16 | `view-sessions-composer.css` | 会话详情：输入框与指标浮层 | `view-sessions` |
| 17 | `view-sessions-transcript.css` | 会话详情：时间线正文 | `view-sessions` |
| 18 | `wb-pool.css` | 身份池表格、拖拽排序、自动化对话框 | 以上全部 |
| 19 | `wb-qr.css` | 扫码登录二维码 | `wb-pool` |
| 20 | `wb-report.css` | 打卡 / 成长报告 | `wb-pool` |
| 21 | `tailwind.css` | Tailwind utilities（由 `npm run build:ui` 生成） | 以上全部 |

> 层叠顺序是语义的一部分：`logs.css` 原来的位置展开成 8–11，`views.css`
> 展开成 12–17，`workbuddy.css` 展开成 18–20。**拆文件时新文件名必须同时进
> `ui/index.html` 与 `ui/tools/check-ui.mjs` 的 `CASCADE` 数组**，否则
> `npm run check:ui` 会失败。

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
2. **不引入 UI 框架。** 渲染用 `core/dom.js` 的 `` html`` `` 模板标签——插值默认转义；需要嵌套标记时显式 `raw()`。不要用 `innerHTML` 拼字符串绕过它。
   **Tailwind 是一个例外，且只是 CSS 层**：`tailwind.css` 排在级联最后，`preflight` 在
   `tailwind.config.js` 里**关闭**（它的 reset 会落在 `base.css` 之后，改变现有 20 层的盒模型）。
   颜色、间距、圆角、阴影全部映射成 `var(--token)`，所以 `bg-surface` / `text-muted` 自动跟随
   深色模式，**不需要 `dark:` 变体**，也不允许在这里复制十六进制值。新增样式仍优先复用既有
   类；只有在写新组件且现有类不合适时才用 utility。`check-dist.mjs` 会在 preflight 被重新
   打开时失败。
3. **`appState` 只放跨模块共享的值。** 视图内部状态（选中项、定时器、已加载标记）留在该模块的闭包里。加字段前先确认真的有第二个模块要读。
4. **数据源只走 `core/`，视图不碰传输层。** 不要在视图里直接访问 `window.__TAURI__`，也不要在视图里手写 `fetch`：`core/ipc.js` 负责 IPC（浏览器里降级为日志 no-op，这是 `ui/tools/smoke.mjs` 能脱离 Tauri 跑完整启动流程的原因），`core/api.js` 负责两个观测面板的数据源。**首页统计与请求日志表走 HTTP 优先**（`GET /api/stats/today`、`GET /api/request-logs`），监听器不可用时自动回退到同名 Tauri 命令；两条传输返回同一份形状（由 `src/api.rs` 单点定义），所以视图不需要知道是谁答的。判断新数据属于哪边：需要 `AppHandle`、窗口或系统面板等能力的走 Tauri 命令；只读、可持久化查询的观测数据走 `/api/*`。
5. **打破循环依赖用注入，不用反向 import。** 已有两例：`palette.js` 接收 `switchTab`，`settings/oauth-qr.js` 接收 `repaint`。反向 import 会让模块初始化顺序变成隐式契约。
6. **`settings/` 的内部划分**：`form.js` 只搭骨架，`fields.js` 是纯标记构建器，`badges.js` 管「已修改」基线，`load.js` / `save.js` 读写后端，`credential-pool.js` / `oauth-qr.js` / `checkin.js` 管身份池，`pool-transfer.js` 管账号池导入 / 导出（两个弹窗），`dsh.js` 管 DSH 一键写入，`index.js` 是唯一对外接口。外部（`main.js` / `palette.js`）只从 `settings/index.js` 导入。

## 三、`id` 的命名与使用

代码大量依赖 `#id`，因此 **id 是接口**，不是实现细节。

- 一个 id 只在一个模块里创建；读取可以来自多处。
- 命名前缀标出归属：`tab-` 视图挂载点、`stat-` 概览指标、`setting-` 设置字段、`table-` 请求表、`log-` 控制台、`wb-` 身份池、`btn-` 按钮。
- 只属于某一个视图的行/组也要显式收拢，便于按视图整组显隐。例：`#table-actions` 与 `#table-filter-row` 都只在表格视图显示，`setLogsViewMode` 一并切换——把表格专用的操作放进共享的工具栏时，要保证切到控制台时它们一起隐藏。
- `setText(id, v)` 对不存在的元素是**静默 no-op**（这是刻意的降级设计），所以拼错 id 不会有任何报错——`ui/tools/check-ui.mjs` 就是为这类错误存在的：静态比对会拦截「只在变量里定义 id」，`--runtime` 则会加载真实页面核对每个静态 id 查找是否命中（按需创建的弹窗 id 会被识别并列出，不算失败）。

## 四、修改 UI 后的自检清单

```bash
npm run check:ui      # 静态：级联顺序、模块图、id
npm run smoke         # 无头浏览器行为回归（针对源码）
npm run build:ui && npm run check:dist && npm run smoke:dist   # 针对真正发布的产物
```

- [ ] 级联顺序与文件清单未变（或已同步更新本文件与 `index.html`）
- [ ] 新样式没写死颜色，或用的是既有令牌（用 Tailwind 时也走 `var(--token)`）
- [ ] 新 `id` 有归属前缀，且只创建一次
- [ ] 页面无 console 报错，四个标签页均正常渲染（运行概览 / 实时日志 / 会话 / 服务设置）
- [ ] 深色模式下目视确认（`prefers-color-scheme: dark`）
- [ ] **改完 UI 跑过 `npm run build:ui` 后的 `check:dist` + `smoke:dist`**：源码绿不等于产物绿（模块拆分、级联顺序、
      入口标签形状都只在构建后才可能出错）

`smoke.mjs` 会在无头 Chrome 里对着 mock 后端把真实页面完整启动一遍，断言概览指标、设置表单与身份池的渲染值，并验证两个跨模块集成点（命令面板的动态导入、扫码成功后身份池重绘）。改动这些路径后它应当仍然全绿；若断言值确实需要变，请一并更新脚本里的 `EXPECTED`，让改动是显式的而不是被忽略的。

该脚本还在自己的 origin 上实现了 `/api/stats/today` 与 `/api/request-logs`，**且故意与 IPC mock 取不同的值**（`get_stats` 报 1234 条、`/api/stats/today` 报 4321 条）。于是「首页统计来自 HTTP」和「没有回退到 IPC」是两条可证的断言，而不是靠两条路径恰好同值蒙混过去。新增 HTTP 化的面板时请沿用这个做法。

新增/调整视觉效果时，**改令牌或复用既有类**，不要为单个视图新开一套样式——这正是本目录各项约定要防的事。

## 五、标签页的 URL（hash 路由）

每个标签页都有自己的地址，**URL 是路由的唯一真相源**：

| 标签 | 地址 |
|---|---|
| 运行概览 | `#overview`（默认） |
| 实时日志 | `#logs` |
| 会话 | `#sessions` |
| 服务设置 | `#settings` |

- 点击标签、`window.switchTab('logs')`（托盘「显示日志」）、粘贴一个 `#sessions` 链接，都会写同一个 hash；`hashchange` 是浏览器前进/后退的入口。
- 未知 hash（例如旧链接的 `#old`）回落到 `#overview`，不会出现「所有面板都隐藏」。
- **会话还可以直接指向某一个会话**：`#sessions/<uuid>`。这是会话行「在浏览器打开」按钮使用的
  地址，让一个会话可分享、可收藏，刷新后仍停在同一处。
- 登录页也一样：`/admin/login#sessions` 登录成功后回到 `#sessions`；会话过期被踢到登录页时也会带上当前 hash。
  登录表单里有一个隐藏的 `next` 字段，服务端**只接受四个已知标签名**以及 `sessions/<uuid>`（uuid 必须是
  严格的小写 hex UUID）——它是一个表单字段，绝不能变成开放重定向。多接受这一种形态并不放宽该约束：
  它仍然无法表达 scheme、host 或路径穿越。

## 六、本地调试页面（连真实后端，不编译 Rust）

改 UI（布局、CSS、会话页）时不必每次起 Tauri 窗口。

```bash
task dev-server      # 终端一：起一个真实网关（$HOME/.proxy-rs-dev，token=dev）
task ui-dev          # 终端二：Vite 热更新，端口 4300
```

- Vite 托管 `ui/`，**热模块替换**——改 JS 模块即时生效，改 CSS 不刷新页面；
- `/admin/*`、`/api/*` 代理到真实网关：登录、会话列表（`~/.claude/projects`）、
  RPC 全部是真实数据；
- 首次访问会落到网关的登录页，登录后 cookie 落在 4300 这个 origin 上，之后照常使用。

换后端：`BACKEND=http://127.0.0.1:3456 task ui-dev`。

**这里没有 mock。** 旧版 `dev.mjs` 自带一套 JS 会话解析器，它总会和
`src/claude_sessions.rs` 差一个字段，而且「mock 能过、真实网关会失败」的 bug 在这里
永远复现不出来——所以删掉了。命令失败就是失败，看到的和线上一致。

`npm run smoke` 仍然用 mock（它要在没有后端的环境里跑回归），那是**测试**，不是调试。
