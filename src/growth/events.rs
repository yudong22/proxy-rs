//! Event payload construction (the `(eventCode, id)` dedupe rule lives here).

use super::*;

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
