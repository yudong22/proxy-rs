//! WorkBuddy 国内版「成长任务中心」客户端。
//!
//! 这里只做**无副作用的协议封装 + 分类判断**：任务清单、接取、事件上报、领奖、
//! 余额查询。谁来决定「跑哪些、跑几次、什么时候跑」属于编排层
//! （[`crate::growth_runner`]，P2），本模块不碰磁盘、不碰账号池、不看时钟。
//!
//! 这样切的原因：协议的形状（信封、事件字段、去重规则）会随上游变，而编排策略
//! （节流、每天一次、夜间窗口）会随产品决策变。混在一起时，改任意一半都要重新
//! 验证另一半；分开后，本模块可以用固定的 mock 响应把协议钉死。
//!
//! # 三条必须记住的上游规则（均来自实测，违反则静默失败）
//!
//! 1. **事件必须带 `userId`**，否则上游返回 `code: 0` 但把它当 heartbeat 丢弃，
//!    进度永远不动。
//! 2. **专家/团队事件按 `(eventCode, id)` 去重**：同一个 id 重复上报不累加进度，
//!    所以需要 id 池，每次换一个。
//! 3. **`auto` 类任务不接受接取**（`accept` 会拒），直接上报即可。
//!
//! # 能力边界
//!
//! 任务被分成三档（[`Automation`]）：能上报推进的、只认桌面客户端真实行为的、
//! 物理上不可能的。后两类**永远不进可执行列表**——假装跑一遍只会换来 400
//! `task not completed`，并让用户以为积分已经拿到了。

use crate::workbuddy_auth::WorkBuddyCredential;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// 默认成长接口 host。
///
/// 与聊天侧同一个 authority（桌面客户端的事件上报也打这里）。`www.codebuddy.cn`
/// 同样可用且数据一致（2026-10-25 实测），但聊天侧的指纹头与它配套，默认走这里。
pub const DEFAULT_GROWTH_ENDPOINT: &str = "https://copilot.tencent.com";

/// 任务清单路径。
const TASKS_PATH: &str = "/v2/activity/growth/tasks";
/// 批量接取路径。
const ACCEPT_PATH: &str = "/v2/activity/growth/tasks/accept";
/// 能量（成长值）余额路径。
const ENERGY_PATH: &str = "/v2/activity/growth/energy";
/// 事件上报路径。
const REPORT_PATH: &str = "/v2/report";

/// 一个成长任务能被自动化的程度。
///
/// 这三档不是"难度"分级，而是**是否应该尝试**的判据：把 [`Self::DesktopOnly`]
/// 当成 [`Self::Auto`] 去跑，会发出被上游忽略的事件并报告成功，用户以为积分到手
/// 了，实际一分没有。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Automation {
    /// 上报事件即可推进。
    Auto { event: EventKind },
    /// 只认桌面客户端的真实行为信号，伪造上报会被忽略。
    DesktopOnly {
        /// 给用户看的手动完成指引（含深链语义）。
        hint: String,
    },
    /// 物理上不可能自动化。
    Unforgeable {
        /// 为什么不能，给用户看。
        reason: String,
    },
}

/// 可以用事件推进的任务类型。
///
/// 每个变体对应一个上游 `eventCode` 与一组必填字段；字段形状见
/// [`GrowthEvent::build`]，那里是唯一构造事件的地方。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// 发起一次对话（`chat_request_send`）。
    Chat,
    /// 用 GLM 模型发起对话，同样走 `chat_request_send` 但模型字段不同。
    GlmChat,
    /// 夜猫子：仅在 23:00–08:00 上报才计数。
    NightCat,
    /// 召唤一次专家（`expert_actual_use`，`expertType=agent`）。
    Expert,
    /// 召唤一次专家团（`expert_actual_use`，`expertType=team`）。
    ExpertTeam,
    /// 固定召唤「腾讯轻量云专家」（`expert_actual_use`，id 固定）。
    Lighthouse,
    /// 使用一次模板（`agent_task_created_with_template`）。
    Template,
    /// 安装/体验一次技能（`skill_info`）。
    Skill,
    /// 创建一个自动化任务（`automated_task_create_suc`）。
    Automation,
    /// 创建设计画布任务（`wbx_design_canvas_task_create`）。
    Canvas,
    /// 使用灵感案例（`playbook_prompt_send`）。
    Playbook,
    /// 应用主题外观（`appearance_skin_apply`）。
    Skin,
}

/// 一个任务的上游判定口径。
///
/// `code` 是上游给的稳定标识，`target` 只在任务清单没给出时兜底（上游有时省略
/// `progress.target`）。`reward` 用于面板预估，不代表实发（以 claim 回执为准）。
#[derive(Debug, Clone)]
pub struct TaskSpec {
    pub code: &'static str,
    pub automation: Automation,
    pub target: u32,
    pub reward: u32,
}

/// 需要不同 `expert` id 才能累加进度的专家 id 池。
///
/// 上游按 `(eventCode, id)` 去重，所以同一 id 连发不会涨进度。这份列表来自参考
/// 实现 2026-09 实测校准（逐个验证过能推进 `expert_5`），本模块按 `idx` 取模轮转。
const EXPERT_ID_POOL: &[(&str, &str)] = &[
    ("ex_PZw8Gu81HfN4", "运维工程师"),
    ("ex_ROsDtJbzADFV", "产品经理"),
    ("ex_SMUnl0nJbPix", "UI设计师"),
    ("ex_ZTR062oVBOCW", "数据分析师"),
    ("ex_a3sSSFBy8qaC", "后端架构师"),
    ("ex_aG1kvKbq8lPx", "文案策划"),
    ("ex_al1vxtUOYQ10", "测试专家"),
    ("ex_cZfiyuET9UQP", "安全顾问"),
    ("ex_eggOvQuVP0hq", "算法工程师"),
    ("ex_hSwsQjkSKnkX", "前端工程师"),
    ("ex_mMbwwmFA9n9P", "项目管理专家"),
    ("ex_uAQE5POfk7Zh", "增长运营专家"),
    ("ex_uZzSAScSy7FZ", "行业研究员"),
    ("ex_LHywGrZOtG7G", "数据分析师"),
    ("ex_NX5C8GBciVed", "测试架构师"),
    ("ex_DdCsaoq4AtcO", "云端运维专家"),
    ("ex_KzqKQguubrNQ", "内容创作专家"),
    ("ex_2cvvUZQhDyeJ", "腾讯轻量云专家"),
];

