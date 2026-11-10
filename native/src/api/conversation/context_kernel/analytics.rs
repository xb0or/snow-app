//! 上游 `cache-report.ts`（billion-context#800）：提示词缓存对账。
//!
//! provider 报的每次请求用量就是**总账** —— 这个模块从不发明数字；它把每次请求的
//! 缓存未命中拆成三个可加桶，让合计**按构造**精确闭合：
//!
//! ```text
//! missed_i = input_i − cached_i
//!          = newContent_i   自上次请求以来新增的内容（合法的新鲜量，根本不是失效）
//!          + compRepay_i    压缩折叠强制重付的部分（前缀在折叠的首个起点处分叉）
//!          + ttlRepay_i     残差：稳定前缀里**无法归因**的未命中 → 上游 TTL 到期 /
//!                           provider 侧驱逐，或客户端 wire 重写 —— 内核区分不了
//! ```
//!
//! 没有任何估算参与闭合：newContent 被夹到观测到的增长内，compRepay 同时被剩余未命中
//! 和折叠分叉点蕴含的结构性超额夹住，ttlRepay 就是剩下的。因此对**任意**输入：
//!
//! ```text
//! Σ input = Σ cached + Σ newContent + Σ compRepay + Σ ttlRepay   （精确）
//! ```
//!
//! 上游来源：https://github.com/ranxianglei/acp-kernel（MIT）。

/// 一次请求的用量观测。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CacheSample {
    /// 观测到用量报告的 epoch 毫秒。
    pub at: i64,
    /// 该请求计费的提示词 token 总量 —— **归一化**为包含 cached（Anthropic 的
    /// `input_tokens` 不含缓存，OpenAI 含）。
    pub input: i64,
    /// provider 报告的缓存命中 token（provider 不报时为 0）。
    pub cached: i64,
    pub output: i64,
}

/// 一次折叠事件。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FoldEvent {
    /// 折叠应用的 epoch 毫秒（位于两次请求之间）。
    pub at: i64,
    /// S：这次折叠从视图里移除的 token。
    pub tokens_compressed: i64,
    /// σ：渲染后摘要的 token 量（或摘要实际输出 token）。
    pub summary_tokens: i64,
    /// X：折叠后视图里最早分叉点的 token 偏移 ≈ 期望的折叠后命中前缀。
    pub first_fold_start_tokens: Option<i64>,
    pub view_before: Option<i64>,
    pub view_after: Option<i64>,
}

/// 归一化价格倍率（以输入 token 单价 p_in = 1 为单位）：w = 写缓存/输入，
/// r = 读缓存/输入，q = 输出/输入。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PriceProfile {
    pub w: f64,
    pub r: f64,
    pub q: f64,
}

impl Default for PriceProfile {
    fn default() -> Self {
        Self {
            w: 1.0,
            r: 0.1,
            q: 4.0,
        }
    }
}

/// 一次请求未命中的三桶拆分。
#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SampleDecomposition {
    pub missed: i64,
    pub new_content: i64,
    pub comp_repay: i64,
    pub ttl_repay: i64,
    pub fold_index: Option<usize>,
}

/// 把一次请求的未命中拆进三个桶。
pub fn decompose_sample(
    prev: Option<&CacheSample>,
    cur: &CacheSample,
    pending_folds: &[FoldEvent],
) -> SampleDecomposition {
    let missed = (cur.input - cur.cached).max(0);
    let growth = prev.map(|prev| (cur.input - prev.input).max(0)).unwrap_or(0);
    let mut new_content = growth.min(missed);
    let mut comp_repay = 0i64;
    let mut fold_index = None;
    if !pending_folds.is_empty() && missed > new_content {
        // 结构性超额：折叠把前缀从 X 处截断，其后所有内容都要重付。
        let mut structural = 0i64;
        for (index, fold) in pending_folds.iter().enumerate() {
            let Some(divergence) = fold.first_fold_start_tokens else {
                continue;
            };
            let excess = (cur.input - divergence).max(0);
            if excess > structural {
                structural = excess;
                fold_index = Some(index);
            }
        }
        comp_repay = structural.min(missed - new_content).max(0);
    }
    if new_content + comp_repay > missed {
        new_content = missed - comp_repay;
    }
    SampleDecomposition {
        missed,
        new_content,
        comp_repay,
        ttl_repay: missed - new_content - comp_repay,
        fold_index,
    }
}

/// 单次折叠的经济学（测得值）。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FoldEconomics {
    pub seq: usize,
    pub at: i64,
    pub s: i64,
    pub sigma: i64,
    pub v_prime: Option<i64>,
    pub h_pct: Option<f64>,
    pub t: i64,
    pub requests_after: usize,
    pub saved_so_far: i64,
    pub turns_to_next_fold: Option<usize>,
    pub net_token_delta: i64,
    pub one_time_cost_units: f64,
    pub per_turn_saving_units: f64,
    pub breakeven_turns: Option<f64>,
    pub paid_back: Option<bool>,
}

