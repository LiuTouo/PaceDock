//! 快速兩階段流程；所有 mutation、capture、清理均共用既有保護。
use super::*;
use crate::benchmark::physical::{self, QuickResult, RankingStatus, METHOD_VERSION};

pub fn run(ctx: &mut RunContext) -> RunResult {
    let (instance, gpu_name) = match pre_flight(ctx) {
        Ok(v) => v,
        Err(e) => return abort(ctx, e),
    };
    let targets = match physical::select(&ctx.topo, &ctx.config.candidate_core_ids) {
        Ok(v) => v,
        Err(e) => return abort(ctx, e),
    };
    let _layout_guard = match prepare_window_layout(
        ctx.window_control.clone(),
        (ctx.config.width, ctx.config.height),
    ) {
        Ok(g) => {
            ctx.layout = Some(g.plan);
            g
        }
        Err(e) => return abort(ctx, e),
    };
    let seed = u32::from_le_bytes(uuid::Uuid::new_v4().as_bytes()[..4].try_into().unwrap());
    let ids: Vec<_> = targets.iter().map(|c| c.core_id).collect();
    let order = physical::shuffled(&ids, seed);
    let schedule = physical::schedule(&ctx.config, targets.len());
    let total = schedule.candidate_captures;
    let mut detail = SessionDetail {
        summary: SessionSummary {
            id: ctx.session_id.clone(),
            status: SessionStatus::Running,
            started_at: chrono::Local::now().to_rfc3339(),
            gpu_name,
            gpu_instance_id: instance.clone(),
            cpu_fingerprint: cpu_fingerprint_with(&ctx.topo, &ctx.cpu_identity),
            config: ctx.config.clone(),
            quick: Some(QuickResult {
                method_version: METHOD_VERSION,
                candidates: targets.clone(),
                seed,
                screening_order: order.clone(),
                retest_order: vec![],
                schedule,
                screening: vec![],
                retest: vec![],
                status: RankingStatus::Insufficient,
                relative_gap_pct: None,
            }),
            ..Default::default()
        },
        ..Default::default()
    };
    if let Err(e) = storage::save_session_at(&ctx.storage_root, &detail) {
        return finish(ctx, detail, Some(e));
    }
    let gate = env::environment_gate(ctx.env.as_ref()).and_then(|_| {
        env::wait_for_cpu_idle(ctx.env.as_ref(), ctx.sleeper.as_ref(), &|| {
            ctx.cancel.is_cancelled()
        })
    });
    if let Err(e) = gate {
        return finish(ctx, detail, Some(e));
    }
    let dir = ctx.storage_root.join(&ctx.session_id);
    let (cap, buffer) = match calibrate(ctx, &dir, &detail) {
        Ok(v) => v,
        Err(e) => return finish(ctx, detail, Some(e)),
    };
    detail.summary.capture_quality.effective_fps_cap = cap;
    detail.summary.capture_quality.circular_buffer_size = buffer;
    let mut csvs = RoundCsvs::new();
    let mut done = 0;
    let mut retest_order = vec![];
    let original = ctx.config.clone();
    for phase in 0..2 {
        let phase_order = if phase == 0 {
            order.clone()
        } else {
            retest_order.clone()
        };
        if phase == 1 {
            let schedule = &detail.summary.quick.as_ref().unwrap().schedule;
            ctx.config.warm_up_secs = schedule.retest_warmup_secs;
            ctx.config.sample_secs = schedule.retest_sample_secs;
        }
        for id in phase_order {
            if ctx.cancel.is_cancelled() {
                ctx.config = original;
                return finish(ctx, detail, Some("cancelled".into()));
            }
            let target = targets.iter().find(|t| t.core_id == id).unwrap();
            let result = capture_step(
                ctx,
                &instance,
                phase,
                target.lp_indices[0],
                &dir,
                &mut csvs,
                done,
                total,
                &detail,
                cap,
                buffer,
            );
            // 分階段保存，任何部分失敗不會把短篩選混入複測排名。
            let rows = compute_phase_results(&csvs, phase, phase + 1);
            let ranked = physical::rank(&rows, &targets);
            if phase == 0 {
                detail.screening_results = rows;
                detail.summary.quick.as_mut().unwrap().screening = ranked;
            } else {
                detail.results = rows.clone();
                detail.refinement_results = rows;
                detail.summary.quick.as_mut().unwrap().retest = ranked;
            }
            match result {
                StepOutcome::Continue => {}
                StepOutcome::Break(TerminalReason::Cancelled) => {
                    ctx.config = original;
                    return finish(ctx, detail, Some("cancelled".into()));
                }
                StepOutcome::Isolated(e) | StepOutcome::Break(TerminalReason::Error(e)) => {
                    ctx.config = original;
                    return finish(ctx, detail, Some(e));
                }
            }
            done += 1;
            if let Err(e) = storage::save_session_at(&ctx.storage_root, &detail) {
                ctx.config = original;
                return finish(ctx, detail, Some(e));
            }
        }
        if phase == 0 {
            let quick = detail.summary.quick.as_mut().unwrap();
            if quick.screening.len() != targets.len() {
                ctx.config = original;
                return finish(ctx, detail, Some(codes::BENCHMARK_CSV_INVALID.into()));
            }
            retest_order = order
                .iter()
                .rev()
                .filter(|id| {
                    quick
                        .screening
                        .iter()
                        .take(2)
                        .any(|r| r.target.core_id == **id)
                })
                .copied()
                .collect();
            quick.retest_order = retest_order.clone();
        }
    }
    ctx.config = original;
    let quick = detail.summary.quick.as_mut().unwrap();
    (quick.status, quick.relative_gap_pct) =
        physical::assess(&quick.screening, &quick.retest, targets.len());
    let error =
        (quick.status == RankingStatus::Insufficient).then(|| codes::BENCHMARK_CSV_INVALID.into());
    finish(ctx, detail, error)
}

fn finish(ctx: &mut RunContext, mut detail: SessionDetail, error: Option<String>) -> RunResult {
    let status = match error.as_deref() {
        Some("cancelled") => SessionStatus::Cancelled,
        Some(_) => SessionStatus::Failed,
        None => SessionStatus::Completed,
    };
    detail.summary.sample_count = detail
        .screening_results
        .iter()
        .chain(&detail.refinement_results)
        .map(|r| r.sample_count)
        .sum();
    detail.summary.environment_stability.passed = status == SessionStatus::Completed;
    terminal(
        ctx,
        detail,
        status,
        error.filter(|e| e != "cancelled"),
        None,
        vec![],
        vec![],
    )
}
