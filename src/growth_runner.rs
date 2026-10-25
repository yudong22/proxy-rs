//! 成长任务编排层。
//!
//! [`crate::growth`] 只懂协议，本模块负责**策略**：跑哪些任务、每个跑几次、
//! 什么时候放弃、失败如何记录。两者分开的理由写在那边的模块注释里。
//!
//! # 一次运行的形状
//!
//! 1. 预检：跳过企业号 / 已被上游风控的账号
//! 2. 拉任务清单
//! 3. **先处理 `auto` 类任务**（`first_buddy` 是其余任务的前置，不先领奖会让
//!    所有 `single` 任务接取报 `prerequisite not met`）
//! 4. 批量接取剩余的 `single` 任务，并复核（上游会瞬时拒绝整批）
//! 5. 逐任务上报事件 → **复核进度** → 达标则领奖
//! 6. 汇总
//!
//! # 为什么每一步都要复核进度
//!
//! 上游对事件上报的回应是 `code: 0`，**即使它把事件当 heartbeat 丢掉了**。
//! 国际版实测就是这样：上报成功、进度不动。因此本模块不信 `code: 0`，而是在
//! 上报后重拉清单，只有 `current` 真的变了才算推进。连续两次无变化即放弃该任务
//! 并记「上游未推进」，绝不把没到手的积分报成成功。

use crate::growth::{Automation, EventKind, GrowthClient, GrowthTask};
use crate::workbuddy_auth::WorkBuddyCredential;
use serde::Serialize;
use std::collections::HashMap;

/// 单个账号一次运行的上限时长。
///
/// 账号多、任务多时，一次全量运行可能几十秒；给一个明确上限是为了让调度器
/// 的 tick 不会被一个卡住的账号拖住，并保证失败时已完成的部分仍然有效。
const DEFAULT_BUDGET_MS: i64 = 120_000;

/// 两次上报之间的最小间隔。
///
/// 成长接口不像聊天请求那样有明确的频控文档，但同设备短时间高频访问是风控的
/// 典型触发条件（参考实现为此在账号之间还加了 2–5 秒随机错峰）。1 秒是一个
/// 保守值：跑满 5 次的任务多花 4 秒，换来的是一条相对安全的节奏。
const REPORT_GAP_MS: i64 = 1_000;

/// 一个任务为什么没有自动化。
#[derive(Debug, Clone, Serialize)]
pub struct SkippedTask {
    pub code: String,
    pub title: String,
    /// 给用户看的原因（不可伪造的说明 / 桌面端操作指引）。
    pub reason: String,
    /// 若能给出可点击入口则附上。
    pub hint: Option<String>,
    pub reward: u32,
}

/// 一个已完成并领奖的任务。
#[derive(Debug, Clone, Serialize)]
pub struct ClaimedTask {
    pub code: String,
    pub title: String,
    pub credit: f64,
    pub energy: f64,
}

/// 单账号一次运行的完整结果。
#[derive(Debug, Clone, Default, Serialize)]
pub struct AccountGrowthRun {
    pub credential_id: String,
    pub label: String,
    /// 被接受接取的任务数。
    pub accepted: usize,
    /// 达到目标并成功领奖的任务。
    pub claimed: Vec<ClaimedTask>,
    /// 本次实际拿到的积分（`claimed` 的 credit 之和）。
    pub credits_earned: f64,
    /// 直接跳过的任务（桌面专属 / 不可伪造 / 未识别）。
    pub skipped: Vec<SkippedTask>,
    /// 上报了但进度没动 —— 上游可能已加反自动化校验。
    pub stalled: Vec<String>,
    /// 逐条错误（网络、领奖 400 等），保留原文便于诊断。
    pub errors: Vec<String>,
    /// 运行前的能量余额，用于事后对账。
    pub energy_before: Option<f64>,
    /// 运行后的能量余额。
    pub energy_after: Option<f64>,
    /// 该账号是否被判定为上游风控受限，后续应跳过一段时间。
    pub restricted: bool,
}

impl AccountGrowthRun {
    /// 是否有值得一提的产出（领到奖或推进了任务）。
    pub fn did_something(&self) -> bool {
        !self.claimed.is_empty()
    }
}