/// 专家团队的 id 池（参考实现验证过前 3 个可推进 `Expert_team_use_3`）。
const TEAM_ID_POOL: &[(&str, &str)] = &[
    ("CloudOpsTeam", "运维专家团队"),
    ("CloudContentTeam", "内容专家团队"),
    ("CloudDevTeam", "研发专家团队"),
];

/// 任务分类表。
///
/// 与上游无关的静态知识：哪些任务能上报推进、各自要几次、给多少分。上游新增任务
/// 若不在表里，[`classify`] 会**失败关闭**归为不可自动化，而不是猜着跑。
pub fn task_specs() -> &'static [TaskSpec] {
    // `OnceLock` 而不是 `const`：`TaskSpec` 里含 `String`（DesktopOnly 的 hint），
    // 不能放进 `const` 数组字面量里按引用返回。
    static SPECS: std::sync::OnceLock<Vec<TaskSpec>> = std::sync::OnceLock::new();
    SPECS.get_or_init(|| {
        vec![
            TaskSpec {
                code: "chat_5",
                automation: Automation::Auto {
                    event: EventKind::Chat,
                },
                target: 5,
                reward: 100,
            },
            TaskSpec {
                code: "expert_5",
                automation: Automation::Auto {
                    event: EventKind::Expert,
                },
                target: 5,
                reward: 100,
            },
            TaskSpec {
                code: "Expert_team_use_3",
                automation: Automation::Auto {
                    event: EventKind::ExpertTeam,
                },
                target: 3,
                reward: 100,
            },
            TaskSpec {
                code: "template_5",
                automation: Automation::Auto {
                    event: EventKind::Template,
                },
                target: 5,
                reward: 100,
            },
            TaskSpec {
                code: "skill_1",
                automation: Automation::Auto {
                    event: EventKind::Skill,
                },
                target: 1,
                reward: 100,
            },
            TaskSpec {
                code: "automation_1",
                automation: Automation::Auto {
                    event: EventKind::Automation,
                },
                target: 1,
                reward: 100,
            },
            TaskSpec {
                code: "create_canvas",
                automation: Automation::Auto {
                    event: EventKind::Canvas,
                },
                target: 1,
                reward: 300,
            },
            TaskSpec {
                code: "playbook_prompt",
                automation: Automation::Auto {
                    event: EventKind::Playbook,
                },
                target: 1,
                reward: 100,
            },
            TaskSpec {
                code: "Hp_Appearance",
                automation: Automation::Auto {
                    event: EventKind::Skin,
                },
                target: 1,
                reward: 100,
            },
            TaskSpec {
                code: "Model_chat_GLM5.2",
                automation: Automation::Auto {
                    event: EventKind::GlmChat,
                },
                target: 1,
                reward: 100,
            },
            TaskSpec {
                code: "black_cat",
                automation: Automation::Auto {
                    event: EventKind::NightCat,
                },
                target: 3,
                reward: 100,
            },
            TaskSpec {
                code: "WorkBuddy_Valorant",
                automation: Automation::Auto {
                    event: EventKind::Expert,
                },
                target: 1,
                reward: 100,
            },
            // 轻量云专家：走 Expert 事件，但固定用该专家的 id（事件里 name/id 必须是
            // 真实专家，否则上游只接事件不推进）。
            TaskSpec {
                code: "Expert_lighthouse",
                automation: Automation::Auto {
                    event: EventKind::Lighthouse,
                },
                target: 1,
                reward: 100,
            },
            // first_buddy 是 `auto` 类：上游**不接受接取**，完成后领奖才发放 Buddy
            // 实例；而它是所有 single 任务与猫猫旅行的前置（不先领，接取会报
            // `prerequisite not met: first_buddy`）。因此编排层必须最先处理它。
            TaskSpec {
                code: "first_buddy",
                automation: Automation::Auto {
                    event: EventKind::Chat,
                },
                target: 1,
                reward: 300,
            },
            // 四个单项专家体验任务：各自 50 分，靠 Expert 事件推进。
            TaskSpec {
                code: "expert_use_maidehao",
                automation: Automation::Auto {
                    event: EventKind::Expert,
                },
                target: 1,
                reward: 50,
            },
            TaskSpec {
                code: "expert_use_douyinyin",
                automation: Automation::Auto {
                    event: EventKind::Expert,
                },
                target: 1,
                reward: 50,
            },
            TaskSpec {
                code: "expert_use_shangjia_xuanchuan",
                automation: Automation::Auto {
                    event: EventKind::Expert,
                },
                target: 1,
                reward: 50,
            },
            TaskSpec {
                code: "expert_use_bowangwang",
                automation: Automation::Auto {
                    event: EventKind::Expert,
                },
                target: 1,
                reward: 50,
            },
            // 只认桌面客户端真实行为：伪造上报会被忽略或落成 heartbeat。
            TaskSpec {
                code: "RichMeow_Chat",
                automation: Automation::DesktopOnly {
                    hint: "在桌面端发起 1 次对话".into(),
                },
                target: 1,
                reward: 100,
            },
            TaskSpec {
                code: "Library_read",
                automation: Automation::DesktopOnly {
                    hint: "在桌面端打开「资料库」并读完介绍文档".into(),
                },
                target: 1,
                reward: 100,
            },
            TaskSpec {
                code: "Buddy_App",
                automation: Automation::DesktopOnly {
                    hint: "在桌面端左上角「发现应用」进入任意一个 Buddy 应用".into(),
                },
                target: 1,
                reward: 100,
            },
            TaskSpec {
                code: "Buddy_App_QQ",
                automation: Automation::DesktopOnly {
                    hint: "在桌面端「发现应用」进入「企鹅教师助手」".into(),
                },
                target: 1,
                reward: 100,
            },
            // 物理上不可能自动化。
            TaskSpec {
                code: "wb_wechat_oa_subscribe_task",
                automation: Automation::Unforgeable {
                    reason: "需在微信关注「腾讯 WorkBuddy」公众号满 24 小时".into(),
                },
                target: 1,
                reward: 100,
            },
            TaskSpec {
                code: "Expert_Philanthropy",
                automation: Automation::Unforgeable {
                    reason: "需完成真实捐款动作".into(),
                },
                target: 1,
                reward: 0,
            },
        ]
    })
}