/// 计算单次折叠的经济学。
pub fn compute_fold_economics(
    seq: usize,
    at: i64,
    s: i64,
    sigma: i64,
    v_prime: Option<i64>,
    h_pct: Option<f64>,
    t: i64,
    requests_after: usize,
    turns_to_next_fold: Option<usize>,
    price: PriceProfile,
) -> FoldEconomics {
    let saved_so_far = (s - sigma) * requests_after as i64;
    let one_time_cost_units = (price.w - price.r) * t as f64 + price.q * sigma as f64
        - price.r * s as f64;
    let per_turn_saving_units = (s - sigma) as f64 * price.r;
    let breakeven_turns = if per_turn_saving_units > 0.0 {
        Some((one_time_cost_units.max(0.0)) / per_turn_saving_units)
    } else {
        None
    };
    let paid_back = match (breakeven_turns, turns_to_next_fold) {
        (Some(breakeven), Some(observed)) => Some(observed as f64 >= breakeven),
        _ => None,
    };
    FoldEconomics {
        seq,
        at,
        s,
        sigma,
        v_prime,
        h_pct,
        t,
        requests_after,
        saved_so_far,
        turns_to_next_fold,
        net_token_delta: t + sigma - s,
        one_time_cost_units,
        per_turn_saving_units,
        breakeven_turns,
        paid_back,
    }
}

/// 折叠经济学汇总。
#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EconomicsSummary {
    pub folds: usize,
    pub gross_saved: i64,
    pub repay_cost: i64,
    pub summary_cost: i64,
    pub net_tokens: i64,
    pub paid_back_count: usize,
    pub not_paid_back_count: usize,
    pub unobserved_count: usize,
}

pub fn summarize_fold_economics(folds: &[FoldEconomics]) -> EconomicsSummary {
    let mut summary = EconomicsSummary {
        folds: folds.len(),
        ..Default::default()
    };
    for fold in folds {
        summary.gross_saved += (fold.s - fold.sigma) * fold.requests_after as i64;
        summary.repay_cost += fold.t;
        summary.summary_cost += fold.sigma;
        match fold.paid_back {
            Some(true) => summary.paid_back_count += 1,
            Some(false) => summary.not_paid_back_count += 1,
            None => summary.unobserved_count += 1,
        }
    }
    summary.net_tokens = summary.gross_saved - summary.repay_cost - summary.summary_cost;
    summary
}

/// 总账汇总。
#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheTotals {
    pub requests: usize,
    pub input: i64,
    pub cached: i64,
    pub output: i64,
    pub hit_pct: f64,
    pub new_content: i64,
    pub comp_repay: i64,
    pub ttl_repay: i64,
    pub residual: i64,
    pub balanced: bool,
}

/// 一行请求明细。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CacheReportLine {
    pub seq: usize,
    pub at: i64,
    pub input: i64,
    pub cached: i64,
    pub output: i64,
    pub hit_pct: f64,
    pub missed: i64,
    pub new_content: i64,
    pub comp_repay: i64,
    pub ttl_repay: i64,
    pub fold_seq: Option<usize>,
}

/// 完整报告。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CacheReport {
    pub generated_at: i64,
    pub profile: PriceProfile,
    pub totals: CacheTotals,
    pub economics: EconomicsSummary,
    pub folds: Vec<FoldEconomics>,
    pub lines: Vec<CacheReportLine>,
    pub lines_omitted: usize,
}