/// 一次全池运行的结果。
#[derive(Debug, Clone, Default, Serialize)]
pub struct GrowthRunReport {
    pub accounts: Vec<AccountGrowthRun>,
    pub total_credits: f64,
    pub total_claimed: usize,
    /// 参与运行的账号数（跳过的不算）。
    pub total_accounts: usize,
    /// 跳过的账号及原因（企业号、风控中等）。
    pub skipped_accounts: Vec<(String, String)>,
}

/// 是否命中上游的账号级风控。
///
/// 上游用 `403` 或业务码 `11140` 表示"这个账号在这台设备上被限制了"。遇到它
/// 必须**立即停手并记住**：继续试探只会让同设备上的其他账号连坐（参考实现
/// 为此实现了跨重启的受限标记）。
fn is_restricted_error(err: &str) -> bool {
    err.contains("403") || err.contains("11140")
}

/// 跑一个账号的成长任务。
///
/// `only` 非空时只处理其中的任务 code（用于夜间 `black_cat` 单独批次）；
/// `dry_run` 只读、不接取不领奖，供面板预览。
pub async fn run_account(
    client: &reqwest::Client,
    cred: &WorkBuddyCredential,
    only: Option<&[&str]>,
    dry_run: bool,
) -> AccountGrowthRun {
    let mut run = AccountGrowthRun {
        credential_id: cred.id.clone(),
        label: crate::workbuddy_auth::display_label(cred),
        ..Default::default()
    };

    // 企业号没有个人成长体系，上游会直接拒。提前跳过省一轮请求。
    if !cred.account.enterprise_id.is_empty() {
        run.skipped.push(SkippedTask {
            code: "(account)".into(),
            title: String::new(),
            reason: "企业账号没有个人成长体系，无法参与".into(),
            hint: None,
            reward: 0,
        });
        return run;
    }

    let growth = GrowthClient::new(client, None);
    let started = crate::util::unix_millis();

    let mut tasks = match growth.tasks(cred).await {
        Ok(t) => t,
        Err(e) => {
            let msg = e.to_string();
            run.restricted = is_restricted_error(&msg);
            run.errors.push(format!("拉取任务清单失败: {msg}"));
            return run;
        }
    };

    run.energy_before = growth.energy(cred).await.ok();

    // 只跑指定任务的批次（夜间 black_cat）：过滤后其余一律不看。
    if let Some(only) = only {
        tasks.retain(|t| only.contains(&t.code.as_str()));
    }

    // ── 1. auto 类先处理 ──────────────────────────────────────────────
    // `first_buddy` 属于这一类，且是其余任务的前置：不先领到它，后面所有
    // single 任务接取都会失败。它们也不接受接取，直接上报即可。
    for task in tasks.iter().filter(|t| t.task_type == "auto") {
        if dry_run || !task.is_actionable() {
            continue;
        }
        advance_task(&growth, cred, task, &mut run, dry_run).await;
        if run.restricted {
            break;
        }
    }

    // ── 2. 批量接取 ──────────────────────────────────────────────────
    if !dry_run && !run.restricted {
        let to_accept: Vec<String> = tasks
            .iter()
            .filter(|t| {
                t.status == "not_accepted"
                    && t.task_type != "auto"
                    && matches!(t.automation, Some(Automation::Auto { .. }))
            })
            .map(|t| t.code.clone())
            .collect();

        if !to_accept.is_empty() {
            match growth.accept(cred, &to_accept).await {
                Ok(report) => {
                    run.accepted = report.accepted.len();
                    if !report.rejected.is_empty() {
                        // 被拒不一定是错误：`auto` 类本来就不接受接取。记录但不报错。
                        run.errors.push(format!("接取被拒: {:?}", report.rejected));
                    }
                }
                Err(e) => {
                    let msg = e.to_string();
                    run.restricted = is_restricted_error(&msg);
                    run.errors.push(format!("接取失败: {msg}"));
                }
            }
            // 复核：上游偶尔瞬时拒绝整批，重取一次才能拿到真实状态。
            if let Ok(fresh) = growth.tasks(cred).await {
                tasks = fresh;
                if let Some(only) = only {
                    tasks.retain(|t| only.contains(&t.code.as_str()));
                }
            }
        }
    }

    // ── 3. 逐任务推进 ────────────────────────────────────────────────
    for task in tasks.iter() {
        if run.restricted || crate::util::unix_millis() - started > DEFAULT_BUDGET_MS {
            break;
        }
        if task.task_type == "auto" {
            continue; // 已在第 1 步处理
        }
        match &task.automation {
            Some(Automation::Auto { .. }) => {}
            Some(Automation::DesktopOnly { hint }) => {
                run.skipped.push(SkippedTask {
                    code: task.code.clone(),
                    title: task.title.clone(),
                    reason: "只认桌面客户端的真实操作，程序化上报无效".into(),
                    hint: Some(hint.clone()),
                    reward: task.reward,
                });
                continue;
            }
            Some(Automation::Unforgeable { reason }) => {
                run.skipped.push(SkippedTask {
                    code: task.code.clone(),
                    title: task.title.clone(),
                    reason: reason.clone(),
                    hint: None,
                    reward: task.reward,
                });
                continue;
            }
            None => {
                // 未识别 = 失败关闭。不猜它能不能跑。
                run.skipped.push(SkippedTask {
                    code: task.code.clone(),
                    title: task.title.clone(),
                    reason: "上游新增的任务，本版本未识别，已跳过（不会虚假成功）".into(),
                    hint: None,
                    reward: task.reward,
                });
                continue;
            }
        }
        // 注意用 `needs_work` 而不是 `is_actionable`：后者要求还有剩余次数，
        // 会把「已达标但没领奖」的任务排除掉，积分就永远留在账号里。
        if !task.needs_work() {
            continue;
        }
        advance_task(&growth, cred, task, &mut run, dry_run).await;
    }

    run.energy_after = growth.energy(cred).await.ok();
    run.credits_earned = run.claimed.iter().map(|c| c.credit).sum();
    run
}

