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
//! ## File layout
//!
//! The module note above draws one line — protocol vs orchestration — and this
//! file layout follows it. `growth_runner` is the orchestration side; these three
//! files are the protocol side, split further so the *catalogue* (which changes
//! when the vendor ships a new task) is separate from the *transport* (which
//! changes when the wire format does):
//!
//! | file | owns |
//! |---|---|
//! | [`protocol`] | paths, envelopes, response shapes, the HTTP client |
//! | [`tasks`] | the task catalogue, capability tiers, classification, id pools |
//! | [`events`] | event payload construction and the `(eventCode, id)` dedupe rule |
//!
//! `task_specs()` is deliberately data, not code: it is the table that has to be
//! re-read every time the vendor adds a task, and keeping it in its own file
//! means that edit never touches the transport.

use crate::workbuddy_auth::WorkBuddyCredential;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

mod events;
mod protocol;
mod tasks;

pub use events::*;
pub use protocol::*;
pub use tasks::*;

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