/// 构建报告：`max_lines` 保留最新的若干条明细（合计仍覆盖全部）。
pub fn build_cache_report(
    samples: &[CacheSample],
    fold_events: &[FoldEvent],
    max_lines: usize,
    now_ms: i64,
) -> CacheReport {
    let mut totals = CacheTotals {
        requests: samples.len(),
        ..Default::default()
    };
    let mut lines: Vec<CacheReportLine> = Vec::new();
    let mut fold_accounting: Vec<(i64, usize)> = Vec::new();
    let mut prev: Option<CacheSample> = None;
    for (index, sample) in samples.iter().enumerate() {
        let pending: Vec<FoldEvent> = fold_events
            .iter()
            .filter(|fold| {
                fold.at <= sample.at && prev.is_none_or(|previous| fold.at > previous.at)
            })
            .copied()
            .collect();
        let decomposition = decompose_sample(prev.as_ref(), sample, &pending);
        totals.input += sample.input;
        totals.cached += sample.cached;
        totals.output += sample.output;
        totals.new_content += decomposition.new_content;
        totals.comp_repay += decomposition.comp_repay;
        totals.ttl_repay += decomposition.ttl_repay;
        let fold_seq = decomposition.fold_index.map(|position| {
            let global = fold_accounting
                .iter()
                .filter(|(at, _)| *at <= sample.at)
                .count();
            fold_accounting.push((sample.at, position));
            global
        });
        lines.push(CacheReportLine {
            seq: index + 1,
            at: sample.at,
            input: sample.input,
            cached: sample.cached,
            output: sample.output,
            hit_pct: if sample.input > 0 {
                sample.cached as f64 / sample.input as f64 * 100.0
            } else {
                0.0
            },
            missed: decomposition.missed,
            new_content: decomposition.new_content,
            comp_repay: decomposition.comp_repay,
            ttl_repay: decomposition.ttl_repay,
            fold_seq,
        });
        prev = Some(*sample);
    }
    totals.hit_pct = if totals.input > 0 {
        totals.cached as f64 / totals.input as f64 * 100.0
    } else {
        0.0
    };
    totals.residual = totals.input
        - totals.cached
        - (totals.new_content + totals.comp_repay + totals.ttl_repay);
    totals.balanced = totals.residual == 0;

    // 折叠经济学：每次折叠之后观测到的请求数，以及首次折叠后请求的命中率。
    let mut folds: Vec<FoldEconomics> = Vec::new();
    for (index, event) in fold_events.iter().enumerate() {
        let after: Vec<&CacheReportLine> = lines.iter().filter(|line| line.at > event.at).collect();
        let h_pct = after.first().map(|line| line.hit_pct);
        let next_fold_at = fold_events.get(index + 1).map(|fold| fold.at);
        let turns_to_next_fold = next_fold_at.map(|at| {
            lines
                .iter()
                .filter(|line| line.at > event.at && line.at <= at)
                .count()
        });
        let repay: i64 = after
            .first()
            .map(|line| line.comp_repay)
            .unwrap_or(0);
        folds.push(compute_fold_economics(
            index + 1,
            event.at,
            event.tokens_compressed,
            event.summary_tokens,
            event.view_after,
            h_pct,
            repay,
            after.len(),
            turns_to_next_fold,
            PriceProfile::default(),
        ));
    }
    let economics = summarize_fold_economics(&folds);
    let lines_omitted = lines.len().saturating_sub(max_lines.max(1));
    if lines_omitted > 0 {
        lines.drain(..lines_omitted);
    }
    CacheReport {
        generated_at: now_ms,
        profile: PriceProfile::default(),
        totals,
        economics,
        folds,
        lines,
        lines_omitted,
    }
}

// ---------------------------------------------------------------------------
// 渲染
// ---------------------------------------------------------------------------