/// 把单个任务推到目标并领奖。
///
/// 每次上报后都重拉清单核对 `current`；连续两次无变化即判定「上游未推进」并
/// 放弃。这是本模块唯一愿意相信的进度信号——`code: 0` 不足以说明任何事。
async fn advance_task(
    growth: &GrowthClient<'_>,
    cred: &WorkBuddyCredential,
    task: &GrowthTask,
    run: &mut AccountGrowthRun,
    dry_run: bool,
) {
    if dry_run {
        return;
    }
    let Some(Automation::Auto { event }) = &task.automation else {
        return;
    };
    let kind: EventKind = *event;

    // 夜猫子任务在非夜间窗口上报不计数，白跑一次还多暴露一次。
    if kind == EventKind::NightCat {
        let hour = local_hour();
        if !crate::growth::in_night_window(hour) {
            run.skipped.push(SkippedTask {
                code: task.code.clone(),
                title: task.title.clone(),
                reason: format!("只在 23:00–08:00 上报才计入，当前 {hour}:00 已跳过"),
                hint: None,
                reward: task.reward,
            });
            return;
        }
    }

    let mut current = task.current;
    let mut stall_rounds = 0;

    while current < task.target {
        // 每次用不同的 idx，专家/团队任务才能拿到不同 id（上游按 id 去重）。
        // 若任务指定了专家 id（`jump_url` 里的 `expertId=`），必须用它——上游
        // 只认那一个，用通用池的 id 上报会让进度停在 0。
        let idx = current;
        let event_json = crate::growth::build_event_with_expert(
            kind,
            &cred.account.uid,
            idx,
            task.expert_id.as_deref(),
        );
        if let Err(e) = growth.report(cred, &[event_json]).await {
            let msg = e.to_string();
            run.restricted = is_restricted_error(&msg);
            run.errors.push(format!("{} 上报失败: {msg}", task.code));
            return;
        }

        tokio::time::sleep(std::time::Duration::from_millis(REPORT_GAP_MS as u64)).await;

        // 复核：只有 current 真的动了才算数。
        let fresh = match growth.tasks(cred).await {
            Ok(list) => list.into_iter().find(|t| t.code == task.code),
            Err(e) => {
                let msg = e.to_string();
                run.restricted = is_restricted_error(&msg);
                run.errors.push(format!("{} 复核失败: {msg}", task.code));
                return;
            }
        };
        let Some(fresh) = fresh else {
            run.errors
                .push(format!("{} 在复核时从清单里消失", task.code));
            return;
        };
        if fresh.current > current {
            current = fresh.current;
            stall_rounds = 0;
        } else {
            stall_rounds += 1;
            if stall_rounds >= 2 {
                run.stalled.push(task.code.clone());
                return;
            }
        }
        if fresh.status == "claimed" {
            return;
        }
    }

    // 达标 → 领奖。
    //
    // 走到这里有两种情况：刚才把进度推到了 target，或者进来时就已经达标
    // （`current >= target`，循环体一次也没进）。后者正是"上游已完成但我们还
    // 没领"的状态，必须领，否则积分停在账号里、面板却显示 completed。
    match growth.claim(cred, &task.code).await {
        Ok(report) => {
            if report.already_claimed {
                return;
            }
            run.claimed.push(ClaimedTask {
                code: task.code.clone(),
                title: task.title.clone(),
                credit: report.credit,
                energy: report.energy,
            });
        }
        Err(e) => {
            let msg = e.to_string();
            run.restricted = is_restricted_error(&msg);
            run.errors.push(format!("{} 领奖失败: {msg}", task.code));
        }
    }
}