/// 查一个任务的分类；未知任务按不可自动化处理。
///
/// **失败关闭**是刻意的：上游新增任务时，猜它"大概能上报"会发出无效事件并可能在
/// 日志里报成功。归为不可自动化至多让用户发现"这任务没自动做"，而不会虚报收益。
pub fn classify(code: &str) -> Option<&'static TaskSpec> {
    task_specs().iter().find(|s| s.code == code)
}

/// 一个任务在本次运行中的快照。
#[derive(Debug, Clone, Serialize)]
pub struct GrowthTask {
    pub code: String,
    pub title: String,
    /// 上游回执的接取状态：`not_accepted` / `accepted` / `in_progress` /
    /// `completed` / `claimed`。
    pub status: String,
    /// 上游给的 `task_type`：`single` / `auto`。
    pub task_type: String,
    pub current: u32,
    pub target: u32,
    pub reward: u32,
    /// 分类结果；未知任务为 `None`（由调用方当作不可自动化）。
    pub automation: Option<Automation>,
    /// 该任务指定的专家 id（可空）。
    ///
    /// 上游的「体验某个特定专家」类任务**只认那一个专家的 id**：用别的专家
    /// id 上报，事件被接受、进度不动。id 藏在 `jump_url` 的 `expertId=` 参数里
    /// （形如 `workbuddy://experts?expertId=ex_xxx`），解析出来即可，无需硬编码。
    pub expert_id: Option<String>,
}

/// 从 `jump_url` 里取出 `expertId` 参数。
///
/// 上游把任务要求的专家 id 放在深链里，而不是单独字段。解析失败返回 `None`
/// 并由调用方回退到通用专家池——那样任务不会推进，所以 `None` 只应发生在
/// 上游改了链接格式时，需要人工看一眼。
fn expert_id_from_jump_url(url: &str) -> Option<String> {
    // The deep link is URL-encoded *inside* a query parameter, so the separator
    // arrives as `%3D` rather than `=`
    // (`...?deeplink=workbuddy%3A%2F%2Fexperts%3FexpertId%3Dex_GDl31WahyIH0`).
    // Matching only the literal `expertId=` finds nothing and silently falls
    // back to the generic pool — which the upstream accepts without advancing
    // the task, so the task looks permanently stuck at 0/1.
    //
    // Both spellings are handled: a plain `expertId=` (if upstream ever stops
    // encoding it) and the encoded `expertId%3D`.
    let after = url
        .split("expertId=")
        .nth(1)
        .or_else(|| url.split("expertId%3D").nth(1))?;
    let id = after.split(['&', '#', ' ', '%']).next()?.trim();
    if id.is_empty() {
        None
    } else {
        Some(id.to_string())
    }
}

impl GrowthTask {
    /// 是否已经领过奖（再做任何事都是浪费请求）。
    pub fn is_claimed(&self) -> bool {
        self.status == "claimed"
    }

    /// 还差几次上报。
    pub fn remaining(&self) -> u32 {
        self.target.saturating_sub(self.current)
    }

    /// 是否值得在本次运行中推进。
    ///
    /// 只有 [`Automation::Auto`] 且未领奖、未达标的才算；桌面专属与不可伪造的
    /// 任务永远返回 `false`，调用方据此把它们列进「需你手动完成」而不是执行。
    pub fn is_actionable(&self) -> bool {
        if self.is_claimed() {
            return false;
        }
        matches!(self.automation, Some(Automation::Auto { .. })) && self.remaining() > 0
    }

    /// 已达标但还没领奖。
    ///
    /// 和 [`Self::is_actionable`] 分开是因为两者要做的事不同：前者要"继续上报"，
    /// 后者要"直接领奖"。把它们合成一个布尔值，会让已达标的任务既不推进也不
    /// 领奖——积分就停在账号里，用户看着 `completed` 却拿不到分。
    pub fn is_claimable(&self) -> bool {
        !self.is_claimed()
            && matches!(self.automation, Some(Automation::Auto { .. }))
            && self.remaining() == 0
    }

    /// 本次运行是否应处理（推进或领奖）。
    pub fn needs_work(&self) -> bool {
        self.is_actionable() || self.is_claimable()
    }
}

/// `GET /v2/activity/growth/tasks` 的一条原始任务。
#[derive(Debug, Deserialize)]
struct RawTask {
    #[serde(default)]
    task_code: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    accept_status: String,
    #[serde(default)]
    task_type: String,
    #[serde(default)]
    reward_credit: u32,
    #[serde(default)]
    jump_url: String,
    #[serde(default)]
    progress: Option<RawProgress>,
}

#[derive(Debug, Default, Deserialize)]
struct RawProgress {
    #[serde(default)]
    current: u32,
    #[serde(default)]
    target: u32,
}

/// 上游统一信封。
#[derive(Debug, Deserialize)]
struct Envelope<T> {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    msg: String,
    data: Option<T>,
}

/// 成长接口的 HTTP 客户端。
pub struct GrowthClient<'a> {
    client: &'a reqwest::Client,
    endpoint: &'a str,
}

