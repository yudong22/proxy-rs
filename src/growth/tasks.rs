//! The task catalogue: what each task is, which capability tier it needs, and how it is classified.

use super::*;

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
pub(crate) const EXPERT_ID_POOL: &[(&str, &str)] = &[
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
pub(crate) const TEAM_ID_POOL: &[(&str, &str)] = &[
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
pub(crate) fn expert_id_from_jump_url(url: &str) -> Option<String> {
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
        // 未接取的任务不该上报：上游还没为它建立进度记录，事件会被忽略，
        // 表现为"上报了但进度不动"。接取是前一步，必须在 `accept` 之后才推。
        // `auto` 类例外——它们本就不接受接取，直接上报即可。
        if self.task_type != "auto" && self.status == "not_accepted" {
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

/// 任务固定绑定的专家 id（当 `jump_url` 里没有 `expertId` 时使用）。
///
/// `WorkBuddy_Valorant` 的深链只有 `workbuddy://expert`，不带 id，但任务标题
/// 指明要哪个专家。这个 id 取自参考实现 2026-09 实测校准，本模块 2026-10-25
/// 复测确认可把进度从 0/1 推到 1/1。
pub(crate) fn fixed_expert_for(task_code: &str) -> Option<&'static str> {
    match task_code {
        "WorkBuddy_Valorant" => Some("ex_xQHHD1u5Jkua"),
        _ => None,
    }
}

/// 任务指定专家 id 时，用它的中文名（事件里 `name` 必须与 id 对应，否则上游
/// 可能只收事件不推进）。已知的取自 `jump_url` 对应任务的标题。
pub(crate) fn expert_name_for(id: &str) -> Option<&'static str> {
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
pub(crate) fn id_pool(kind: EventKind) -> &'static [(&'static str, &'static str)] {
    match kind {
        EventKind::Expert => EXPERT_ID_POOL,
        EventKind::ExpertTeam => TEAM_ID_POOL,
        // 其它 kind 不用 id 池（靠 conversationId/requestId 唯一即可）。
        _ => &[],
    }
}

#[allow(clippy::manual_range_contains)]
pub fn in_night_window(hour: u32) -> bool {
    hour >= 23 || hour < 8
}