/// 本地小时（0–23），用于夜猫子窗口判断。
fn local_hour() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // 与 `workbuddy_auth::local_day` 同源：用本地时区偏移，而不是 UTC。
    let offset = crate::util::local_utc_offset_secs();
    let local = secs + offset;
    ((local % 86_400) / 3_600) as u32
}

/// 跑整池。
///
/// 账号之间**随机错峰 2–5 秒**：同设备多账号在极短时间内连续访问同一套成长接口
/// 是风控的典型触发条件。这个延时不计入单账号预算。
pub async fn run_pool(
    client: &reqwest::Client,
    credentials: &[WorkBuddyCredential],
    only: Option<&[&str]>,
    dry_run: bool,
) -> GrowthRunReport {
    let mut report = GrowthRunReport::default();
    let mut first = true;

    for cred in credentials {
        if !cred.enabled {
            report
                .skipped_accounts
                .push((cred.id.clone(), "账号已禁用".into()));
            continue;
        }
        if let Some(until) = cred.cooldown_until_ms {
            if until > crate::util::unix_millis() {
                report
                    .skipped_accounts
                    .push((cred.id.clone(), "上游风控受限中，等待解封".into()));
                continue;
            }
        }

        if !first {
            let jitter = 2_000 + (crate::util::unix_millis() % 3_000);
            tokio::time::sleep(std::time::Duration::from_millis(jitter as u64)).await;
        }
        first = false;

        let run = run_account(client, cred, only, dry_run).await;
        if run.restricted {
            report
                .skipped_accounts
                .push((cred.id.clone(), "本次运行被上游风控中断".into()));
        }
        report.total_accounts += 1;
        report.total_credits += run.credits_earned;
        report.total_claimed += run.claimed.len();
        report.accounts.push(run);
    }
    report
}

/// 把「本次运行」的日期记到偏好里，防同一天重复跑。
pub fn mark_growth_run(day: &str, night: bool) -> anyhow::Result<()> {
    let mut prefs = crate::workbuddy_auth::load_preferences();
    if night {
        prefs.last_night_growth_date = Some(day.to_string());
    } else {
        prefs.last_growth_run_date = Some(day.to_string());
    }
    crate::workbuddy_auth::save_preferences(&prefs)
}

/// 本次是否该跑日间批次。
pub fn should_run_day(prefs: &crate::workbuddy_auth::PoolPreferences, today: &str) -> bool {
    prefs.growth_enabled && prefs.last_growth_run_date.as_deref() != Some(today)
}

/// 本次是否该跑夜间批次（`black_cat`）。
///
/// 与日间批次独立：夜间批次只跑 `black_cat` 一个任务，且只在 23:00–08:00 有效。
/// 两者互不占用对方的"今天已跑"标记。
pub fn should_run_night(prefs: &crate::workbuddy_auth::PoolPreferences, today: &str) -> bool {
    prefs.growth_enabled
        && prefs.last_night_growth_date.as_deref() != Some(today)
        && crate::growth::in_night_window(local_hour())
}