impl<'a> GrowthClient<'a> {
    pub fn new(client: &'a reqwest::Client, endpoint: Option<&'a str>) -> Self {
        Self {
            client,
            endpoint: endpoint
                .unwrap_or(DEFAULT_GROWTH_ENDPOINT)
                .trim_end_matches('/'),
        }
    }

    /// 组装一个请求。
    ///
    /// 只带成长体系需要的身份头：`Authorization` + `X-User-Id` + `X-Domain`。
    /// 刻意**不带** `X-IDE-*` 那套聊天指纹——成长接口不校验渠道，而带上它们会让
    /// 一次失败的请求看起来像聊天通道的 11-128，混淆两类错误。
    fn request(&self, cred: &WorkBuddyCredential, path: &str) -> reqwest::RequestBuilder {
        let mut req = self
            .client
            .post(format!("{}{}", self.endpoint, path))
            .timeout(std::time::Duration::from_secs(20))
            .header("User-Agent", crate::providers::WORKBUDDY_USER_AGENT)
            .header("X-Product-Code", "workbuddy")
            .header("Authorization", format!("Bearer {}", cred.access_token));
        if !cred.account.uid.is_empty() {
            req = req.header("X-User-Id", &cred.account.uid);
        }
        if !cred.domain.is_empty() {
            req = req.header("X-Domain", &cred.domain);
        }
        req
    }

    /// 拉取任务清单。
    pub async fn tasks(&self, cred: &WorkBuddyCredential) -> Result<Vec<GrowthTask>> {
        // 任务清单是 GET，与其它端点不同，单列一次。
        let mut req = self
            .client
            .get(format!("{}{}", self.endpoint, TASKS_PATH))
            .timeout(std::time::Duration::from_secs(20))
            .header("User-Agent", crate::providers::WORKBUDDY_USER_AGENT)
            .header("X-Product-Code", "workbuddy")
            .header("Authorization", format!("Bearer {}", cred.access_token));
        if !cred.account.uid.is_empty() {
            req = req.header("X-User-Id", &cred.account.uid);
        }
        if !cred.domain.is_empty() {
            req = req.header("X-Domain", &cred.domain);
        }
        let text = self.send(req, "拉取成长任务清单").await?;
        let env: Envelope<TasksData> =
            serde_json::from_str(&text).context("成长任务清单不是有效 JSON")?;
        if env.code != 0 {
            anyhow::bail!("成长任务清单被拒 (code={}): {}", env.code, env.msg);
        }
        let raw = env.data.map(|d| d.tasks).unwrap_or_default();

        Ok(raw
            .into_iter()
            .map(|t| {
                let spec = classify(&t.task_code);
                let current = t.progress.as_ref().map(|p| p.current).unwrap_or(0);
                let target = t
                    .progress
                    .as_ref()
                    .map(|p| p.target)
                    .filter(|n| *n > 0)
                    .or_else(|| spec.map(|s| s.target))
                    .unwrap_or(1);
                // 先算 expert_id 再 move `task_code`：`fixed_expert_for` 需要
                // 读它，而构造体里 `code` 会把它移走。
                let expert_id = expert_id_from_jump_url(&t.jump_url)
                    .or_else(|| fixed_expert_for(&t.task_code).map(str::to_string));
                GrowthTask {
                    code: t.task_code,
                    title: t.title,
                    status: t.accept_status,
                    task_type: t.task_type,
                    current,
                    target,
                    // 上游没给就用表里的，便于面板预估；真正到账以 claim 为准。
                    reward: if t.reward_credit > 0 {
                        t.reward_credit
                    } else {
                        spec.map(|s| s.reward).unwrap_or(0)
                    },
                    automation: spec.map(|s| s.automation.clone()),
                    expert_id,
                }
            })
            .collect())
    }

    /// 批量接取任务。
    ///
    /// 返回被接受与被拒的 code 列表。**被拒不等于失败**：`auto` 类任务上游本来
    /// 就不接受接取，把它们放进 `rejected` 是正常的。
    pub async fn accept(
        &self,
        cred: &WorkBuddyCredential,
        codes: &[String],
    ) -> Result<AcceptReport> {
        if codes.is_empty() {
            return Ok(AcceptReport::default());
        }
        let req = self
            .request(cred, ACCEPT_PATH)
            .json(&json!({ "task_codes": codes }));
        let text = self.send(req, "接取成长任务").await?;
        let env: Envelope<AcceptData> =
            serde_json::from_str(&text).context("接取回执不是有效 JSON")?;
        if env.code != 0 {
            anyhow::bail!("接取被拒 (code={}): {}", env.code, env.msg);
        }
        let mut report = AcceptReport::default();
        for r in env.data.map(|d| d.results).unwrap_or_default() {
            if r.status == "accepted" {
                report.accepted.push(r.task_code);
            } else {
                report.rejected.push(r.task_code);
            }
        }
        Ok(report)
    }

