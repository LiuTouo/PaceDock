//! GPU 實體核心目標與快速測試排程。LP 永遠保持邏輯處理器索引語意。
use super::{BenchmarkConfig, FpsCapPolicy, LpResult};
use crate::{error::codes, topology::Topology};
use serde::{Deserialize, Serialize};

pub const METHOD_VERSION: u32 = 3;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CoreTarget {
    pub core_id: u32,
    pub lp_indices: Vec<u32>,
}

pub fn candidates(topo: &Topology) -> Vec<CoreTarget> {
    if topo.processor_groups != 1 {
        return vec![];
    }
    topo.physical_cores
        .iter()
        .filter(|c| !topo.has_hybrid || c.is_p_core)
        .filter(|c| !c.lp_indices.is_empty() && c.lp_indices.iter().all(|lp| *lp < 64))
        .filter(|c| {
            c.lp_indices.iter().all(|i| {
                topo.logical_processors
                    .iter()
                    .any(|l| l.index == *i && l.core_id == c.id)
            })
        })
        .filter(|c| {
            c.lp_indices
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len()
                == c.lp_indices.len()
        })
        .filter(|c| {
            topo.logical_processors
                .iter()
                .filter(|l| l.core_id == c.id)
                .count()
                == c.lp_indices.len()
        })
        .map(|c| CoreTarget {
            core_id: c.id,
            lp_indices: c.lp_indices.clone(),
        })
        .collect()
}

pub fn select(topo: &Topology, ids: &[u32]) -> Result<Vec<CoreTarget>, String> {
    let available = candidates(topo);
    if available.is_empty() {
        return Err(codes::BENCHMARK_SESSION_INCOMPATIBLE.into());
    }
    if ids.is_empty() {
        return Ok(available);
    }
    let mut selected = Vec::new();
    for id in ids {
        let target = available
            .iter()
            .find(|c| c.core_id == *id)
            .ok_or(codes::BENCHMARK_SESSION_INCOMPATIBLE)?;
        if selected.contains(target) {
            return Err(codes::BENCHMARK_INVALID_CONFIG.into());
        }
        selected.push(target.clone());
    }
    Ok(selected)
}

