//! The wire protocol: paths, envelopes, response shapes, and the HTTP client.

use super::*;

/// 默认成长接口 host。
///
/// 与聊天侧同一个 authority（桌面客户端的事件上报也打这里）。`www.codebuddy.cn`
/// 同样可用且数据一致（2026-10-25 实测），但聊天侧的指纹头与它配套，默认走这里。
pub const DEFAULT_GROWTH_ENDPOINT: &str = "https://copilot.tencent.com";

/// 任务清单路径。
pub(crate) const TASKS_PATH: &str = "/v2/activity/growth/tasks";

/// 批量接取路径。
pub(crate) const ACCEPT_PATH: &str = "/v2/activity/growth/tasks/accept";

/// 能量（成长值）余额路径。
pub(crate) const ENERGY_PATH: &str = "/v2/activity/growth/energy";

/// 事件上报路径。
pub(crate) const REPORT_PATH: &str = "/v2/report";

/// `GET /v2/activity/growth/tasks` 的一条原始任务。
#[derive(Debug, Deserialize)]
pub(crate) struct RawTask {
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
pub(crate) struct RawProgress {
    #[serde(default)]
    current: u32,
    #[serde(default)]
    target: u32,
}

/// 上游统一信封。
#[derive(Debug, Deserialize)]
pub(crate) struct Envelope<T> {
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
                // `progress` 缺失意味着**上游还没为这个任务建立进度记录**，
                // 通常就是「尚未接取」。此时把 current 当 0、target 取表里的值
                // 是对的；但如果任务尚未接取，就不该上报——那属于接取阶段的
                // 事，上报只会换来一条无效事件。
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
pub(crate) struct TasksData {
    #[serde(default)]
    tasks: Vec<RawTask>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AcceptData {
    #[serde(default)]
    results: Vec<AcceptResult>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AcceptResult {
    #[serde(default)]
    task_code: String,
    #[serde(default)]
    status: String,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ClaimData {
    #[serde(default)]
    credit: f64,
    #[serde(default)]
    energy: f64,
    #[serde(default)]
    already_claimed: bool,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct EnergyData {
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