    /// 上报一批事件。
    ///
    /// `code != 0` 会返回错误；`code == 0` 只代表上游收下了，**不代表进度推进**
    /// （被当作 heartbeat 丢弃时也是 0）。真正确认要看下一次
    /// [`Self::tasks`] 的 `current` 是否变化，见 [`ProgressCheck`]。
    pub async fn report(&self, cred: &WorkBuddyCredential, events: &[Value]) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        let req = self.request(cred, REPORT_PATH).json(events);
        let text = self.send(req, "上报成长事件").await?;
        let env: Envelope<Value> =
            serde_json::from_str(&text).context("事件上报回执不是有效 JSON")?;
        if env.code != 0 {
            anyhow::bail!("事件上报被拒 (code={}): {}", env.code, env.msg);
        }
        Ok(())
    }

    /// 领取一个已完成任务的奖励。
    pub async fn claim(&self, cred: &WorkBuddyCredential, code: &str) -> Result<ClaimReport> {
        // 领奖路径把 task_code 放在路径里，且不在 `/v2` 名下——与接取不同。
        let path = format!("/activity/growth/tasks/{}/claim", code);
        let req = self.request(cred, &path);
        let text = self.send(req, "领取成长任务奖励").await?;
        let env: Envelope<ClaimData> =
            serde_json::from_str(&text).context("领奖回执不是有效 JSON")?;
        if env.code != 0 {
            anyhow::bail!("领奖被拒 (code={}): {}", env.code, env.msg);
        }
        let data = env.data.unwrap_or_default();
        Ok(ClaimReport {
            credit: data.credit,
            energy: data.energy,
            already_claimed: data.already_claimed,
        })
    }

    /// 查询能量（成长值）余额。
    pub async fn energy(&self, cred: &WorkBuddyCredential) -> Result<f64> {
        let mut req = self
            .client
            .get(format!("{}{}", self.endpoint, ENERGY_PATH))
            .timeout(std::time::Duration::from_secs(15))
            .header("User-Agent", crate::providers::WORKBUDDY_USER_AGENT)
            .header("X-Product-Code", "workbuddy")
            .header("Authorization", format!("Bearer {}", cred.access_token));
        if !cred.account.uid.is_empty() {
            req = req.header("X-User-Id", &cred.account.uid);
        }
        if !cred.domain.is_empty() {
            req = req.header("X-Domain", &cred.domain);
        }
        let text = self.send(req, "查询成长能量").await?;
        let env: Envelope<EnergyData> =
            serde_json::from_str(&text).context("能量回执不是有效 JSON")?;
        Ok(env.data.map(|d| d.balance).unwrap_or(0.0))
    }

    /// 发出请求并把非 2xx 变成可读错误（带原始 body 片段）。
    async fn send(&self, req: reqwest::RequestBuilder, what: &str) -> Result<String> {
        let resp = req
            .send()
            .await
            .with_context(|| format!("{what}请求失败"))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!(
                "{what}返回 {}: {}",
                status,
                crate::util::truncate(&text, 300)
            );
        }
        Ok(text)
    }
}

#[derive(Debug, Deserialize)]
struct TasksData {
    #[serde(default)]
    tasks: Vec<RawTask>,
}

#[derive(Debug, Deserialize)]
struct AcceptData {
    #[serde(default)]
    results: Vec<AcceptResult>,
}

#[derive(Debug, Deserialize)]
struct AcceptResult {
    #[serde(default)]
    task_code: String,
    #[serde(default)]
    status: String,
}

#[derive(Debug, Default, Deserialize)]
struct ClaimData {
    #[serde(default)]
    credit: f64,
    #[serde(default)]
    energy: f64,
    #[serde(default)]
    already_claimed: bool,
}

#[derive(Debug, Default, Deserialize)]
struct EnergyData {
    #[serde(default)]
    balance: f64,
}

/// 接取结果。
#[derive(Debug, Default, Clone, Serialize)]
pub struct AcceptReport {
    pub accepted: Vec<String>,
    pub rejected: Vec<String>,
}

/// 领奖结果。
#[derive(Debug, Default, Clone, Serialize)]
pub struct ClaimReport {
    pub credit: f64,
    pub energy: f64,
    pub already_claimed: bool,
}

/// 任务固定绑定的专家 id（当 `jump_url` 里没有 `expertId` 时使用）。
///
/// `WorkBuddy_Valorant` 的深链只有 `workbuddy://expert`，不带 id，但任务标题
/// 指明要哪个专家。这个 id 取自参考实现 2026-09 实测校准，本模块 2026-10-25
/// 复测确认可把进度从 0/1 推到 1/1。
fn fixed_expert_for(task_code: &str) -> Option<&'static str> {
    match task_code {
        "WorkBuddy_Valorant" => Some("ex_xQHHD1u5Jkua"),
        _ => None,
    }
}

/// 任务指定专家 id 时，用它的中文名（事件里 `name` 必须与 id 对应，否则上游
/// 可能只收事件不推进）。已知的取自 `jump_url` 对应任务的标题。
fn expert_name_for(id: &str) -> Option<&'static str> {
    match id {
        "ex_GDl31WahyIH0" => Some("中国电商运营专家"),
        "ex_FbY5nlWTqDod" => Some("抖音策略师"),
        "ex_Weo3x2EZuSDm" => Some("商家宣传页与海报设计师"),
        "ex_WZY136wV5aMt" => Some("直播电商教练"),
        "ex_xQHHD1u5Jkua" => Some("无畏契约拼豆专家"),
        _ => None,
    }
}

/// 某一类事件用哪个 id 池。
fn id_pool(kind: EventKind) -> &'static [(&'static str, &'static str)] {
    match kind {
        EventKind::Expert => EXPERT_ID_POOL,
        EventKind::ExpertTeam => TEAM_ID_POOL,
        // 其它 kind 不用 id 池（靠 conversationId/requestId 唯一即可）。
        _ => &[],
    }
}

/// 生成一个事件。
///
/// `idx` 决定取哪个 id（专家/团队池按模轮转）并参与 `conversationId` 的构造，
/// 保证同一任务的多条事件**互不相同**——上游按 `(eventCode, id)` 去重，重复的
/// 事件不会累加进度，而 `code: 0` 会让调用方误以为成功了。
///
/// `uid` 必须非空：上游缺少 `userId` 时会静默丢弃整条事件。
pub fn build_event(kind: EventKind, uid: &str, idx: u32) -> Value {
    build_event_with_expert(kind, uid, idx, None)
}