/// 按用户手动触发的要求，把结果压成一行日志。
pub fn summarize(report: &GrowthRunReport) -> String {
    if report.accounts.is_empty() {
        return "成长任务：没有可运行的账号".to_string();
    }
    let mut parts = vec![format!(
        "共 {} 个账号，领取 {} 个任务，获得 {} 积分",
        report.total_accounts, report.total_claimed, report.total_credits
    )];
    let stalled: usize = report.accounts.iter().map(|a| a.stalled.len()).sum();
    if stalled > 0 {
        parts.push(format!("{stalled} 个任务上游未推进"));
    }
    let errors: usize = report.accounts.iter().map(|a| a.errors.len()).sum();
    if errors > 0 {
        parts.push(format!("{errors} 条错误（见日志）"));
    }
    parts.join("；")
}

/// 按 credential id 汇总本次运行结果，供 GUI 展示。
pub fn by_account(report: &GrowthRunReport) -> HashMap<String, AccountGrowthRun> {
    report
        .accounts
        .iter()
        .map(|a| (a.credential_id.clone(), a.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workbuddy_auth::PoolPreferences;

    fn prefs(enabled: bool, day: Option<&str>, night: Option<&str>) -> PoolPreferences {
        PoolPreferences {
            growth_enabled: enabled,
            last_growth_run_date: day.map(str::to_string),
            last_night_growth_date: night.map(str::to_string),
            ..Default::default()
        }
    }

    /// 默认关闭：没显式开启时任何批次都不跑。
    #[test]
    fn growth_is_off_until_enabled() {
        let p = prefs(false, None, None);
        assert!(!should_run_day(&p, "2026-10-25"));
        assert!(!should_run_night(&p, "2026-10-25"));
    }

    /// 同一天不跑第二次。
    #[test]
    fn day_pass_runs_once_per_day() {
        let p = prefs(true, Some("2026-10-25"), None);
        assert!(!should_run_day(&p, "2026-10-25"));
        assert!(should_run_day(&p, "2026-10-26"));
    }

    /// 日间批次与夜间批次互不占用对方的标记。
    ///
    /// 这是个容易写错的点：如果两者共用一个日期字段，白天跑完就会把夜间的
    /// 机会吞掉，`black_cat` 永远轮不到。
    #[test]
    fn day_and_night_passes_track_separately() {
        let p = prefs(true, Some("2026-10-25"), None);
        // 白天跑过了，但夜间还没跑 —— 只要当前处于夜间窗口就该跑。
        if crate::growth::in_night_window(local_hour()) {
            assert!(should_run_night(&p, "2026-10-25"));
        }
        let p2 = prefs(true, None, Some("2026-10-25"));
        assert!(should_run_day(&p2, "2026-10-25"));
    }

    /// 403 与 11140 都要识别为风控，其它错误不算。
    #[test]
    fn restriction_detection_matches_upstream_signals() {
        assert!(is_restricted_error("接取失败: 返回 403 Forbidden"));
        assert!(is_restricted_error("事件上报被拒 (code=11140): risk"));
        assert!(!is_restricted_error("网络超时"));
        assert!(!is_restricted_error(
            "领奖被拒 (code=400): task not completed"
        ));
    }

    /// 汇总行必须把「没推进」和「错误」分开说，不能只说"完成"。
    #[test]
    fn summary_surfaces_stalls_and_errors() {
        let report = GrowthRunReport {
            accounts: vec![AccountGrowthRun {
                credential_id: "wb-a".into(),
                label: "甲".into(),
                claimed: vec![ClaimedTask {
                    code: "chat_5".into(),
                    title: "对话".into(),
                    credit: 100.0,
                    energy: 5.0,
                }],
                credits_earned: 100.0,
                stalled: vec!["expert_5".into()],
                errors: vec!["领奖失败: x".into()],
                ..Default::default()
            }],
            total_credits: 100.0,
            total_claimed: 1,
            total_accounts: 1,
            skipped_accounts: vec![],
        };
        let s = summarize(&report);
        assert!(s.contains("100"), "要报出积分: {s}");
        assert!(s.contains("未推进"), "要暴露未推进任务: {s}");
        assert!(s.contains("错误"), "要暴露错误: {s}");
    }

    /// 空池不该 panic，也不该产出误导性的「全部完成」。
    #[test]
    fn empty_report_is_explicit() {
        let s = summarize(&GrowthRunReport::default());
        assert!(s.contains("没有可运行"), "got: {s}");
    }
}