/// 报告里的数字格式：≥ 10,000 用 `55.0K`，否则原样。
fn format_k(value: i64) -> String {
    if value >= 10_000 {
        format!("{:.1}K", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

fn format_time(at: i64) -> String {
    chrono::DateTime::from_timestamp_millis(at)
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| "--:--:--".to_string())
}

fn format_pct(value: f64) -> String {
    format!("{value:.1}%")
}

/// 把报告渲染成紧凑、模型与人双可读的文本。身份行（`identity check`）永远打印，
/// 这样实现坏了也无处藏身。
pub fn format_cache_report(report: &CacheReport, session_label: &str, detail_full: bool) -> String {
    let label = if session_label.trim().is_empty() {
        String::new()
    } else {
        format!(" ({})", session_label.trim())
    };
    let mode = if detail_full { "full" } else { "summary" };
    let mut out = format!(
        "ACP CACHE REPORT{label} — {} requests  [{mode} — detail:\"full\" for every fold & line]\n",
        report.totals.requests
    );
    let totals = &report.totals;
    let verdict = if totals.hit_pct >= 85.0 {
        "OK"
    } else {
        "INVESTIGATE"
    };
    out.push_str("\nGRAND LEDGER\n");
    out.push_str(&format!("  total input    {} tok\n", format_k(totals.input)));
    out.push_str(&format!(
        "  total cached   {} tok  (hit {} → {verdict})\n",
        format_k(totals.cached),
        format_pct(totals.hit_pct)
    ));
    out.push_str(&format!("  total output   {} tok\n", format_k(totals.output)));
    let missed = (totals.input - totals.cached).max(0);
    out.push_str(&format!(
        "  miss breakdown (input − cached = {} tok):\n",
        format_k(missed)
    ));
    let per_request = if totals.requests > 0 {
        totals.new_content / totals.requests as i64
    } else {
        0
    };
    out.push_str(&format!(
        "    new content     {} tok  (~{per_request}/req fresh append — not an invalidation)\n",
        format_k(totals.new_content)
    ));
    out.push_str(&format!(
        "    compress re-pay {} tok  ({} folds — re-billed prefix)\n",
        format_k(totals.comp_repay),
        report.economics.folds
    ));
    let mut spikes: Vec<(usize, i64)> = report
        .lines
        .iter()
        .map(|line| (line.seq, line.ttl_repay))
        .filter(|(_, value)| *value > 0)
        .collect();
    spikes.sort_by(|a, b| b.1.cmp(&a.1));
    let spike_text = spikes
        .iter()
        .take(3)
        .map(|(seq, value)| format!("#{seq} {}", format_k(*value)))
        .collect::<Vec<_>>()
        .join(", ");
    out.push_str(&format!(
        "    upstream-ttl-or-client-rewrite (unattributed)  {} tok  (stable-prefix misses: upstream TTL expiry / eviction or client-side wire rewrite — kernel cannot distinguish{}\n",
        format_k(totals.ttl_repay),
        if spike_text.is_empty() {
            ")".to_string()
        } else {
            format!("; top spikes: {spike_text})")
        }
    ));
    out.push_str(&format!(
        "  identity check   {} — {} = {} + {} + {} + {} (residual {})\n",
        if totals.balanced { "OK" } else { "BROKEN" },
        format_k(totals.input),
        format_k(totals.cached),
        format_k(totals.new_content),
        format_k(totals.comp_repay),
        format_k(totals.ttl_repay),
        totals.residual
    ));

    let p = report.profile;
    out.push_str(&format!(
        "\nFOLD ECONOMICS ({} folds @ w={} r={} q={})\n",
        report.economics.folds, p.w, p.r, p.q
    ));
    out.push_str(&format!(
        "  gross saved {} tok · repay cost {} tok · summary cost {} tok → net {}{} tok\n",
        report.economics.gross_saved,
        report.economics.repay_cost,
        report.economics.summary_cost,
        if report.economics.net_tokens >= 0 { "+" } else { "-" },
        report.economics.net_tokens.abs()
    ));
    out.push_str(&format!(
        "  verdict: {} PAID BACK / {} NOT PAID BACK / {} unobserved\n",
        report.economics.paid_back_count,
        report.economics.not_paid_back_count,
        report.economics.unobserved_count
    ));
    for fold in report.folds.iter() {
        out.push_str(&format!(
            "  #{} {} S={} σ={} h={} T={} ΔC₁={} Δs={}/turn n*={} k={} → {}\n",
            fold.seq,
            format_time(fold.at),
            fold.s,
            fold.sigma,
            fold
                .h_pct
                .map(format_pct)
                .unwrap_or_else(|| "—".to_string()),
            fold.t,
            trim_number(fold.one_time_cost_units),
            trim_number(fold.per_turn_saving_units),
            fold.breakeven_turns
                .map(|value| format!("{value:.1}"))
                .unwrap_or_else(|| "—".to_string()),
            fold.turns_to_next_fold
                .map(|value| value.to_string())
                .unwrap_or_else(|| "—".to_string()),
            match fold.paid_back {
                Some(true) => "✓",
                Some(false) => "✗",
                None => "?",
            }
        ));
    }

    let anomalies: Vec<&CacheReportLine> = report
        .lines
        .iter()
        .filter(|line| line.hit_pct < 85.0 || line.missed >= 5_000)
        .collect();
    if !anomalies.is_empty() || detail_full {
        out.push_str("\nLINE ITEMS (anomalies: hit<85% or miss≥5000):\n");
        out.push_str("  #     time      input  cached    hit%      new    comp     ttl  fold\n");
        for line in report.lines.iter() {
            if !detail_full && !anomalies.iter().any(|entry| entry.seq == line.seq) {
                continue;
            }
            out.push_str(&format!(
                "  {:>4}  {}  {:>7}  {:>7}  {:>6}  {:>7}  {:>7}  {:>7}  {}\n",
                line.seq,
                format_time(line.at),
                format_k(line.input),
                format_k(line.cached),
                format_pct(line.hit_pct),
                format_k(line.new_content),
                format_k(line.comp_repay),
                format_k(line.ttl_repay),
                line.fold_seq
                    .map(|value| format!("#{value}"))
                    .unwrap_or_default()
            ));
        }
    }
    if report.lines_omitted > 0 {
        out.push_str(&format!(
            "\n({} older line(s) omitted from the listing; totals still cover them)\n",
            report.lines_omitted
        ));
    }
    out.trim_end().to_string()
}

fn trim_number(value: f64) -> String {
    if (value.fract()).abs() < 1e-9 {
        format!("{}", value as i64)
    } else {
        format!("{value:.1}")
    }
}