/// 同 [`build_event`]，但允许指定专家 id。
///
/// 用于「体验指定专家」类任务：上游只认那一个 id，用通用池里的 id 上报会让
/// 进度停在 0（实测确认）。`explicit` 为 `None` 时行为与 [`build_event`] 一致。
pub fn build_event_with_expert(
    kind: EventKind,
    uid: &str,
    idx: u32,
    explicit: Option<&str>,
) -> Value {
    // 时间戳参与 id，因此跨秒必然不同；同一秒内靠 `idx` 区分。
    let now = crate::util::unix_millis();
    let conv = format!("wb-growth-{now}-{idx}");
    let req = format!("{conv}-req");

    match kind {
        EventKind::Chat | EventKind::GlmChat | EventKind::NightCat => {
            let (model_id, model_name) = match kind {
                EventKind::GlmChat | EventKind::NightCat => ("glm-5.2", "GLM-5.2"),
                _ => ("deepseek-v4-flash", "DeepSeek V4 Flash"),
            };
            json!({
                "eventCode": "chat_request_send",
                "timestamp": now,
                "reportDelay": 0,
                // 夜猫子任务要求 mode=night，其余用 craft。
                "mode": if kind == EventKind::NightCat { "night" } else { "craft" },
                "conversationId": conv,
                "requestId": req,
                "inputLength": 12,
                "requestModelId": model_id,
                "requestModelName": model_name,
                "isPlan": false,
                "agentName": "default",
                "agentType": "conversation",
                "userId": uid,
            })
        }
        // 轻量云专家用固定 id（上游按 (eventCode, id) 去重，但该任务只要求 1 次，
        // 不需要轮转）。
        EventKind::Lighthouse => json!({
            "eventCode": "expert_actual_use",
            "timestamp": now,
            "reportDelay": 0,
            "mode": "CLOUD",
            "id": "ex_2cvvUZQhDyeJ",
            "name": "腾讯轻量云专家",
            "expertTitle": "腾讯轻量云专家",
            "type": "02-Engineering",
            "expertType": "agent",
            "source": "builtin",
            "version": "1.0.2",
            "cost": 0,
            "characterCount": 12,
            "conversationId": conv,
            "requestId": req,
            "messageId": req,
            "requestModelId": "deepseek-v4-flash",
            "requestModelName": "DeepSeek V4 Flash",
            "userId": uid,
        }),
        EventKind::Expert | EventKind::ExpertTeam => {
            let pool = id_pool(kind);
            // 指定了 id 就必须用它：任务只认那一个专家。
            let (id, name) = match explicit {
                Some(id) => (id, expert_name_for(id).unwrap_or("专家")),
                None => pool
                    .get((idx as usize) % pool.len().max(1))
                    .copied()
                    .unwrap_or(("ContentCreator", "内容创作专家")),
            };
            json!({
                "eventCode": "expert_actual_use",
                "timestamp": now,
                "reportDelay": 0,
                "mode": "CLOUD",
                "id": id,
                "name": name,
                "expertTitle": name,
                "type": "02-Engineering",
                "expertType": if kind == EventKind::ExpertTeam { "team" } else { "agent" },
                "source": "builtin",
                "version": "1.0.2",
                "cost": 0,
                "characterCount": 12,
                "conversationId": conv,
                "requestId": req,
                "messageId": req,
                "requestModelId": "deepseek-v4-flash",
                "requestModelName": "DeepSeek V4 Flash",
                "userId": uid,
            })
        }
        EventKind::Template => json!({
            "eventCode": "agent_task_created_with_template",
            "timestamp": now,
            "reportDelay": 0,
            "isCustomModel": true,
            // 模板任务的 id 字段是模板序号，取 idx 让每次不同。
            "id": idx.to_string(),
            "name": "幻灯片",
            "requestId": req,
            "conversationId": conv,
            "userId": uid,
        }),
        EventKind::Skill => json!({
            "eventCode": "skill_info",
            "timestamp": now,
            "reportDelay": 0,
            "skillId": "skill_2096525080079265792",
            "name": "pptx",
            "userId": uid,
        }),
        EventKind::Automation => json!({
            "eventCode": "automated_task_create_suc",
            "timestamp": now,
            "reportDelay": 0,
            "name": "每周工作整理",
            "type": "cron",
            "source": "manually",
            "modelId": "deepseek-v4-flash",
            "modelIsThinking": false,
            "conversationId": conv,
            "requestId": req,
            "schedule": {
                "type": "recurring",
                "rrule": "FREQ=WEEKLY;BYDAY=FR;BYHOUR=9;BYMINUTE=0"
            },
            "prompt": "每周五自动整理本周工作",
            "userId": uid,
        }),
        EventKind::Canvas => json!({
            "eventCode": "wbx_design_canvas_task_create",
            "timestamp": now,
            "reportDelay": 0,
            "conversationId": conv,
            "requestId": req,
            "source": "summon_keyword",
            "isCustomModel": false,
            "name": "",
            "inputLength": 12,
            "id": format!("wbx-canvas-{now}"),
            "cost": 0,
            "isSuccessful": true,
            "userId": uid,
        }),
        EventKind::Playbook => json!({
            "eventCode": "playbook_prompt_send",
            "timestamp": now,
            "reportDelay": 0,
            "id": "worker-ledger-freedom-dashboard",
            "name": "打工人小账本",
            "type": "other",
            "promptLength": 10,
            "isOfficial": 1,
            "source": "discover",
            "conversationId": conv,
            "requestId": req,
            "userId": uid,
        }),
        EventKind::Skin => json!({
            "eventCode": "appearance_skin_apply",
            "timestamp": now,
            "reportDelay": 0,
            "action": "apply",
            "source": "settings_close",
            "id": "theme-tkmw7j",
            "vipLevel": "free",
            "series": "craft",
            "type": "unknown",
            "name": "和平精英激战金秋",
            "userId": uid,
        }),
    }
}