pub fn mask(target: &CoreTarget, topo: &Topology) -> Result<Vec<u8>, String> {
    if !candidates(topo).contains(target) {
        return Err(codes::BENCHMARK_SESSION_INCOMPATIBLE.into());
    }
    let bits = target
        .lp_indices
        .iter()
        .fold(0u64, |m, lp| m | (1u64 << lp));
    if bits == 0 {
        return Err(codes::BENCHMARK_SESSION_INCOMPATIBLE.into());
    }
    let mut bytes = bits.to_le_bytes().to_vec();
    while bytes.last() == Some(&0) {
        bytes.pop();
    }
    Ok(bytes)
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct QuickSchedule {
    pub screening_warmup_secs: u32,
    pub screening_sample_secs: u32,
    pub retest_warmup_secs: u32,
    pub retest_sample_secs: u32,
    pub candidate_captures: u32,
    pub estimated_min_secs: u64,
    pub estimated_max_secs: u64,
}

pub fn schedule(config: &BenchmarkConfig, count: usize) -> QuickSchedule {
    use super::runner::*;
    let n = count as u64;
    let overhead = 4 + (RESTART_STABILIZE_MS + WORKLOAD_STARTUP_MS) / 1000;
    let base = n * (overhead + config.warm_up_secs as u64 + config.sample_secs as u64)
        + n.min(2)
            * (overhead + config.retest_warm_up_secs as u64 + config.retest_sample_secs as u64)
        + 4;
    let calibration =
        (CALIBRATION_WARMUP_SECS + CALIBRATION_SAMPLE_SECS) as u64 + WORKLOAD_STARTUP_MS / 1000;
    let adaptive = config.fps_cap_policy == FpsCapPolicy::Adaptive;
    QuickSchedule {
        screening_warmup_secs: config.warm_up_secs,
        screening_sample_secs: config.sample_secs,
        retest_warmup_secs: config.retest_warm_up_secs,
        retest_sample_secs: config.retest_sample_secs,
        candidate_captures: (n + n.min(2)) as u32,
        estimated_min_secs: base + if adaptive { calibration * 2 } else { 0 },
        estimated_max_secs: base
            + if adaptive {
                calibration * CALIBRATION_TIERS.len() as u64
            } else {
                0
            },
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub enum RankingStatus {
    Consistent,
    Close,
    Reversed,
    SingleCandidate,
    #[default]
    Insufficient,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CoreCapture {
    pub target: CoreTarget,
    pub metrics: LpResult,
    pub score: f64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct QuickResult {
    pub method_version: u32,
    pub candidates: Vec<CoreTarget>,
    pub seed: u32,
    pub screening_order: Vec<u32>,
    pub retest_order: Vec<u32>,
    pub schedule: QuickSchedule,
    pub screening: Vec<CoreCapture>,
    pub retest: Vec<CoreCapture>,
    pub status: RankingStatus,
    pub relative_gap_pct: Option<f64>,
    /// 原始策略（OS 預設）對照結果；舊 session 無此欄位 → None
    #[serde(default)]
    pub baseline: Option<BaselineCompare>,
}

/// 基線對照門檻：p1_low 需改善 ≥ [`BASELINE_P1_MIN_IMPROVE_PCT`]，
/// avg 退步與 MAD/spike 惡化分別不得超過各自上限，否則不算「勝過預設」。
pub const BASELINE_P1_MIN_IMPROVE_PCT: f64 = 1.0;
pub const BASELINE_AVG_MAX_REGRESS_PCT: f64 = 1.0;
pub const BASELINE_ROBUST_MAX_REGRESS_PCT: f64 = 5.0;
/// 前後兩次基線自身差異超過此值（p1 相對中位數）→ 環境漂移，對照不可靠
pub const BASELINE_DRIFT_MAX_PCT: f64 = 5.0;

/// 勝出候選 vs 原始策略（前後兩次基線的中位數）的對照結論。
/// 只改變推薦文案，不作為套用閘門（eligible 不受影響）。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BaselineCompare {
    pub verdict: BaselineVerdict,
    /// 勝出者 1% low 相對基線中位數的改善百分比（正 = 較佳）
    pub p1_improvement_pct: Option<f64>,
    /// 勝出者 avg FPS 相對基線中位數的改善百分比
    pub avg_improvement_pct: Option<f64>,
    /// 前後基線自身的 p1 差異百分比（環境漂移指標；樣本不足 → None）
    pub baseline_drift_pct: Option<f64>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum BaselineVerdict {
    /// 勝出候選明顯優於 OS 預設
    BeatsDefault,
    /// 與預設無顯著差異（套用無實質效益）
    WithinThreshold,
    /// OS 預設較佳
    Worse,
    /// 基線不可靠（無資料或前後漂移過大）
    Inconclusive,
}

/// 純比較：勝出者與前後兩次原始策略基線的逐指標對比。
/// 基線值取各 capture 的中位數（B0/B1 各一次 capture）。
pub fn compare_baseline(winner: &LpResult, baselines: &[LpResult]) -> BaselineCompare {
    use super::metrics::median;
    let metric = |f: fn(&LpResult) -> Option<f64>| -> Option<f64> {
        let vals: Vec<f64> = baselines
            .iter()
            .filter_map(f)
            .filter(|v| v.is_finite() && *v > 0.0)
            .collect();
        (!vals.is_empty()).then(|| median(&vals))
    };
    let improvement = |f: fn(&LpResult) -> Option<f64>| -> Option<f64> {
        let base = metric(f)?;
        let w = f(winner)?;
        (w.is_finite() && w > 0.0).then(|| (w / base - 1.0) * 100.0)
    };
    let p1 = improvement(|b| b.p1_low);
    let avg = improvement(|b| b.avg_fps);
    // 前後基線自身漂移：兩次都有 p1 才計算
    let drift = (baselines.len() == 2).then(|| -> Option<f64> {
        let a = baselines[0].p1_low?;
        let b = baselines[1].p1_low?;
        let base = metric(|x| x.p1_low)?;
        Some(((a - b).abs() / base) * 100.0)
    });
    let baseline_drift_pct = drift.flatten();
    let inconclusive = BaselineCompare {
        verdict: BaselineVerdict::Inconclusive,
        p1_improvement_pct: p1,
        avg_improvement_pct: avg,
        baseline_drift_pct,
    };
    // 環境漂移過大或基線無有效 p1 → 對照不可靠
    if metric(|b| b.p1_low).is_none() {
        return inconclusive;
    }
    if baseline_drift_pct.is_some_and(|d| d > BASELINE_DRIFT_MAX_PCT) {
        return inconclusive;
    }
    let Some(p1_imp) = p1 else {
        return inconclusive;
    };
    // 明顯更差：p1 或 avg 退步超過門檻
    if p1_imp < -BASELINE_P1_MIN_IMPROVE_PCT
        || avg.is_some_and(|a| a < -BASELINE_AVG_MAX_REGRESS_PCT)
    {
        return BaselineCompare {
            verdict: BaselineVerdict::Worse,
            p1_improvement_pct: p1,
            avg_improvement_pct: avg,
            baseline_drift_pct,
        };
    }
    // 勝過預設：p1 改善達標、avg 無明顯退步、MAD/spike 無明顯惡化
    let guards: [fn(&LpResult) -> Option<f64>; 2] = [|b| b.frametime_mad_pct, |b| b.spike_rate_pct];
    let robust_ok = guards.iter().all(|f| match (metric(*f), (*f)(winner)) {
        (Some(base), Some(w)) if base > 0.0 && w.is_finite() => {
            (w / base - 1.0) * 100.0 <= BASELINE_ROBUST_MAX_REGRESS_PCT
        }
        _ => true, // 基線缺該指標 → 不納入守門
    });
    let verdict = if p1_imp >= BASELINE_P1_MIN_IMPROVE_PCT
        && avg.map_or(true, |a| a >= -BASELINE_AVG_MAX_REGRESS_PCT)
        && robust_ok
    {
        BaselineVerdict::BeatsDefault
    } else {
        BaselineVerdict::WithinThreshold
    };
    BaselineCompare {
        verdict,
        p1_improvement_pct: p1,
        avg_improvement_pct: avg,
        baseline_drift_pct,
    }
}

pub fn shuffled(ids: &[u32], seed: u32) -> Vec<u32> {
    let mut result = ids.to_vec();
    let mut state = seed as u64;
    for i in (1..result.len()).rev() {
        state = state.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^= z >> 31;
        result.swap(i, (z % (i as u64 + 1)) as usize);
    }
    result
}

pub fn rank(rows: &[LpResult], targets: &[CoreTarget]) -> Vec<CoreCapture> {
    use super::metrics::{competitive_score, round_medians};
    let valid: Vec<_> = rows
        .iter()
        .filter(|r| {
            r.error.is_none()
                && r.sample_count > 0
                && [r.avg_fps, r.p1_low, r.p01_low]
                    .iter()
                    .all(|v| v.is_some_and(|n| n.is_finite() && n > 0.0))
                && [r.frametime_mad_pct, r.spike_rate_pct]
                    .iter()
                    .all(|v| v.is_some_and(|n| n.is_finite() && n >= 0.0))
        })
        .cloned()
        .collect();
    let med = round_medians(&valid);
    let mut ranked: Vec<_> = valid
        .iter()
        .filter_map(|r| {
            let target = targets
                .iter()
                .find(|t| t.lp_indices.first() == Some(&r.lp))?;
            let score = competitive_score(r, &med)?;
            (score.is_finite() && score > 0.0).then(|| CoreCapture {
                target: target.clone(),
                metrics: r.clone(),
                score,
            })
        })
        .collect();
    ranked.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then(a.target.core_id.cmp(&b.target.core_id))
    });
    ranked
}

pub fn assess(
    screening: &[CoreCapture],
    retest: &[CoreCapture],
    count: usize,
) -> (RankingStatus, Option<f64>) {
    if count == 0
        || screening.len() != count
        || retest.len() != count.min(2)
        || retest
            .iter()
            .any(|r| !screening.iter().take(2).any(|s| s.target == r.target))
    {
        return (RankingStatus::Insufficient, None);
    }
    if count == 1 {
        return (RankingStatus::SingleCandidate, None);
    }
    let gap = (retest[0].score - retest[1].score).abs() / retest[1].score * 100.0;
    if !gap.is_finite() {
        return (RankingStatus::Insufficient, None);
    }
    (
        if gap <= 0.5 {
            RankingStatus::Close
        } else if screening[0].target != retest[0].target {
            RankingStatus::Reversed
        } else {
            RankingStatus::Consistent
        },
        Some(gap),
    )
}

/// 校準完成後，與初始估算共用排程成本；不包含不可預測的重試。
pub fn remaining_secs(plan: &QuickSchedule, count: usize, done: u32) -> u64 {
    let screening_left = (count as u32).saturating_sub(done) as u64;
    let retest_done = done.saturating_sub(count as u32);
    let retest_left = (count.min(2) as u32).saturating_sub(retest_done) as u64;
    let overhead =
        4 + (super::runner::RESTART_STABILIZE_MS + super::runner::WORKLOAD_STARTUP_MS) / 1000;
    screening_left
        * (overhead + plan.screening_warmup_secs as u64 + plan.screening_sample_secs as u64)
        + retest_left * (overhead + plan.retest_warmup_secs as u64 + plan.retest_sample_secs as u64)
        + 4
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{benchmark::metrics::compute_lp_result, topology::build_topology};

    #[test]
    fn whole_core_targets_include_zero_smt_sparse_and_lp63() {
        let topo = build_topology(vec![(vec![0, 3], 0, true), (vec![31, 63], 0, true)]);
        let cores = candidates(&topo);
        assert_eq!(cores.len(), 2);
        assert_eq!(cores[0].core_id, 0);
        assert_eq!(mask(&cores[0], &topo).unwrap(), vec![9]);
        assert_eq!(
            mask(&cores[1], &topo).unwrap(),
            (1u64 << 31 | 1u64 << 63).to_le_bytes()
        );
        assert!(mask(
            &CoreTarget {
                core_id: 0,
                lp_indices: vec![0]
            },
            &topo
        )
        .is_err());
        assert!(select(&topo, &[99]).is_err());
        assert!(select(&topo, &[0, 0]).is_err());
    }

    #[test]
    fn hybrid_only_p_cores_and_rejects_groups_and_empty() {
        let mut topo = build_topology(vec![(vec![0, 1], 2, true), (vec![4], 0, false)]);
        assert_eq!(candidates(&topo).len(), 1);
        assert!(select(&topo, &[1]).is_err());
        topo.processor_groups = 2;
        assert!(select(&topo, &[]).is_err());
        assert!(candidates(&Topology::default()).is_empty());
        assert!(candidates(&build_topology(vec![(vec![63, 64], 0, true)])).is_empty());
        assert_eq!(
            candidates(&build_topology(vec![
                (vec![0], 0, false),
                (vec![9], 0, false)
            ]))
            .len(),
            2
        );
    }

    #[test]
    fn schedule_counts_and_seed_are_reproducible() {
        for n in 1..65 {
            assert_eq!(
                schedule(&BenchmarkConfig::default(), n).candidate_captures,
                (n + n.min(2)) as u32
            );
        }
        let ids: Vec<_> = (0..16).collect();
        assert_eq!(shuffled(&ids, 15), shuffled(&ids, 15));
        assert_ne!(shuffled(&ids, 15), shuffled(&ids, 16));
        let mut order = shuffled(&ids, 16);
        order.sort();
        assert_eq!(order, ids);
    }

    #[test]
    fn rankings_close_reversal_insufficient_and_zero_mad() {
        let targets = candidates(&build_topology(vec![
            (vec![0], 0, false),
            (vec![1], 0, false),
        ]));
        let rows = vec![
            compute_lp_result(0, &[10.0; 100]).unwrap(),
            compute_lp_result(1, &[11.0; 100]).unwrap(),
        ];
        let screening = rank(&rows, &targets);
        assert_eq!(screening.len(), 2); // zero MAD/spike valid
        assert_eq!(
            assess(&screening, &screening, 2).0,
            RankingStatus::Consistent
        );
        let reverse = rank(
            &[
                compute_lp_result(0, &[11.0; 100]).unwrap(),
                compute_lp_result(1, &[10.0; 100]).unwrap(),
            ],
            &targets,
        );
        assert_eq!(assess(&screening, &reverse, 2).0, RankingStatus::Reversed);
        let close = rank(
            &[rows[0].clone(), compute_lp_result(1, &[10.0; 100]).unwrap()],
            &targets,
        );
        assert_eq!(
            assess(&screening, &close, 2),
            (RankingStatus::Close, Some(0.0))
        );
        assert_eq!(assess(&screening, &[], 2).0, RankingStatus::Insufficient);
        assert_eq!(
            assess(&screening[..1], &screening[..1], 1).0,
            RankingStatus::SingleCandidate
        );
        let mut bad = rows[0].clone();
        bad.avg_fps = Some(f64::NAN);
        assert!(rank(&[bad], &targets).is_empty());
    }

    // ── 原始策略對照（baseline A/B）──

    fn base_result(avg: f64, p1: f64, mad: f64, spike: f64) -> LpResult {
        LpResult {
            lp: 0,
            avg_fps: Some(avg),
            p1_low: Some(p1),
            frametime_mad_pct: Some(mad),
            spike_rate_pct: Some(spike),
            completed: true,
            ..Default::default()
        }
    }

    #[test]
    fn baseline_compare_beats_within_worse_and_inconclusive() {
        // 基線：p1=100、avg=200、MAD=10、spike=10（B0/B1 一致）
        let base = base_result(200.0, 100.0, 10.0, 10.0);
        // 勝出者 p1 +3%、avg −0.5%、MAD/spike 持平 → BeatsDefault
        let winner = base_result(199.0, 103.0, 10.0, 10.0);
        let cmp = compare_baseline(&winner, &[base.clone(), base.clone()]);
        assert_eq!(cmp.verdict, BaselineVerdict::BeatsDefault);
        assert!((cmp.p1_improvement_pct.unwrap() - 3.0).abs() < 1e-9);
        assert!((cmp.avg_improvement_pct.unwrap() + 0.5).abs() < 1e-9);
        assert!(cmp.baseline_drift_pct.unwrap() < 1e-9);
        // p1 僅 +0.5% → WithinThreshold
        let near = base_result(200.0, 100.5, 10.0, 10.0);
        assert_eq!(
            compare_baseline(&near, &[base.clone(), base.clone()]).verdict,
            BaselineVerdict::WithinThreshold
        );
        // p1 −2% → Worse
        let worse = base_result(200.0, 98.0, 10.0, 10.0);
        assert_eq!(
            compare_baseline(&worse, &[base.clone(), base.clone()]).verdict,
            BaselineVerdict::Worse
        );
        // MAD 惡化 +20%（> 5% 上限）→ 降級 WithinThreshold
        let regressed = base_result(200.0, 103.0, 12.0, 10.0);
        assert_eq!(
            compare_baseline(&regressed, &[base.clone(), base.clone()]).verdict,
            BaselineVerdict::WithinThreshold
        );
        // 前後基線 p1 差 8%（> 5% 漂移上限）→ Inconclusive
        let b0 = base_result(200.0, 96.0, 10.0, 10.0);
        let b1 = base_result(200.0, 104.0, 10.0, 10.0);
        let cmp = compare_baseline(&winner, &[b0, b1]);
        assert_eq!(cmp.verdict, BaselineVerdict::Inconclusive);
        assert!((cmp.baseline_drift_pct.unwrap() - 8.0).abs() < 1e-9);
        // 無有效基線 → Inconclusive
        let empty = LpResult::default();
        assert_eq!(
            compare_baseline(&winner, &[empty]).verdict,
            BaselineVerdict::Inconclusive
        );
    }
}
