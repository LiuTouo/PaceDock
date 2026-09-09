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
}