/// 夜猫子任务的生效窗口判断（23:00–08:00，含跨零点）。
///
/// 单独导出便于编排层在非窗口期跳过该任务——在白天上报 `mode=night` 事件上游
/// 不计数，白跑一次还多一分风控暴露。
// `(8..23)` 的补集写法（`!(8..23).contains(&hour)`）虽然能让 clippy 满意，却把
// 「23 点到次日 8 点」这个业务窗口写成了一个需要心算补集的表达式。这里保留直白
// 比较，并用 allow 说明原因。
#[allow(clippy::manual_range_contains)]
pub fn in_night_window(hour: u32) -> bool {
    hour >= 23 || hour < 8
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 分类表里每个 code 必须唯一，否则 `classify` 会静默返回第一个。
    #[test]
    fn task_codes_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for spec in task_specs() {
            assert!(seen.insert(spec.code), "重复的 task code: {}", spec.code);
        }
    }

    /// 能上报的任务必须给出事件类型，且必须是 `Auto`。
    #[test]
    fn every_actionable_spec_names_an_event() {
        for spec in task_specs() {
            if let Automation::Auto { .. } = spec.automation {
                assert!(spec.target >= 1, "{} target 必须 >= 1", spec.code);
            } else {
                // 不可自动化的任务不该有 target 语义，但保留 1 便于展示。
                assert_eq!(spec.target, 1, "{} 不该有多次目标", spec.code);
            }
        }
    }

    /// 桌面专属与不可伪造的任务**不能**被 `is_actionable` 放行。
    ///
    /// 这是本模块最重要的一条守卫：放行它们会发出被忽略的事件并报告成功。
    #[test]
    fn unforgeable_and_desktop_tasks_are_never_actionable() {
        for spec in task_specs() {
            if matches!(spec.automation, Automation::Auto { .. }) {
                continue;
            }
            let task = GrowthTask {
                code: spec.code.to_string(),
                title: spec.code.to_string(),
                status: "accepted".to_string(),
                task_type: "single".to_string(),
                current: 0,
                target: spec.target,
                reward: spec.reward,
                automation: Some(spec.automation.clone()),
                expert_id: None,
            };
            assert!(
                !task.is_actionable(),
                "{} 是 {:?}，不应被当作可执行",
                spec.code,
                spec.automation
            );
        }
    }

    #[test]
    fn claimed_tasks_are_not_actionable() {
        let task = GrowthTask {
            code: "chat_5".to_string(),
            title: "发起 5 次对话".to_string(),
            status: "claimed".to_string(),
            task_type: "single".to_string(),
            current: 5,
            target: 5,
            reward: 100,
            automation: Some(Automation::Auto {
                event: EventKind::Chat,
            }),
            expert_id: None,
        };
        assert!(!task.is_actionable());
        assert!(task.is_claimed());
    }

    #[test]
    fn remaining_never_underflows_when_current_exceeds_target() {
        // 上游有时把 current 报得比 target 大（历史数据），不该 panic。
        let task = GrowthTask {
            code: "chat_5".to_string(),
            title: String::new(),
            status: "completed".to_string(),
            task_type: "single".to_string(),
            current: 7,
            target: 5,
            reward: 100,
            automation: Some(Automation::Auto {
                event: EventKind::Chat,
            }),
            expert_id: None,
        };
        assert_eq!(task.remaining(), 0);
        assert!(!task.is_actionable());
    }

    /// 未知任务必须失败关闭：不进可执行集合。
    #[test]
    fn unknown_tasks_classify_to_none() {
        assert!(classify("some_future_task").is_none());
        let task = GrowthTask {
            code: "some_future_task".to_string(),
            title: String::new(),
            status: "accepted".to_string(),
            task_type: "single".to_string(),
            current: 0,
            target: 1,
            reward: 0,
            automation: None,
            expert_id: None,
        };
        assert!(!task.is_actionable());
    }

    /// `first_buddy` 必须被识别为可自动化，且必须是 `auto` 类的前置。
    ///
    /// 它在上游不接受接取（`auto` 类），但一旦领到，后续 single 任务才能接取——
    /// 漏掉它会让整轮任务全部卡在 `prerequisite not met: first_buddy`。
    #[test]
    fn first_buddy_is_auto_and_classified() {
        let spec = classify("first_buddy").expect("first_buddy 必须分类，它是其他任务的前置");
        assert!(matches!(spec.automation, Automation::Auto { .. }));
        assert_eq!(spec.reward, 300);
    }

    /// 轻量云专家用固定 id，走 Expert 事件。
    #[test]
    fn lighthouse_uses_the_fixed_expert_id() {
        let spec = classify("Expert_lighthouse").expect("Expert_lighthouse 必须分类");
        assert!(matches!(spec.automation, Automation::Auto { .. }));
        let ev = build_event(EventKind::Lighthouse, "u", 0);
        assert_eq!(
            ev.get("id").and_then(Value::as_str),
            Some("ex_2cvvUZQhDyeJ"),
            "轻量云专家必须用真实专家 id，否则上游只收事件不推进"
        );
        assert_eq!(
            ev.get("eventCode").and_then(Value::as_str),
            Some("expert_actual_use")
        );
    }

    /// 真实账号的完整清单必须**全部**有分类。
    ///
    /// 这份 code 列表来自 2026-10-25 对 `libai` 账号的实测回执。新增任务若没进
    /// 分类表，会在运行时被失败关闭跳过；这条测试让遗漏在 CI 就暴露，而不是等到
    /// 用户发现"某个任务一直没自动做"。
    #[test]
    fn every_known_live_task_is_classified() {
        let live = [
            "create_canvas",
            "playbook_prompt",
            "RichMeow_Chat",
            "Library_read",
            "Expert_lighthouse",
            "Hp_Appearance",
            "Buddy_App",
            "Buddy_App_QQ",
            "wb_wechat_oa_subscribe_task",
            "WorkBuddy_Valorant",
            "expert_use_maidehao",
            "expert_use_douyinyin",
            "expert_use_shangjia_xuanchuan",
            "expert_use_bowangwang",
            "Expert_team_use_3",
            "first_buddy",
            "chat_5",
            "skill_1",
            "expert_5",
            "template_5",
            "automation_1",
        ];
        let missing: Vec<&str> = live
            .iter()
            .filter(|c| classify(c).is_none())
            .copied()
            .collect();
        assert!(
            missing.is_empty(),
            "实测清单里这些任务没有分类: {missing:?}"
        );
    }

    /// 深链里的 `expertId` 是 URL 编码的，必须解析出来。
    ///
    /// 上游把深链塞进 `deeplink=` 参数，于是 `expertId=` 变成了
    /// `expertId%3D`。只匹配字面 `=` 会解析失败并静默回退到通用专家池——
    /// 上游接受那种事件但不推进进度，任务会永远停在 0/1。
    #[test]
    fn expert_id_is_parsed_from_a_url_encoded_deeplink() {
        let live = "https://www.workbuddy.cn/work/launch/?deeplink=workbuddy%3A%2F%2Fexperts%3FexpertId%3Dex_GDl31WahyIH0&buddy_type=workbuddy";
        assert_eq!(
            expert_id_from_jump_url(live).as_deref(),
            Some("ex_GDl31WahyIH0"),
            "URL 编码的 expertId 必须能解析出来"
        );
    }

    /// 未编码的写法也要支持（上游若改回普通 query 不该失效）。
    #[test]
    fn expert_id_is_parsed_from_a_plain_deeplink() {
        assert_eq!(
            expert_id_from_jump_url("workbuddy://experts?expertId=ex_ABC123").as_deref(),
            Some("ex_ABC123")
        );
    }

    /// 没有 expertId 的深链返回 `None`，由调用方回退到通用池（如
    /// `WorkBuddy_Valorant` 的 `workbuddy://expert`）。
    #[test]
    fn missing_expert_id_is_none_not_empty() {
        assert_eq!(expert_id_from_jump_url("workbuddy://expert"), None);
        assert_eq!(expert_id_from_jump_url(""), None);
        assert_eq!(expert_id_from_jump_url("x?expertId="), None);
    }

    /// 指定了 expert id 的事件必须用它，而不是池里的。
    #[test]
    fn explicit_expert_id_overrides_the_pool() {
        let ev = build_event_with_expert(EventKind::Expert, "u", 0, Some("ex_GDl31WahyIH0"));
        assert_eq!(
            ev.get("id").and_then(Value::as_str),
            Some("ex_GDl31WahyIH0")
        );
        // 名称要与 id 对应，否则上游可能只收事件不推进。
        assert_eq!(
            ev.get("name").and_then(Value::as_str),
            Some("中国电商运营专家")
        );
    }

    /// 夜猫子窗口跨零点。
    #[test]
    fn night_window_wraps_past_midnight() {
        assert!(in_night_window(23));
        assert!(in_night_window(0));
        assert!(in_night_window(7));
        assert!(!in_night_window(8));
        assert!(!in_night_window(12));
        assert!(!in_night_window(22));
    }

    /// 事件必须带 `userId`——缺了它上游 200 但静默丢弃。
    #[test]
    fn every_event_carries_the_user_id() {
        for kind in [
            EventKind::Chat,
            EventKind::GlmChat,
            EventKind::NightCat,
            EventKind::Expert,
            EventKind::ExpertTeam,
            EventKind::Template,
            EventKind::Skill,
            EventKind::Automation,
            EventKind::Canvas,
            EventKind::Playbook,
            EventKind::Skin,
        ] {
            let ev = build_event(kind, "uid-123", 0);
            assert_eq!(
                ev.get("userId").and_then(Value::as_str),
                Some("uid-123"),
                "{kind:?} 缺少 userId"
            );
            assert!(
                ev.get("eventCode").and_then(Value::as_str).is_some(),
                "{kind:?} 缺少 eventCode"
            );
        }
    }

    /// 同一次运行内多次生成的事件必须互不相同。
    ///
    /// 上游按 `(eventCode, id)` 去重：两条完全相同的事件只算一次进度，而两次上报
    /// 都返回 `code: 0`。这会让「target=5 的任务」永远停在 1/5。
    #[test]
    fn repeated_events_for_one_task_are_distinct() {
        for kind in [EventKind::Expert, EventKind::ExpertTeam, EventKind::Chat] {
            let a = build_event(kind, "uid", 0);
            let b = build_event(kind, "uid", 1);
            assert_ne!(a, b, "{kind:?} 的 idx=0 与 idx=1 事件相同，会被去重");

            if matches!(kind, EventKind::Expert | EventKind::ExpertTeam) {
                assert_ne!(
                    a.get("id").and_then(Value::as_str),
                    b.get("id").and_then(Value::as_str),
                    "{kind:?} 必须轮转 id 才能累加进度"
                );
            }
        }
    }

    /// 专家池与团队池必须非空，否则 `build_event` 会落到兜底 id 且永远不推进。
    #[test]
    fn id_pools_are_populated() {
        assert!(EXPERT_ID_POOL.len() >= 5, "专家池太小，跑不满 expert_5");
        assert!(
            TEAM_ID_POOL.len() >= 3,
            "团队池太小，跑不满 Expert_team_use_3"
        );
        // 池内 id 不能重复，否则轮转会再次撞上去重。
        let mut seen = std::collections::HashSet::new();
        for (id, _) in EXPERT_ID_POOL {
            assert!(seen.insert(*id), "专家池重复 id: {id}");
        }
    }

    /// 夜猫子事件必须用 `mode=night`，其余对话用 `craft`。
    #[test]
    fn night_cat_uses_night_mode() {
        let cat = build_event(EventKind::NightCat, "u", 0);
        assert_eq!(cat.get("mode").and_then(Value::as_str), Some("night"));
        let chat = build_event(EventKind::Chat, "u", 0);
        assert_eq!(chat.get("mode").and_then(Value::as_str), Some("craft"));
    }

    /// 团队事件必须标 `expertType=team`，专家标 `agent`。
    #[test]
    fn team_events_are_marked_as_teams() {
        let team = build_event(EventKind::ExpertTeam, "u", 0);
        assert_eq!(team.get("expertType").and_then(Value::as_str), Some("team"));
        let expert = build_event(EventKind::Expert, "u", 0);
        assert_eq!(
            expert.get("expertType").and_then(Value::as_str),
            Some("agent")
        );
    }
}
