use super::*;
use crate::benchmark::physical::{self, CoreTarget, RankingStatus, METHOD_VERSION};

pub fn tested_target(
    detail: &SessionDetail,
    topo: &Topology,
    identity: &CpuIdentity,
    core_id: u32,
) -> Result<CoreTarget, String> {
    let fail = || codes::BENCHMARK_SESSION_INCOMPATIBLE.to_string();
    let quick = detail.summary.quick.as_ref().ok_or_else(fail)?;
    if detail.summary.status != SessionStatus::Completed
        || detail.summary.error.is_some()
        || detail.summary.config.method_version != METHOD_VERSION
        || quick.method_version != METHOD_VERSION
        || !detail.summary.capture_quality.integrity_passed
        || !detail.summary.environment_stability.passed
        || detail.summary.cpu_fingerprint != cpu_fingerprint_with(topo, identity)
        || quick.status == RankingStatus::Insufficient
    {
        return Err(fail());
    }
    let current = physical::select(topo, &detail.summary.config.candidate_core_ids)?;
    if current != quick.candidates {
        return Err(fail());
    }
    let screening = physical::rank(&detail.screening_results, &current);
    let retest = physical::rank(&detail.refinement_results, &current);
    if physical::assess(&screening, &retest, current.len()).0 != quick.status
        || retest.len() != current.len().min(2)
        || retest
            .iter()
            .map(|r| r.target.core_id)
            .collect::<std::collections::HashSet<_>>()
            .len()
            != retest.len()
    {
        return Err(fail());
    }
    let target = retest
        .iter()
        .find(|r| r.target.core_id == core_id)
        .ok_or_else(fail)?
        .target
        .clone();
    physical::mask(&target, topo)?;
    Ok(target)
}

impl BenchmarkManager {
    pub(crate) fn apply_core(
        &self,
        _guard: GpuOperationGuard,
        topo: &Topology,
        instance: &str,
        core_id: u32,
        session_id: Option<&str>,
    ) -> Result<(), String> {
        self.apply_core_unlocked_at(
            topo,
            &detect_cpu_identity(),
            instance,
            core_id,
            session_id,
            &storage::benchmarks_root(),
            &recovery::recovery_path(),
            &restore_record_path(),
        )
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub fn apply_core_at(
        &self,
        topo: &Topology,
        identity: &CpuIdentity,
        instance: &str,
        core_id: u32,
        session_id: Option<&str>,
        root: &Path,
        journal: &Path,
        restore: &Path,
    ) -> Result<(), String> {
        let _guard = self.reserve(OP_MUTATION)?;
        self.apply_core_unlocked_at(
            topo, identity, instance, core_id, session_id, root, journal, restore,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_core_unlocked_at(
        &self,
        topo: &Topology,
        identity: &CpuIdentity,
        instance: &str,
        core_id: u32,
        session_id: Option<&str>,
        root: &Path,
        journal: &Path,
        restore: &Path,
    ) -> Result<(), String> {
        if self.recovery_required() {
            return Err(codes::BENCHMARK_RECOVERY_REQUIRED.into());
        }
        let target = if let Some(id) = session_id {
            let detail = storage::get_at_verified(root, id)?;
            if detail.summary.id != id
                || !detail
                    .summary
                    .gpu_instance_id
                    .eq_ignore_ascii_case(instance)
            {
                return Err(codes::BENCHMARK_SESSION_INCOMPATIBLE.into());
            }
            tested_target(&detail, topo, identity, core_id)?
        } else {
            physical::select(topo, &[core_id])?.remove(0)
        };
        let bytes = physical::mask(&target, topo)?;
        if !self
            .backend
            .enumerate_present_adapters()
            .map_err(|e| e.code().to_string())?
            .iter()
            .any(|g| g.instance_id.eq_ignore_ascii_case(instance))
        {
            return Err(codes::GPU_NOT_FOUND.into());
        }
        if !self
            .backend
            .basic_display_enabled()
            .map_err(|e| e.code().to_string())?
        {
            return Err(codes::GPU_BASIC_DISPLAY_DISABLED.into());
        }
        let result = apply_mask_to_gpu(
            self.backend.as_ref(),
            self.sleeper.as_ref(),
            instance,
            bytes.clone(),
            journal,
            restore,
        );
        self.flag_recovery_if_needed(&result);
        if result.is_ok() {
            // 記住套用內容供漂移偵測；記錄寫入失敗只降級（不擋套用）。
            let record = AppliedPolicyRecord {
                instance_id: instance.to_string(),
                core_id,
                lp_indices: target.lp_indices.iter().map(|&l| l as u16).collect(),
                override_bytes: bytes,
                applied_at: chrono::Local::now().to_rfc3339(),
            };
            if let Err(e) = write_applied_record(&applied_record_path(), &record) {
                log::warn!("套用記錄寫入失敗（漂移偵測將無資料）: {e}");
            }
        }
        result.map_err(|e| e.code)
    }
}
