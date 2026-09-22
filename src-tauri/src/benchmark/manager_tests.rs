//! manager 的單元測試(自 manager.rs 機械搬移,路徑不變:super = manager)。
use super::*;
use crate::benchmark::{
    cpu_fingerprint_with, CpuIdentity, ReliabilityStatus, ReliabilitySummary, SessionDetail,
};
use crate::gpu::fake::FakeBackend;
use crate::gpu::{AffinityPolicy, GpuDevice, NoopSleeper, RegistryValueSnapshot};
use crate::topology::{build_topology, Topology};
use uuid::Uuid;
use windows::Win32::System::Registry::{REG_BINARY, REG_DWORD};

const GPU_A: &str = r"PCI\VEN_FAKE&DEV_1";
const GPU_B: &str = r"PCI\VEN_FAKE&DEV_2";

/// 測試用的固定 CPU 身分（x64 Intel），讓指紋完全確定、不依賴真實機器
fn fixed_identity() -> CpuIdentity {
    CpuIdentity {
        architecture: 9, // PROCESSOR_ARCHITECTURE_AMD64
        family: 6,
        model: 183,
        stepping: 1,
    }
}

fn topo() -> Topology {
    build_topology((0..8u32).map(|c| (vec![c], 0, false)).collect())
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pacedock_mgr_{}_{}", std::process::id(), name));
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_file(dir.join("journal.json"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn device(instance: &str) -> GpuDevice {
    GpuDevice {
        instance_id: instance.to_string(),
        friendly_name: format!("GPU {instance}"),
    }
}

/// 測試用「已通過可靠性」的摘要（status=Passed）
fn passed_reliability() -> ReliabilitySummary {
    ReliabilitySummary {
        status: ReliabilityStatus::Passed,
        ..Default::default()
    }
}

fn completed_session(storage_root: &Path, topo: &Topology, gpu: &str, best_lp: u32) -> String {
    let id = Uuid::new_v4().to_string();
    let detail = SessionDetail {
        summary: crate::benchmark::SessionSummary {
            id: id.clone(),
            status: SessionStatus::Completed,
            started_at: "2026-08-11T00:00:00Z".into(),
            finished_at: Some("2026-08-11T00:01:00Z".into()),
            gpu_name: "GPU".into(),
            gpu_instance_id: gpu.to_string(),
            cpu_fingerprint: cpu_fingerprint_with(topo, &fixed_identity()),
            best_lp: Some(best_lp),
            reliability: passed_reliability(),
            severe_lps: vec![],
            sample_count: 5,
            total_bytes: 0,
            config: BenchmarkConfig::default(),
            error: None,
            ..Default::default()
        },
        results: vec![],
        samples: vec![],
        ..Default::default()
    };
    storage::save_session_at(storage_root, &detail).unwrap();
    id
}

/// u32 → 精簡 little-endian bytes（尾端零移除），fixture 用
fn le_trimmed(v: u32) -> Vec<u8> {
    let b = v.to_le_bytes();
    let mut len = b.len();
    while len > 0 && b[len - 1] == 0 {
        len -= 1;
    }
    b[..len].to_vec()
}

/// 既有策略 fixture：DevicePolicy 是 DWORD；AssignmentSetOverride 是 REG_BINARY
fn policy_on(instance: &str, device_policy: u32, override_mask: u32) -> AffinityPolicy {
    AffinityPolicy {
        instance_id: instance.to_string(),
        device_policy: RegistryValueSnapshot::dword(device_policy),
        assignment_set_override: RegistryValueSnapshot::binary(le_trimmed(override_mask)),
    }
}

fn assert_dword(policy: &AffinityPolicy, name: &str, expected: u32) {
    let v = if name == "DevicePolicy" {
        policy.device_policy.as_dword()
    } else {
        policy.assignment_set_override.as_dword()
    };
    assert_eq!(v, Some(expected), "{name} 值不符");
}

/// 斷言 AssignmentSetOverride 是 REG_BINARY，且位元組等於給定 mask 的精簡 LE
fn assert_override_mask(policy: &AffinityPolicy, mask: u32) {
    let o = &policy.assignment_set_override;
    assert_eq!(
        o.value_type,
        Some(REG_BINARY.0),
        "AssignmentSetOverride 應為 REG_BINARY"
    );
    assert_eq!(
        o.bytes.as_deref(),
        Some(le_trimmed(mask).as_slice()),
        "AssignmentSetOverride bytes 不符"
    );
}

#[test]
fn apply_success_writes_policy_restarts_and_records() {
    let dir = temp_dir("ok");
    let storage_root = dir.join("benchmarks");
    let journal = dir.join("journal.json");
    let restore = dir.join("restore.json");
    let backend = FakeBackend::new(vec![device(GPU_A)]);
    // 模擬既有策略（已鎖定 LP 0）
    backend.set_policy(policy_on(GPU_A, 4, 1));

    let sid = completed_session(&storage_root, &topo(), GPU_A, 5);
    apply_best_affinity(
        &backend,
        &NoopSleeper,
        &fixed_identity(),
        &topo(),
        &storage_root,
        &journal,
        &restore,
        &sid,
    )
    .unwrap();

    let cur = backend.current_policy(GPU_A);
    assert_dword(&cur, "DevicePolicy", DEVICE_POLICY_SINGLE_PROCESSOR);
    assert_override_mask(&cur, 1u32 << 5);
    assert_eq!(backend.restart_count(), 1);
    assert_eq!(backend.disable_attempts(), 1);
    assert_eq!(backend.enable_attempts(), 1); // disable 成功後有嘗試 enable
    assert!(!journal.exists(), "成功後日誌應清除");
    assert!(restore.exists(), "一層還原記錄應寫入");
    let _ = std::fs::remove_dir_all(&dir);
}

/// GPU 原本沒有 Affinity Policy 值（present=false）：套用成功，
/// 還原時把兩個值都「刪除」回缺失狀態。
#[test]
fn apply_and_restore_missing_registry_values() {
    let dir = temp_dir("missingvals");
    let storage_root = dir.join("benchmarks");
    let journal = dir.join("journal.json");
    let restore = dir.join("restore.json");
    let backend = FakeBackend::new(vec![device(GPU_A)]);
    // 不 set_policy → 兩值皆不存在
    assert!(!backend.current_policy(GPU_A).device_policy.present);

    let sid = completed_session(&storage_root, &topo(), GPU_A, 3);
    apply_best_affinity(
        &backend,
        &NoopSleeper,
        &fixed_identity(),
        &topo(),
        &storage_root,
        &journal,
        &restore,
        &sid,
    )
    .unwrap();
    assert_override_mask(&backend.current_policy(GPU_A), 1u32 << 3);

    // 還原 → 回到「不存在」
    restore_previous_affinity(&backend, &NoopSleeper, &restore).unwrap();
    let restored = backend.current_policy(GPU_A);
    assert!(!restored.device_policy.present);
    assert!(!restored.assignment_set_override.present);
    assert!(restored.device_policy.bytes.is_none());
    assert!(!restore.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn apply_requires_completed_session() {
    let dir = temp_dir("notcomp");
    let storage_root = dir.join("benchmarks");
    let backend = FakeBackend::new(vec![device(GPU_A)]);
    let id = Uuid::new_v4().to_string();
    let mut detail = SessionDetail::default();
    detail.summary.id = id.clone();
    detail.summary.status = SessionStatus::Running;
    detail.summary.gpu_instance_id = GPU_A.into();
    detail.summary.cpu_fingerprint = cpu_fingerprint_with(&topo(), &fixed_identity());
    storage::save_session_at(&storage_root, &detail).unwrap();

    let err = apply_best_affinity(
        &backend,
        &NoopSleeper,
        &fixed_identity(),
        &topo(),
        &storage_root,
        &dir.join("journal.json"),
        &dir.join("restore.json"),
        &id,
    )
    .unwrap_err();
    assert_eq!(err, codes::BENCHMARK_SESSION_NOT_COMPLETED);
    assert_eq!(backend.restart_count(), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn apply_rejects_cpu_fingerprint_mismatch() {
    let dir = temp_dir("cpumismatch");
    let storage_root = dir.join("benchmarks");
    let backend = FakeBackend::new(vec![device(GPU_A)]);
    // 用「不同 CPU」建立 session（8C16T vs 8C 指紋不同）
    let id = Uuid::new_v4().to_string();
    let other_topo = build_topology(
        (0..8u32)
            .map(|c| (vec![c * 2, c * 2 + 1], 0, true))
            .collect(),
    );
    let mut detail = SessionDetail::default();
    detail.summary.id = id.clone();
    detail.summary.status = SessionStatus::Completed;
    detail.summary.gpu_instance_id = GPU_A.into();
    detail.summary.best_lp = Some(3);
    detail.summary.reliability = passed_reliability();
    detail.summary.cpu_fingerprint = cpu_fingerprint_with(&other_topo, &fixed_identity());
    storage::save_session_at(&storage_root, &detail).unwrap();

    let err = apply_best_affinity(
        &backend,
        &NoopSleeper,
        &fixed_identity(),
        &topo(),
        &storage_root,
        &dir.join("journal.json"),
        &dir.join("restore.json"),
        &id,
    )
    .unwrap_err();
    assert_eq!(err, codes::BENCHMARK_SESSION_INCOMPATIBLE);
    assert_eq!(backend.restart_count(), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn apply_rejects_gpu_not_present() {
    let dir = temp_dir("gpumissing");
    let storage_root = dir.join("benchmarks");
    // 本機只有 GPU_B，session 指向 GPU_A
    let backend = FakeBackend::new(vec![device(GPU_B)]);
    let sid = completed_session(&storage_root, &topo(), GPU_A, 3);
    let err = apply_best_affinity(
        &backend,
        &NoopSleeper,
        &fixed_identity(),
        &topo(),
        &storage_root,
        &dir.join("journal.json"),
        &dir.join("restore.json"),
        &sid,
    )
    .unwrap_err();
    assert_eq!(err, codes::GPU_NOT_FOUND);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn apply_blocks_when_basic_display_disabled() {
    let dir = temp_dir("basic");
    let storage_root = dir.join("benchmarks");
    let backend = FakeBackend::new(vec![device(GPU_A)]);
    backend.basic_display_on.store(false, Ordering::SeqCst);
    let sid = completed_session(&storage_root, &topo(), GPU_A, 3);
    let err = apply_best_affinity(
        &backend,
        &NoopSleeper,
        &fixed_identity(),
        &topo(),
        &storage_root,
        &dir.join("journal.json"),
        &dir.join("restore.json"),
        &sid,
    )
    .unwrap_err();
    assert_eq!(err, codes::GPU_BASIC_DISPLAY_DISABLED);
    assert_eq!(backend.restart_count(), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 64-LP 拓撲（group 0 上限）
fn topo_64() -> Topology {
    build_topology(
        (0..32u32)
            .map(|c| (vec![c * 2, c * 2 + 1], 0, true))
            .collect(),
    )
}

/// LP 0/31/32/63 都能套用：AssignmentSetOverride 是 REG_BINARY 且位元組精確
#[test]
fn apply_lp_boundaries_write_reg_binary_masks() {
    let dir = temp_dir("lpbound");
    let storage_root = dir.join("benchmarks");
    let journal = dir.join("journal.json");
    let restore = dir.join("restore.json");
    let backend = FakeBackend::new(vec![device(GPU_A)]);
    let topo64 = topo_64();
    assert_eq!(topo64.total_lp, 64);

    for lp in [0u32, 31, 32, 63] {
        let sid = completed_session(&storage_root, &topo64, GPU_A, lp);
        apply_best_affinity(
            &backend,
            &NoopSleeper,
            &fixed_identity(),
            &topo64,
            &storage_root,
            &journal,
            &restore,
            &sid,
        )
        .unwrap();
        let cur = backend.current_policy(GPU_A);
        assert_eq!(
            cur.assignment_set_override.value_type,
            Some(REG_BINARY.0),
            "LP {lp} 應為 REG_BINARY"
        );
        assert_eq!(
            cur.assignment_set_override.bytes.as_deref(),
            Some(single_lp_mask_bytes(lp).as_slice()),
            "LP {lp} mask bytes 不符"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn apply_rejects_best_lp_64() {
    let dir = temp_dir("lp64");
    let storage_root = dir.join("benchmarks");
    let backend = FakeBackend::new(vec![device(GPU_A)]);
    let topo64 = topo_64();
    let sid = completed_session(&storage_root, &topo64, GPU_A, 64);
    let err = apply_best_affinity(
        &backend,
        &NoopSleeper,
        &fixed_identity(),
        &topo64,
        &storage_root,
        &dir.join("journal.json"),
        &dir.join("restore.json"),
        &sid,
    )
    .unwrap_err();
    assert_eq!(err, codes::BENCHMARK_SESSION_INCOMPATIBLE);
    assert_eq!(backend.restart_count(), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn apply_rejects_best_lp_outside_topology() {
    let dir = temp_dir("lpoutside");
    let storage_root = dir.join("benchmarks");
    let backend = FakeBackend::new(vec![device(GPU_A)]);
    // 8-LP 拓撲，best_lp=8 超出實際 LP 範圍
    let sid = completed_session(&storage_root, &topo(), GPU_A, 8);
    let err = apply_best_affinity(
        &backend,
        &NoopSleeper,
        &fixed_identity(),
        &topo(),
        &storage_root,
        &dir.join("journal.json"),
        &dir.join("restore.json"),
        &sid,
    )
    .unwrap_err();
    assert_eq!(err, codes::BENCHMARK_SESSION_INCOMPATIBLE);
    assert_eq!(backend.restart_count(), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 原本策略含非 DWORD 型別：還原必須逐型別、逐位元組還原
#[test]
fn restore_rejects_non_dword_device_policy() {
    let dir = temp_dir("non_dword");
    let storage_root = dir.join("benchmarks");
    let journal = dir.join("journal.json");
    let restore = dir.join("restore.json");
    let backend = FakeBackend::new(vec![device(GPU_A)]);
    // DevicePolicy=REG_SZ：真實驅動語意只接受 DWORD；非 DWORD 快照一律
    // fail closed（不得讓任意型別/bytes 進入 HKLM）
    let original = AffinityPolicy {
        instance_id: GPU_A.to_string(),
        device_policy: RegistryValueSnapshot {
            present: true,
            value_type: Some(1), // REG_SZ
            bytes: Some(vec![b'g', 0, b'p', 0, 0, 0]),
        },
        assignment_set_override: RegistryValueSnapshot {
            present: true,
            value_type: Some(REG_BINARY.0),
            bytes: Some(vec![0x00, 0x00, 0x00, 0x80]),
        },
    };
    backend.set_policy(original.clone());
    let sid = completed_session(&storage_root, &topo(), GPU_A, 4);
    apply_best_affinity(
        &backend,
        &NoopSleeper,
        &fixed_identity(),
        &topo(),
        &storage_root,
        &journal,
        &restore,
        &sid,
    )
    .unwrap();
    // 快照（非 DWORD）寫回還原記錄成功，但 restore 必須在 write 前被語意
    // 驗證擋下 → 還原失敗，裝置狀態維持在套用後的 DWORD 策略
    assert_eq!(
        backend.current_policy(GPU_A).device_policy.value_type,
        Some(REG_DWORD.0)
    );
    assert!(restore_previous_affinity(&backend, &NoopSleeper, &restore).is_err());
    assert_eq!(
        backend.current_policy(GPU_A).device_policy.value_type,
        Some(REG_DWORD.0),
        "非 DWORD 快照不得被寫入"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn apply_write_failure_restores_and_clears_journal() {
    let dir = temp_dir("writefail");
    let storage_root = dir.join("benchmarks");
    let journal = dir.join("journal.json");
    let restore = dir.join("restore.json");
    let backend = FakeBackend::new(vec![device(GPU_A)]);
    let original = policy_on(GPU_A, 2, 0b1);
    backend.set_policy(original.clone());
    let sid = completed_session(&storage_root, &topo(), GPU_A, 4);
    backend.fail_next_write(); // 第一次 write 失敗（apply 的 write）

    let err = apply_best_affinity(
        &backend,
        &NoopSleeper,
        &fixed_identity(),
        &topo(),
        &storage_root,
        &journal,
        &restore,
        &sid,
    )
    .unwrap_err();
    assert_eq!(err, codes::GPU_APPLY_FAILED);
    // 還原成功 → 策略與原快照逐位元組一致，日誌清除
    assert_eq!(backend.current_policy(GPU_A), original);
    assert!(!journal.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn apply_restart_disable_failure_restores_and_clears_journal() {
    let dir = temp_dir("restartfail");
    let storage_root = dir.join("benchmarks");
    let journal = dir.join("journal.json");
    let restore = dir.join("restore.json");
    let backend = FakeBackend::new(vec![device(GPU_A)]);
    let original = policy_on(GPU_A, 1, 0b11);
    backend.set_policy(original.clone());
    let sid = completed_session(&storage_root, &topo(), GPU_A, 4);
    backend.fail_next_restart.store(true, Ordering::SeqCst);

    let err = apply_best_affinity(
        &backend,
        &NoopSleeper,
        &fixed_identity(),
        &topo(),
        &storage_root,
        &journal,
        &restore,
        &sid,
    )
    .unwrap_err();
    assert_eq!(err, codes::GPU_RESTART_FAILED);
    // 套用 restart 失敗後，還原 restart 成功 → 策略還原、日誌清除
    assert_eq!(backend.current_policy(GPU_A), original);
    assert!(!journal.exists());
    assert!(!restore.exists(), "套用未完成，不該寫入還原記錄");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn apply_restart_failure_and_restore_failure_keeps_journal() {
    let dir = temp_dir("restorefail");
    let storage_root = dir.join("benchmarks");
    let journal = dir.join("journal.json");
    let restore = dir.join("restore.json");
    let backend = FakeBackend::new(vec![device(GPU_A)]);
    let original = policy_on(GPU_A, 1, 0b11);
    backend.set_policy(original.clone());
    let sid = completed_session(&storage_root, &topo(), GPU_A, 4);
    backend.disable_fails.store(true, Ordering::SeqCst); // 一直失敗

    let err = apply_best_affinity(
        &backend,
        &NoopSleeper,
        &fixed_identity(),
        &topo(),
        &storage_root,
        &journal,
        &restore,
        &sid,
    )
    .unwrap_err();
    assert_eq!(err, codes::GPU_RESTART_FAILED);
    // 策略已寫回（write 成功），但 restart 失敗 → 還原未驗證 → 保留日誌等啟動重試
    assert_eq!(backend.current_policy(GPU_A), original);
    assert!(journal.exists(), "還原失敗時必須保留日誌");
    assert!(!restore.exists(), "還原記錄不該寫入");
    assert_eq!(backend.enable_attempts(), 0, "disable 失敗不該嘗試 enable");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn apply_enable_failure_attempts_enable_after_disable() {
    let dir = temp_dir("enablefail");
    let storage_root = dir.join("benchmarks");
    let journal = dir.join("journal.json");
    let backend = FakeBackend::new(vec![device(GPU_A)]);
    let original = policy_on(GPU_A, 1, 0b11);
    backend.set_policy(original.clone());
    let sid = completed_session(&storage_root, &topo(), GPU_A, 4);
    backend.enable_fails.store(true, Ordering::SeqCst);

    let err = apply_best_affinity(
        &backend,
        &NoopSleeper,
        &fixed_identity(),
        &topo(),
        &storage_root,
        &journal,
        &dir.join("restore.json"),
        &sid,
    )
    .unwrap_err();
    assert_eq!(err, codes::GPU_RESTART_FAILED);
    // 每次 restart：disable 成功 → 必嘗試 enable（總共 apply + restore 兩次）
    assert!(
        backend.enable_attempts() >= 2,
        "disable 成功後必須嘗試 enable"
    );
    assert_eq!(backend.current_policy(GPU_A), original);
    assert!(journal.exists(), "還原失敗 → 保留日誌");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn restore_previous_restores_bytes_and_clears_record() {
    let dir = temp_dir("restoreprev");
    let storage_root = dir.join("benchmarks");
    let journal = dir.join("journal.json");
    let restore = dir.join("restore.json");
    let backend = FakeBackend::new(vec![device(GPU_A)]);
    let original = policy_on(GPU_A, 0, 0xFFFF);
    backend.set_policy(original.clone());
    let sid = completed_session(&storage_root, &topo(), GPU_A, 5);

    apply_best_affinity(
        &backend,
        &NoopSleeper,
        &fixed_identity(),
        &topo(),
        &storage_root,
        &journal,
        &restore,
        &sid,
    )
    .unwrap();
    // 現在策略已被改寫
    assert_override_mask(&backend.current_policy(GPU_A), 1u32 << 5);

    restore_previous_affinity(&backend, &NoopSleeper, &restore).unwrap();
    assert_eq!(backend.current_policy(GPU_A), original, "必須逐位元組還原");
    assert!(!restore.exists(), "還原後記錄清除");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn restore_previous_without_record_errors() {
    let dir = temp_dir("norecord");
    let backend = FakeBackend::new(vec![device(GPU_A)]);
    let err =
        restore_previous_affinity(&backend, &NoopSleeper, &dir.join("restore.json")).unwrap_err();
    assert_eq!(err, codes::GPU_RESTORE_FAILED);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn apply_twice_is_one_level_record() {
    let dir = temp_dir("onelevel");
    let storage_root = dir.join("benchmarks");
    let journal = dir.join("journal.json");
    let restore = dir.join("restore.json");
    let backend = FakeBackend::new(vec![device(GPU_A), device(GPU_B)]);
    let orig_a = policy_on(GPU_A, 0, 0xFFFF);
    let orig_b = policy_on(GPU_B, 0, 0xFF00);
    backend.set_policy(orig_a.clone());
    backend.set_policy(orig_b.clone());

    let sid_a = completed_session(&storage_root, &topo(), GPU_A, 1);
    apply_best_affinity(
        &backend,
        &NoopSleeper,
        &fixed_identity(),
        &topo(),
        &storage_root,
        &journal,
        &restore,
        &sid_a,
    )
    .unwrap();
    let sid_b = completed_session(&storage_root, &topo(), GPU_B, 2);
    apply_best_affinity(
        &backend,
        &NoopSleeper,
        &fixed_identity(),
        &topo(),
        &storage_root,
        &journal,
        &restore,
        &sid_b,
    )
    .unwrap();

    // 只保留最新一次的快照
    let rec = std::fs::read_to_string(&restore).unwrap();
    let saved: AffinityPolicy = serde_json::from_str(&rec).unwrap();
    assert_eq!(saved.instance_id, GPU_B);
    assert_eq!(saved, orig_b);

    // 還原 → 回到 GPU_B 原狀（GPU_A 維持套用後）
    restore_previous_affinity(&backend, &NoopSleeper, &restore).unwrap();
    assert_eq!(backend.current_policy(GPU_B), orig_b);
    assert_override_mask(&backend.current_policy(GPU_A), 1u32 << 1);
    let _ = std::fs::remove_dir_all(&dir);
}

// ── 啟動還原 ──

#[test]
fn startup_recovery_no_journal_ok() {
    let dir = temp_dir("recovery_none");
    let backend = FakeBackend::new(vec![device(GPU_A)]);
    assert!(attempt_startup_recovery(&backend, &NoopSleeper, &dir.join("journal.json")).is_ok());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn startup_recovery_restores_policy_applied_crash() {
    let dir = temp_dir("recovery_crash");
    let journal = dir.join("journal.json");
    let backend = FakeBackend::new(vec![device(GPU_A)]);
    let original = policy_on(GPU_A, 2, 0b1);
    backend.set_policy(original.clone());

    // 模擬崩潰：快照後已寫入新策略、還未完成（stage=PolicyApplied）
    recovery::begin_at(&journal, &original).unwrap();
    let j = recovery::load_from(&journal).unwrap().unwrap();
    recovery::advance_to_at(&journal, &j, RecoveryStage::PolicyApplied).unwrap();
    backend.set_policy(policy_on(GPU_A, 4, 1u32 << 7)); // 假設套用寫入的新值

    attempt_startup_recovery(&backend, &NoopSleeper, &journal).unwrap();
    assert_eq!(backend.current_policy(GPU_A), original, "必須還原到快照");
    assert!(!journal.exists(), "還原驗證成功後清除日誌");
    assert_eq!(backend.restart_count(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn startup_recovery_stage_snapshot_only_clears_without_mutation() {
    let dir = temp_dir("recovery_snapshot");
    let journal = dir.join("journal.json");
    let backend = FakeBackend::new(vec![device(GPU_A)]);
    let original = policy_on(GPU_A, 1, 0b11);
    backend.set_policy(original.clone());
    recovery::begin_at(&journal, &original).unwrap(); // stage=SnapshotTaken，尚未變更

    attempt_startup_recovery(&backend, &NoopSleeper, &journal).unwrap();
    assert_eq!(backend.current_policy(GPU_A), original);
    assert_eq!(backend.restart_count(), 0, "未變更不該重啟裝置");
    assert!(!journal.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn startup_recovery_snapshot_stage_mismatch_is_error() {
    let dir = temp_dir("recovery_mismatch");
    let journal = dir.join("journal.json");
    let backend = FakeBackend::new(vec![device(GPU_A)]);
    let original = policy_on(GPU_A, 1, 0b11);
    recovery::begin_at(&journal, &original).unwrap();
    // 策略被改了（stage 卻停在 SnapshotTaken → 資料異常）
    backend.set_policy(policy_on(GPU_A, 4, 1));

    assert!(attempt_startup_recovery(&backend, &NoopSleeper, &journal).is_err());
    assert!(journal.exists(), "驗證失敗不該清除日誌");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn startup_recovery_restore_failure_keeps_journal() {
    let dir = temp_dir("recovery_restorefail");
    let journal = dir.join("journal.json");
    let backend = FakeBackend::new(vec![device(GPU_A)]);
    let original = policy_on(GPU_A, 2, 0b1);
    recovery::begin_at(&journal, &original).unwrap();
    let j = recovery::load_from(&journal).unwrap().unwrap();
    recovery::advance_to_at(&journal, &j, RecoveryStage::PolicyApplied).unwrap();
    backend.disable_fails.store(true, Ordering::SeqCst); // 還原 restart 一直失敗

    assert!(attempt_startup_recovery(&backend, &NoopSleeper, &journal).is_err());
    assert!(journal.exists(), "還原失敗必須保留日誌供下次啟動");
    let _ = std::fs::remove_dir_all(&dir);
}

// ── BenchmarkManager 狀態骨架 ──

#[test]
fn manager_state_default_and_cancel() {
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)])) as Arc<dyn GpuBackend>;
    let m = BenchmarkManager::new(backend);
    let s = m.state_snapshot();
    assert_eq!(s.status, SessionStatus::Pending);
    assert!(!s.recovery_required);
    assert!(!m.cancel_requested());
    assert_eq!(s.cancel_stage, None);
    assert_eq!(s.cancel_progress, None);
    m.request_cancel();
    assert!(m.cancel_requested());
    let sc = m.state_snapshot();
    assert!(sc.cancel_requested);
    // request_cancel 立即在 state 標記 requested/0%，供前端立刻顯示「已收到取消請求」。
    assert_eq!(sc.cancel_stage.as_deref(), Some("requested"));
    assert_eq!(sc.cancel_progress, Some(0));
    // reset_cancel 歸零取消欄位（下一場 session 不再殘留 requested/0%）。
    m.reset_cancel();
    let s2 = m.state_snapshot();
    assert!(!s2.cancel_requested);
    assert_eq!(s2.cancel_stage, None);
    assert_eq!(s2.cancel_progress, None);
}

/// exit guard：Running 阻擋退出，Idle/Completed 允許
#[test]
fn refuse_exit_blocks_running_allows_idle_and_completed() {
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)])) as Arc<dyn GpuBackend>;
    let m = BenchmarkManager::new(backend);

    // 初始（Idle）→ 允許
    assert!(m.refuse_exit_if_running().is_ok());
    assert!(!m.is_running());

    // Running → 拒絕
    m.state.write().unwrap().status = SessionStatus::Running;
    assert!(m.is_running());
    assert!(m.refuse_exit_if_running().is_err());

    // Completed / Failed / Cancelled → 允許
    for st in [
        SessionStatus::Completed,
        SessionStatus::Failed,
        SessionStatus::Cancelled,
    ] {
        m.state.write().unwrap().status = st;
        assert!(!m.is_running(), "{st:?} 不該算執行中");
        assert!(m.refuse_exit_if_running().is_ok(), "{st:?} 應允許退出");
    }
}

#[test]
fn validate_config_rejects_bad_settings() {
    let t = topo();
    // sample=0
    let c = BenchmarkConfig {
        sample_secs: 0,
        ..Default::default()
    };
    assert!(runner::validate_config(&c, &t).is_err());
    // repetitions 越界
    let c = BenchmarkConfig {
        repetitions: 8,
        ..Default::default()
    };
    assert!(runner::validate_config(&c, &t).is_err());
    // Vulkan 但無 args
    let c = BenchmarkConfig {
        vulkan_args: vec![],
        ..Default::default()
    };
    assert!(runner::validate_config(&c, &t).is_err());
    // 沒 GPU
    let c = BenchmarkConfig {
        gpu_instance_id: None,
        ..Default::default()
    };
    assert!(runner::validate_config(&c, &t).is_err());
    // 合法預設（gpu 補上）
    let c = BenchmarkConfig {
        gpu_instance_id: Some(GPU_A.to_string()),
        ..Default::default()
    };
    assert!(runner::validate_config(&c, &t).is_ok());
}

#[test]
fn effective_lps_excludes_core_zero() {
    let t = topo(); // 8 LP（core c = LP c，故 core 0 = LP 0）
    let c = BenchmarkConfig::default();
    // 無 SMT 均質拓撲：預設 = 所有 primary LP（= 全部 LP），排除 physical Core 0 的 LP 0
    assert_eq!(runner::effective_lps(&c, &t), vec![1, 2, 3, 4, 5, 6, 7]);
    // 候選過濾 + 去重 + 排序（LP 0 已在候選外，故不變）
    let c = BenchmarkConfig {
        candidate_lps: vec![5, 1, 5, 99],
        ..Default::default()
    };
    assert_eq!(runner::effective_lps(&c, &t), vec![1, 5]);
    // 顯式只含 core 0 的候選 → 過濾後為空（由 validate_config 拒絕）
    let c = BenchmarkConfig {
        candidate_lps: vec![0],
        ..Default::default()
    };
    assert!(runner::effective_lps(&c, &t).is_empty());
}

#[test]
fn effective_lps_default_excludes_smt_siblings() {
    // 8C16T SMT：sibling 與 primary 同一顆物理核心（GPU policy 綁單一 LP），
    // 預設只測 primary，排除 physical Core 0（LP 0,1）
    let cores: Vec<(Vec<u32>, u8, bool)> = (0..8u32)
        .map(|c| (vec![c * 2, c * 2 + 1], 0, true))
        .collect();
    let t = crate::topology::build_topology(cores);
    let c = BenchmarkConfig::default();
    assert_eq!(runner::effective_lps(&c, &t), vec![2, 4, 6, 8, 10, 12, 14]);
}

#[test]
fn effective_lps_default_p_core_only_on_hybrid() {
    // 8P(HT)+8E：預設只測 P-core primary；E-core 不當 GPU 中斷目標，不測
    let mut cores: Vec<(Vec<u32>, u8, bool)> = Vec::new();
    let mut lp = 0u32;
    for _ in 0..8 {
        cores.push((vec![lp, lp + 1], 1, true));
        lp += 2;
    }
    for _ in 0..8 {
        cores.push((vec![lp], 0, false));
        lp += 1;
    }
    let t = crate::topology::build_topology(cores);
    let c = BenchmarkConfig::default();
    assert_eq!(runner::effective_lps(&c, &t), vec![2, 4, 6, 8, 10, 12, 14]);
}

#[test]
fn round_order_balanced_rotation_and_reversal() {
    let lps = vec![0, 2, 4, 6];
    // 每輪起點旋轉、奇數輪反轉：不重複、不遺漏 LP
    assert_eq!(runner::round_order(0, &lps), vec![0, 2, 4, 6]);
    assert_eq!(runner::round_order(1, &lps), vec![2, 0, 6, 4]);
    assert_eq!(runner::round_order(2, &lps), vec![4, 6, 0, 2]);
    assert_eq!(runner::round_order(3, &lps), vec![6, 4, 2, 0]);
}

/// list_importable 只回傳「已完成 + CPU 相容 + GPU 存在 + 有 bestLp」的 session
#[test]
fn list_importable_filters_by_compatibility() {
    let dir = temp_dir("importable");
    let root = dir.join("benchmarks");
    let backend = FakeBackend::new(vec![device(GPU_A)]);

    // 相容已完成（GPU_A + fixed identity 指紋 + bestLp）
    let ok = completed_session(&root, &topo(), GPU_A, 3);

    // 不相容 CPU（不同拓撲指紋）
    let other_topo = build_topology(
        (0..8u32)
            .map(|c| (vec![c * 2, c * 2 + 1], 0, true))
            .collect(),
    );
    let bad_cpu = {
        let id = Uuid::new_v4().to_string();
        let detail = crate::benchmark::SessionDetail {
            summary: crate::benchmark::SessionSummary {
                id: id.clone(),
                status: SessionStatus::Completed,
                started_at: "2026-08-11T00:00:00Z".into(),
                finished_at: Some("2026-08-11T00:01:00Z".into()),
                gpu_name: "GPU".into(),
                gpu_instance_id: GPU_A.to_string(),
                cpu_fingerprint: cpu_fingerprint_with(&other_topo, &fixed_identity()),
                best_lp: Some(3),
                reliability: passed_reliability(),
                severe_lps: vec![],
                sample_count: 5,
                total_bytes: 0,
                config: BenchmarkConfig::default(),
                error: None,
                ..Default::default()
            },
            results: vec![],
            samples: vec![],
            ..Default::default()
        };
        storage::save_session_at(&root, &detail).unwrap();
        id
    };

    // GPU 不存在（本機只有 GPU_A，session 指向 GPU_B）
    let bad_gpu = {
        let id = Uuid::new_v4().to_string();
        let mut detail = crate::benchmark::SessionDetail::default();
        detail.summary.id = id.clone();
        detail.summary.status = SessionStatus::Completed;
        detail.summary.gpu_instance_id = GPU_B.to_string();
        detail.summary.cpu_fingerprint = cpu_fingerprint_with(&topo(), &fixed_identity());
        detail.summary.best_lp = Some(1);
        storage::save_session_at(&root, &detail).unwrap();
        id
    };

    // 未完成（Running）
    let running = {
        let id = Uuid::new_v4().to_string();
        let mut detail = crate::benchmark::SessionDetail::default();
        detail.summary.id = id.clone();
        detail.summary.status = SessionStatus::Running;
        detail.summary.gpu_instance_id = GPU_A.to_string();
        detail.summary.cpu_fingerprint = cpu_fingerprint_with(&topo(), &fixed_identity());
        storage::save_session_at(&root, &detail).unwrap();
        id
    };

    // 已完成但無 bestLp
    let no_best = {
        let id = Uuid::new_v4().to_string();
        let mut detail = crate::benchmark::SessionDetail::default();
        detail.summary.id = id.clone();
        detail.summary.status = SessionStatus::Completed;
        detail.summary.gpu_instance_id = GPU_A.to_string();
        detail.summary.cpu_fingerprint = cpu_fingerprint_with(&topo(), &fixed_identity());
        storage::save_session_at(&root, &detail).unwrap();
        id
    };

    let list = list_importable(&backend, &topo(), &fixed_identity(), &root);
    let ids: Vec<String> = list.iter().map(|s| s.id.clone()).collect();
    assert!(ids.contains(&ok), "相容已完成應可匯入");
    assert!(!ids.contains(&bad_cpu));
    assert!(!ids.contains(&bad_gpu));
    assert!(!ids.contains(&running));
    assert!(!ids.contains(&no_best));
    let _ = std::fs::remove_dir_all(&dir);
}

/// apply 拒絕「已完成 + bestLp 但可靠性 Unassessed（舊 session）」的 session
#[test]
fn apply_rejects_unassessed_reliability() {
    let dir = temp_dir("rel_unass");
    let storage_root = dir.join("benchmarks");
    let backend = FakeBackend::new(vec![device(GPU_A)]);
    let id = Uuid::new_v4().to_string();
    let mut detail = SessionDetail::default();
    detail.summary.id = id.clone();
    detail.summary.status = SessionStatus::Completed;
    detail.summary.gpu_instance_id = GPU_A.into();
    detail.summary.best_lp = Some(3);
    detail.summary.cpu_fingerprint = cpu_fingerprint_with(&topo(), &fixed_identity());
    // reliability 保持預設 Unassessed（模擬舊 session 缺欄位）
    storage::save_session_at(&storage_root, &detail).unwrap();

    let err = apply_best_affinity(
        &backend,
        &NoopSleeper,
        &fixed_identity(),
        &topo(),
        &storage_root,
        &dir.join("journal.json"),
        &dir.join("restore.json"),
        &id,
    )
    .unwrap_err();
    assert_eq!(err, codes::BENCHMARK_RELIABILITY_NOT_PASSED);
    assert_eq!(backend.restart_count(), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

/// check_apply：Passed → 可套用；Unassessed → 拒絕（穩定代碼）
#[test]
fn check_apply_requires_reliability_passed() {
    let dir = temp_dir("check_rel");
    let root = dir.join("benchmarks");
    let backend = FakeBackend::new(vec![device(GPU_A)]);

    // Passed → can_apply
    let ok = completed_session(&root, &topo(), GPU_A, 3);
    let st = check_apply_at(&backend, &topo(), &fixed_identity(), &root, &ok);
    assert!(st.can_apply);
    assert_eq!(st.reason, None);

    // 已完成 + bestLp + 相容，但 reliability Unassessed → 拒絕
    let id = Uuid::new_v4().to_string();
    let mut detail = SessionDetail::default();
    detail.summary.id = id.clone();
    detail.summary.status = SessionStatus::Completed;
    detail.summary.gpu_instance_id = GPU_A.into();
    detail.summary.best_lp = Some(3);
    detail.summary.cpu_fingerprint = cpu_fingerprint_with(&topo(), &fixed_identity());
    storage::save_session_at(&root, &detail).unwrap();
    let st = check_apply_at(&backend, &topo(), &fixed_identity(), &root, &id);
    assert!(!st.can_apply);
    assert_eq!(
        st.reason.as_deref(),
        Some(codes::BENCHMARK_RELIABILITY_NOT_PASSED)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// list_importable：Completed + bestLp + 相容，但可靠性 Unassessed → 排除
#[test]
fn list_importable_excludes_unassessed_reliability() {
    let dir = temp_dir("import_rel");
    let root = dir.join("benchmarks");
    let backend = FakeBackend::new(vec![device(GPU_A)]);

    let ok = completed_session(&root, &topo(), GPU_A, 3); // Passed → 納入

    let id = Uuid::new_v4().to_string();
    let mut detail = SessionDetail::default();
    detail.summary.id = id.clone();
    detail.summary.status = SessionStatus::Completed;
    detail.summary.gpu_instance_id = GPU_A.into();
    detail.summary.best_lp = Some(1);
    detail.summary.cpu_fingerprint = cpu_fingerprint_with(&topo(), &fixed_identity());
    storage::save_session_at(&root, &detail).unwrap();

    let list = list_importable(&backend, &topo(), &fixed_identity(), &root);
    let ids: Vec<String> = list.iter().map(|s| s.id.clone()).collect();
    assert!(ids.contains(&ok));
    assert!(!ids.contains(&id), "Unassessed 不該可匯入");
    let _ = std::fs::remove_dir_all(&dir);
}

// ── apply_affinity_to_gpu（共享 mutation 路徑）──

#[test]
fn shared_apply_writes_policy_and_records() {
    let dir = temp_dir("shared_ok");
    let journal = dir.join("journal.json");
    let restore = dir.join("restore.json");
    let backend = FakeBackend::new(vec![device(GPU_A)]);
    backend.set_policy(policy_on(GPU_A, 2, 0b1));

    apply_affinity_to_gpu(&backend, &NoopSleeper, GPU_A, 3, &journal, &restore).unwrap();

    let cur = backend.current_policy(GPU_A);
    assert_dword(&cur, "DevicePolicy", DEVICE_POLICY_SINGLE_PROCESSOR);
    assert_override_mask(&cur, 1u32 << 3);
    assert_eq!(backend.restart_count(), 1);
    assert!(!journal.exists(), "成功後日誌應清除");
    assert!(restore.exists(), "一層還原記錄應寫入");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn shared_apply_write_failure_restores() {
    let dir = temp_dir("shared_wfail");
    let journal = dir.join("journal.json");
    let restore = dir.join("restore.json");
    let backend = FakeBackend::new(vec![device(GPU_A)]);
    let original = policy_on(GPU_A, 2, 0b1);
    backend.set_policy(original.clone());
    backend.fail_next_write();

    let err =
        apply_affinity_to_gpu(&backend, &NoopSleeper, GPU_A, 4, &journal, &restore).unwrap_err();
    assert_eq!(err, codes::GPU_APPLY_FAILED);
    assert_eq!(backend.current_policy(GPU_A), original);
    assert!(!journal.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

// ── BenchmarkManager.apply_gpu_affinity ──

fn manager_with_gpu(gpu: &str) -> (BenchmarkManager, Arc<FakeBackend>) {
    let fake = Arc::new(FakeBackend::new(vec![device(gpu)]));
    let backend = fake.clone() as Arc<dyn GpuBackend>;
    (BenchmarkManager::new(backend), fake)
}

#[test]
fn apply_gpu_affinity_success_writes_correct_mask() {
    let dir = temp_dir("mgr_ok");
    let journal = dir.join("journal.json");
    let restore = dir.join("restore.json");
    let (m, fake) = manager_with_gpu(GPU_A);
    fake.set_policy(policy_on(GPU_A, 2, 0b1));

    m.apply_gpu_affinity_at(&topo(), GPU_A, 5, &journal, &restore)
        .unwrap();

    let cur = m.backend.read_affinity_policy(GPU_A).unwrap();
    assert_dword(&cur, "DevicePolicy", DEVICE_POLICY_SINGLE_PROCESSOR);
    assert_override_mask(&cur, 1u32 << 5);
    assert!(!journal.exists(), "成功後日誌應清除");
    assert!(restore.exists(), "一層還原記錄應寫入");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn apply_gpu_affinity_rejects_invalid_lp_outside_range() {
    let (m, _fake) = manager_with_gpu(GPU_A);
    // 8-LP 拓撲，lp=8 超出範圍
    let err = m.apply_gpu_affinity(&topo(), GPU_A, 8).unwrap_err();
    assert_eq!(err, codes::BENCHMARK_SESSION_INCOMPATIBLE);
}

#[test]
fn apply_gpu_affinity_rejects_lp_64() {
    let (m, _fake) = manager_with_gpu(GPU_A);
    let topo64 = topo_64();
    // 64-LP 拓撲，lp=64 超出 group 0 上限
    let err = m.apply_gpu_affinity(&topo64, GPU_A, 64).unwrap_err();
    assert_eq!(err, codes::BENCHMARK_SESSION_INCOMPATIBLE);
}

#[test]
fn apply_gpu_affinity_rejects_missing_gpu() {
    let (m, _fake) = manager_with_gpu(GPU_A);
    // 本機只有 GPU_A，GPU_B 不存在
    let err = m.apply_gpu_affinity(&topo(), GPU_B, 3).unwrap_err();
    assert_eq!(err, codes::GPU_NOT_FOUND);
}

#[test]
fn apply_gpu_affinity_blocks_when_benchmark_reserved() {
    let (m, _fake) = manager_with_gpu(GPU_A);
    // benchmark reservation 持有中 → manual apply 必須被拒絕（不碰 backend）
    let _guard = m.reserve(OP_BENCHMARK).unwrap();
    let err = m.apply_gpu_affinity(&topo(), GPU_A, 3).unwrap_err();
    assert_eq!(err, codes::BENCHMARK_ALREADY_RUNNING);
}

#[test]
fn apply_gpu_affinity_blocks_recovery_required() {
    let (m, _fake) = manager_with_gpu(GPU_A);
    m.recovery_required.store(true, Ordering::Relaxed);
    let err = m.apply_gpu_affinity(&topo(), GPU_A, 3).unwrap_err();
    assert_eq!(err, codes::BENCHMARK_RECOVERY_REQUIRED);
}

#[test]
fn apply_gpu_affinity_blocks_basic_display_disabled() {
    let (m, fake) = manager_with_gpu(GPU_A);
    fake.basic_display_on.store(false, Ordering::SeqCst);
    let err = m.apply_gpu_affinity(&topo(), GPU_A, 3).unwrap_err();
    assert_eq!(err, codes::GPU_BASIC_DISPLAY_DISABLED);
}

#[test]
fn apply_gpu_affinity_write_failure_restores_and_clears_journal() {
    let dir = temp_dir("mgr_writefail");
    let journal = dir.join("journal.json");
    let restore = dir.join("restore.json");
    let (m, fake) = manager_with_gpu(GPU_A);
    let original = policy_on(GPU_A, 2, 0b1);
    fake.set_policy(original.clone());
    fake.fail_next_write();

    let err = m
        .apply_gpu_affinity_at(&topo(), GPU_A, 4, &journal, &restore)
        .unwrap_err();
    assert_eq!(err, codes::GPU_APPLY_FAILED);
    // 還原成功 → 策略回原樣，日誌清除
    assert_eq!(m.backend.read_affinity_policy(GPU_A).unwrap(), original);
    assert!(!journal.exists(), "還原成功後日誌應清除");
    let _ = std::fs::remove_dir_all(&dir);
}

// ── reservation / 取消污染（concurrency）──

/// request_cancel 留下 channel=true 後，下一場 session 開始前 reset_cancel
/// 必須把 channel 實際值與 state.cancel_requested 都歸零，否則新 session 立即 Cancelled。
#[test]
fn cancel_reset_clears_channel_for_next_session() {
    let (m, _fake) = manager_with_gpu(GPU_A);
    assert!(!m.cancel_requested());
    m.request_cancel();
    assert!(m.cancel_requested());
    m.reset_cancel();
    assert!(!m.cancel_requested());
    assert!(!m.state_snapshot().cancel_requested);
    // 新 clone 給 runner 的 receiver 也必須讀到 false
    assert!(!*m.cancel_receiver().borrow());
}

/// 多執行緒同時 reserve：CAS 保證只會有一個成功（其餘回 BENCHMARK_ALREADY_RUNNING）。
#[test]
fn concurrent_reserve_only_one_wins() {
    let (m, _fake) = manager_with_gpu(GPU_A);
    let m = Arc::new(m);
    let barrier = Arc::new(std::sync::Barrier::new(8));
    let mut handles = Vec::new();
    for _ in 0..8 {
        let m = m.clone();
        let b = barrier.clone();
        handles.push(std::thread::spawn(move || {
            b.wait();
            let g = m.reserve(OP_BENCHMARK);
            // 贏家持鎖到其他執行緒也完成 attempt，避免釋放過早造成第二個成功
            std::thread::sleep(std::time::Duration::from_millis(50));
            g.is_ok()
        }));
    }
    let wins = handles
        .into_iter()
        .map(|h| h.join().unwrap())
        .filter(|ok| *ok)
        .count();
    assert_eq!(wins, 1, "並行 start 只能一個成功");
}

/// benchmark 執行期間，apply_best / manual apply / restore_previous 三種 mutation
/// 都必須被後端拒絕（不碰 backend、不讀 session）。
#[test]
fn benchmark_reservation_blocks_all_mutations() {
    let (m, fake) = manager_with_gpu(GPU_A);
    let _guard = m.reserve(OP_BENCHMARK).unwrap();
    let sid = "00000000-0000-0000-0000-000000000000";

    assert_eq!(
        m.apply_best(&topo(), sid).unwrap_err(),
        codes::BENCHMARK_ALREADY_RUNNING
    );
    assert_eq!(
        m.apply_gpu_affinity(&topo(), GPU_A, 3).unwrap_err(),
        codes::BENCHMARK_ALREADY_RUNNING
    );
    assert_eq!(
        m.restore_previous().unwrap_err(),
        codes::BENCHMARK_ALREADY_RUNNING
    );
    assert_eq!(fake.restart_count(), 0, "被拒時不得動到 GPU");
}

/// mutation 執行期間，不得開始 benchmark（start 的 reserve）也不得另一 mutation。
#[test]
fn mutation_reservation_blocks_benchmark_and_other_mutation() {
    let (m, _fake) = manager_with_gpu(GPU_A);
    let _guard = m.reserve(OP_MUTATION).unwrap();
    assert!(m.reserve(OP_BENCHMARK).is_err(), "start 不得搶 mutation");
    assert!(
        m.reserve(OP_MUTATION).is_err(),
        "mutation 不得並行另一 mutation"
    );
    assert_eq!(
        m.apply_gpu_affinity(&topo(), GPU_A, 3).unwrap_err(),
        codes::BENCHMARK_ALREADY_RUNNING
    );
}

/// 前置驗證失敗（LP 越界 / GPU 不存在 / BasicDisplay 停用）不得卡住 reservation。
#[test]
fn preflight_failure_releases_reservation() {
    let (m, fake) = manager_with_gpu(GPU_A);

    // 1) LP 越界
    assert_eq!(
        m.apply_gpu_affinity(&topo(), GPU_A, 8).unwrap_err(),
        codes::BENCHMARK_SESSION_INCOMPATIBLE
    );
    drop(m.reserve(OP_BENCHMARK).unwrap());

    // 2) GPU 不存在
    assert_eq!(
        m.apply_gpu_affinity(&topo(), GPU_B, 3).unwrap_err(),
        codes::GPU_NOT_FOUND
    );
    drop(m.reserve(OP_MUTATION).unwrap());

    // 3) BasicDisplay 停用
    fake.basic_display_on.store(false, Ordering::SeqCst);
    assert_eq!(
        m.apply_gpu_affinity(&topo(), GPU_A, 3).unwrap_err(),
        codes::GPU_BASIC_DISPLAY_DISABLED
    );
    drop(m.reserve(OP_BENCHMARK).unwrap());
}

// ── 錯誤安全 transaction（rollback / fault injection）──

/// journal stage advance（PolicyApplied）失敗 → 立即 rollback，策略還原、日誌清除。
#[test]
fn apply_journal_advance_failure_rolls_back() {
    let dir = temp_dir("advfail");
    let journal = dir.join("journal.json");
    let restore = dir.join("restore.json");
    let backend = FakeBackend::new(vec![device(GPU_A)]);
    let original = policy_on(GPU_A, 2, 0b1);
    backend.set_policy(original.clone());
    recovery::inject::fail_next_advance();

    let err =
        apply_affinity_to_gpu(&backend, &NoopSleeper, GPU_A, 4, &journal, &restore).unwrap_err();
    assert_eq!(err, codes::GPU_APPLY_FAILED);
    assert_eq!(backend.current_policy(GPU_A), original);
    assert!(!journal.exists(), "advance 失敗 rollback 後日誌應清除");
    let _ = std::fs::remove_dir_all(&dir);
}

/// read-back 讀取失敗（第 2 次 read）→ 立即 rollback，回穩定代碼。
#[test]
fn apply_read_back_failure_rolls_back() {
    let dir = temp_dir("readbackfail");
    let journal = dir.join("journal.json");
    let restore = dir.join("restore.json");
    let backend = FakeBackend::new(vec![device(GPU_A)]);
    let original = policy_on(GPU_A, 2, 0b1);
    backend.set_policy(original.clone());
    backend.fail_nth_read(2); // snapshot read=1 成功；read-back read=2 失敗

    let err =
        apply_affinity_to_gpu(&backend, &NoopSleeper, GPU_A, 4, &journal, &restore).unwrap_err();
    assert_eq!(err, codes::GPU_REGISTRY_FAILED);
    assert_eq!(backend.current_policy(GPU_A), original);
    assert!(!journal.exists(), "read-back 失敗 rollback 後日誌應清除");
    let _ = std::fs::remove_dir_all(&dir);
}

/// read-back 驗證不符（第 2 次 read 回錯 mask）→ 立即 rollback。
#[test]
fn apply_read_back_mismatch_rolls_back() {
    let dir = temp_dir("readbackmismatch");
    let journal = dir.join("journal.json");
    let restore = dir.join("restore.json");
    let backend = FakeBackend::new(vec![device(GPU_A)]);
    let original = policy_on(GPU_A, 2, 0b1);
    backend.set_policy(original.clone());
    backend.fail_nth_read_mismatch(2);

    let err =
        apply_affinity_to_gpu(&backend, &NoopSleeper, GPU_A, 4, &journal, &restore).unwrap_err();
    assert_eq!(err, codes::GPU_APPLY_FAILED);
    assert_eq!(backend.current_policy(GPU_A), original);
    assert!(!journal.exists(), "read-back 不符 rollback 後日誌應清除");
    let _ = std::fs::remove_dir_all(&dir);
}

/// 還原記錄 atomic write 失敗 → rollback，不留下已套用策略。
/// 以 restore 路徑的 parent 做成檔案模擬 write 失敗（create_dir_all 失敗），
/// 只失敗寫入、不影響 clear_restore_record 的 NotFound→Ok，還原乾淨。
#[test]
fn apply_restore_record_write_failure_rolls_back() {
    let dir = temp_dir("recfail");
    let journal = dir.join("journal.json");
    let blocker = dir.join("blocker");
    std::fs::write(&blocker, b"x").unwrap(); // parent 為檔案 → atomic_write 的 create_dir_all 失敗
    let restore = blocker.join("restore.json");
    let backend = FakeBackend::new(vec![device(GPU_A)]);
    let original = policy_on(GPU_A, 2, 0b1);
    backend.set_policy(original.clone());

    let err =
        apply_affinity_to_gpu(&backend, &NoopSleeper, GPU_A, 4, &journal, &restore).unwrap_err();
    assert_eq!(err, codes::GPU_APPLY_FAILED);
    assert_eq!(backend.current_policy(GPU_A), original);
    assert!(!journal.exists(), "還原記錄寫入失敗 rollback 後日誌應清除");
    assert!(!restore.exists(), "還原記錄不該被寫入");
    let _ = std::fs::remove_dir_all(&dir);
}

/// journal clear 失敗（成功路徑）→ rollback，避免 stale 日誌讓下次啟動誤還原。
#[test]
fn apply_journal_clear_failure_rolls_back() {
    let dir = temp_dir("clearfail");
    let journal = dir.join("journal.json");
    let restore = dir.join("restore.json");
    let backend = FakeBackend::new(vec![device(GPU_A)]);
    let original = policy_on(GPU_A, 2, 0b1);
    backend.set_policy(original.clone());
    recovery::inject::fail_next_clear();

    let err =
        apply_affinity_to_gpu(&backend, &NoopSleeper, GPU_A, 4, &journal, &restore).unwrap_err();
    assert_eq!(err, codes::GPU_APPLY_FAILED);
    assert_eq!(backend.current_policy(GPU_A), original);
    assert!(!journal.exists(), "clear 失敗 rollback 後日誌應清除");
    assert!(!restore.exists(), "rollback 後未完成還原記錄應清除");
    let _ = std::fs::remove_dir_all(&dir);
}

/// rollback 失敗（restart 持續失敗）→ 保留 stage=PolicyApplied 日誌、
/// manager 設 recoveryRequired、封鎖後續 mutation。
#[test]
fn apply_rollback_failure_sets_recovery_required() {
    let dir = temp_dir("rollbackfail");
    let journal = dir.join("journal.json");
    let restore = dir.join("restore.json");
    let (m, fake) = manager_with_gpu(GPU_A);
    let original = policy_on(GPU_A, 2, 0b1);
    fake.set_policy(original.clone());
    fake.disable_fails.store(true, Ordering::SeqCst); // restart 持續失敗 → rollback 也失敗

    let err = m
        .apply_gpu_affinity_at(&topo(), GPU_A, 4, &journal, &restore)
        .unwrap_err();
    assert_eq!(err, codes::GPU_RESTART_FAILED);
    assert!(m.recovery_required(), "rollback 失敗應設 recoveryRequired");
    assert!(m.state_snapshot().recovery_required);
    assert!(journal.exists(), "rollback 失敗應保留復原日誌");
    let j = recovery::load_from(&journal).unwrap().unwrap();
    assert_eq!(j.stage, RecoveryStage::PolicyApplied, "應強制完整 restore");
    assert_eq!(m.backend.read_affinity_policy(GPU_A).unwrap(), original);
    // 封鎖後續 mutation
    assert_eq!(
        m.apply_gpu_affinity(&topo(), GPU_A, 3).unwrap_err(),
        codes::BENCHMARK_RECOVERY_REQUIRED
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// rollback 還原成功但 restore record 清理失敗 → Err 且 manager recoveryRequired，
/// 不得留下 stale gpu-restore.json 卻 recoveryRequired=false。
#[test]
fn rollback_restore_record_cleanup_failure_sets_recovery_required() {
    let dir = temp_dir("rollback_crec");
    let journal = dir.join("journal.json");
    let restore = dir.join("restore.json");
    let (m, fake) = manager_with_gpu(GPU_A);
    let original = policy_on(GPU_A, 2, 0b1);
    fake.set_policy(original.clone());
    fake.fail_next_write(); // apply 的 write 失敗 → rollback
    inject::fail_next_clear_restore_record(); // rollback 的 restore record 清理失敗

    let err = m
        .apply_gpu_affinity_at(&topo(), GPU_A, 4, &journal, &restore)
        .unwrap_err();
    assert_eq!(err, codes::GPU_APPLY_FAILED);
    assert!(
        m.recovery_required(),
        "restore record 清理失敗應設 recoveryRequired"
    );
    assert_eq!(m.backend.read_affinity_policy(GPU_A).unwrap(), original);
    assert!(journal.exists(), "清理失敗應保留 dirty journal");
    let j = recovery::load_from(&journal).unwrap().unwrap();
    assert_eq!(j.stage, RecoveryStage::PolicyApplied);
    let _ = std::fs::remove_dir_all(&dir);
}

/// rollback 還原成功但 journal cleanup 失敗 → 不得視為乾淨 rollback。
#[test]
fn rollback_journal_cleanup_failure_sets_recovery_required() {
    let dir = temp_dir("rollback_cjournal");
    let journal = dir.join("journal.json");
    let restore = dir.join("restore.json");
    let (m, fake) = manager_with_gpu(GPU_A);
    let original = policy_on(GPU_A, 2, 0b1);
    fake.set_policy(original.clone());
    fake.fail_next_write(); // apply 的 write 失敗 → rollback
    recovery::inject::fail_next_clear(); // rollback 的 journal clear 失敗

    let err = m
        .apply_gpu_affinity_at(&topo(), GPU_A, 4, &journal, &restore)
        .unwrap_err();
    assert_eq!(err, codes::GPU_APPLY_FAILED);
    assert!(
        m.recovery_required(),
        "journal cleanup 失敗不得視為乾淨 rollback"
    );
    assert!(journal.exists(), "清理失敗應保留 dirty journal");
    let _ = std::fs::remove_dir_all(&dir);
}

/// journal stage advance（PolicyApplied）失敗後，rollback 的完整復原也失敗 →
/// 必須 fail-closed：journal 升級為 PolicyApplied（非只驗證的 SnapshotTaken），
/// manager 設 recoveryRequired 封鎖後續 mutation。
#[test]
fn apply_journal_advance_and_restore_failure_fail_closed() {
    let dir = temp_dir("advance_restore_fail");
    let journal = dir.join("journal.json");
    let restore = dir.join("restore.json");
    let (m, fake) = manager_with_gpu(GPU_A);
    let original = policy_on(GPU_A, 2, 0b1);
    fake.set_policy(original.clone());
    fake.disable_fails.store(true, Ordering::SeqCst); // rollback 的 restore restart 持續失敗
    recovery::inject::fail_next_advance(); // PolicyApplied advance 失敗

    let err = m
        .apply_gpu_affinity_at(&topo(), GPU_A, 4, &journal, &restore)
        .unwrap_err();
    assert_eq!(err, codes::GPU_APPLY_FAILED);
    assert!(
        m.recovery_required(),
        "advance + restore 失敗必須 fail-closed"
    );
    assert!(journal.exists());
    let j = recovery::load_from(&journal).unwrap().unwrap();
    assert_eq!(
        j.stage,
        RecoveryStage::PolicyApplied,
        "journal 不得停留在 SnapshotTaken"
    );
    assert_eq!(
        m.apply_gpu_affinity(&topo(), GPU_A, 3).unwrap_err(),
        codes::BENCHMARK_RECOVERY_REQUIRED
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// load_restore_record：NotFound → None；有效 → Some；壞 JSON → Err；其他 I/O error → Err。
#[test]
fn load_restore_record_distinguishes_notfound_and_errors() {
    let dir = temp_dir("loadrec");
    // NotFound → None
    assert_eq!(
        load_restore_record(&dir.join("missing.json")).unwrap(),
        None
    );
    // 有效（已認證）JSON → Some
    let good = dir.join("good.json");
    let snap = policy_on(GPU_A, 2, 0b1);
    crate::state_auth::auth_write(&good, &serde_json::to_string(&snap).unwrap()).unwrap();
    assert_eq!(load_restore_record(&good).unwrap(), Some(snap.clone()));
    // 未認證的直接寫入（無 MAC 旁檔）→ Err
    let unsigned = dir.join("unsigned.json");
    std::fs::write(&unsigned, serde_json::to_string(&snap).unwrap()).unwrap();
    assert!(load_restore_record(&unsigned).is_err());
    // 壞 JSON → Err
    let bad = dir.join("bad.json");
    std::fs::write(&bad, "{ not json").unwrap();
    assert!(load_restore_record(&bad).is_err());
    // 其他 I/O error（目錄）→ Err，不是 None
    let as_dir = dir.join("asdir.json");
    std::fs::create_dir_all(&as_dir).unwrap();
    assert!(load_restore_record(&as_dir).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

// ── 等效安全驗證 / 等效套用（Task 3）──

fn equivalent_reliability() -> ReliabilitySummary {
    ReliabilitySummary {
        status: ReliabilityStatus::Equivalent,
        algorithm_version: 2,
        ..Default::default()
    }
}

/// 寫入一個 equivalent-mode Completed session（equivalent_finalist_lps = finalists）。
fn equivalent_session(
    storage_root: &Path,
    topo: &Topology,
    gpu: &str,
    finalists: &[u32],
) -> String {
    let id = Uuid::new_v4().to_string();
    let detail = SessionDetail {
        summary: crate::benchmark::SessionSummary {
            id: id.clone(),
            status: SessionStatus::Completed,
            started_at: "2026-08-11T00:00:00Z".into(),
            finished_at: Some("2026-08-11T00:01:00Z".into()),
            gpu_name: "GPU".into(),
            gpu_instance_id: gpu.to_string(),
            cpu_fingerprint: cpu_fingerprint_with(topo, &fixed_identity()),
            best_lp: None,
            reliability: equivalent_reliability(),
            equivalent_finalist_lps: finalists.to_vec(),
            severe_lps: vec![],
            sample_count: 5,
            total_bytes: 0,
            config: BenchmarkConfig::default(),
            error: None,
            ..Default::default()
        },
        results: vec![],
        samples: vec![],
        ..Default::default()
    };
    storage::save_session_at(storage_root, &detail).unwrap();
    id
}

#[test]
fn mask_bytes_to_lp_roundtrips_and_rejects_non_single_bit() {
    assert_eq!(mask_bytes_to_lp(Some(&single_lp_mask_bytes(0))), Some(0));
    assert_eq!(mask_bytes_to_lp(Some(&single_lp_mask_bytes(3))), Some(3));
    assert_eq!(mask_bytes_to_lp(Some(&single_lp_mask_bytes(63))), Some(63));
    // 非單一位元 / 空 → None
    assert_eq!(mask_bytes_to_lp(None), None);
    assert_eq!(mask_bytes_to_lp(Some(&[])), None);
    assert_eq!(mask_bytes_to_lp(Some(&[0b11])), None);
}

#[test]
fn equivalent_validation_plan_immediate_pass_when_current_in_pair() {
    let dir = temp_dir("plan_imm");
    let storage_root = dir.join("benchmarks");
    std::fs::create_dir_all(&storage_root).unwrap();
    let backend = FakeBackend::new(vec![device(GPU_A)]);
    let sid = equivalent_session(&storage_root, &topo(), GPU_A, &[3, 5]);
    let detail = storage::get_at(&storage_root, &sid).unwrap();
    // 目前鎖定 LP 5（在 pair 內）
    let policy = policy_on(GPU_A, 4, 1 << 5);
    match equivalent_validation_plan(&backend, &topo(), &fixed_identity(), &detail, &policy, 3) {
        Ok(EquivalentValidationPlan::ImmediatePass { reference_lp }) => {
            assert_eq!(reference_lp, 5);
        }
        other => panic!("應 ImmediatePass: {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn equivalent_validation_plan_run_captures_when_current_outside_pair() {
    let dir = temp_dir("plan_run");
    let storage_root = dir.join("benchmarks");
    std::fs::create_dir_all(&storage_root).unwrap();
    let backend = FakeBackend::new(vec![device(GPU_A)]);
    let sid = equivalent_session(&storage_root, &topo(), GPU_A, &[3, 5]);
    let detail = storage::get_at(&storage_root, &sid).unwrap();
    // 目前鎖定 LP 2（不在 pair 內）
    let policy = policy_on(GPU_A, 4, 1 << 2);
    match equivalent_validation_plan(&backend, &topo(), &fixed_identity(), &detail, &policy, 3) {
        Ok(EquivalentValidationPlan::RunCaptures { reference_lp }) => {
            assert_eq!(reference_lp, 2);
        }
        other => panic!("應 RunCaptures: {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn equivalent_validation_plan_rejects_non_pair_selected_and_no_reference() {
    let dir = temp_dir("plan_reject");
    let storage_root = dir.join("benchmarks");
    std::fs::create_dir_all(&storage_root).unwrap();
    let backend = FakeBackend::new(vec![device(GPU_A)]);
    let sid = equivalent_session(&storage_root, &topo(), GPU_A, &[3, 5]);
    let detail = storage::get_at(&storage_root, &sid).unwrap();
    let policy = policy_on(GPU_A, 4, 1 << 5);
    // selected 不在 pair → LP_INVALID
    assert_eq!(
        equivalent_validation_plan(&backend, &topo(), &fixed_identity(), &detail, &policy, 9)
            .unwrap_err(),
        codes::BENCHMARK_EQUIVALENT_LP_INVALID
    );
    // 無單一鎖定核心（policy 空）→ NO_REFERENCE
    let empty = AffinityPolicy {
        instance_id: GPU_A.to_string(),
        ..Default::default()
    };
    assert_eq!(
        equivalent_validation_plan(&backend, &topo(), &fixed_identity(), &detail, &empty, 3)
            .unwrap_err(),
        codes::BENCHMARK_EQUIVALENT_NO_REFERENCE
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn apply_equivalent_decision_requires_passed_matching_validation() {
    let dir = temp_dir("apply_decision");
    let storage_root = dir.join("benchmarks");
    std::fs::create_dir_all(&storage_root).unwrap();
    let backend = FakeBackend::new(vec![device(GPU_A)]);
    let sid = equivalent_session(&storage_root, &topo(), GPU_A, &[3, 5]);
    // 未驗證 → VALIDATION_REQUIRED
    {
        let detail = storage::get_at(&storage_root, &sid).unwrap();
        let policy = policy_on(GPU_A, 4, 1 << 2);
        assert_eq!(
            apply_equivalent_decision(&backend, &topo(), &fixed_identity(), &detail, &policy, 3)
                .unwrap_err(),
            codes::BENCHMARK_EQUIVALENT_VALIDATION_REQUIRED
        );
    }
    // 寫入 Passed validation（selected=3、ref mask=LP2）→ 通過
    write_equivalent_validation(
        &storage_root,
        &sid,
        EquivalentSafetyValidation {
            status: EquivalentSafetyStatus::Passed,
            selected_lp: Some(3),
            reference_lp: Some(2),
            rounds: 3,
            reference_policy_mask: Some(single_lp_mask_bytes(2)),
            reason: Some("passed".to_string()),
            ..Default::default()
        },
    )
    .unwrap();
    let detail = storage::get_at(&storage_root, &sid).unwrap();
    // live policy 仍為 LP2（未變）→ 通過
    let policy = policy_on(GPU_A, 4, 1 << 2);
    assert_eq!(
        apply_equivalent_decision(&backend, &topo(), &fixed_identity(), &detail, &policy, 3)
            .unwrap(),
        3
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn apply_equivalent_decision_rejects_reference_changed() {
    let dir = temp_dir("apply_changed");
    let storage_root = dir.join("benchmarks");
    std::fs::create_dir_all(&storage_root).unwrap();
    let backend = FakeBackend::new(vec![device(GPU_A)]);
    let sid = equivalent_session(&storage_root, &topo(), GPU_A, &[3, 5]);
    write_equivalent_validation(
        &storage_root,
        &sid,
        EquivalentSafetyValidation {
            status: EquivalentSafetyStatus::Passed,
            selected_lp: Some(3),
            reference_lp: Some(2),
            rounds: 3,
            reference_policy_mask: Some(single_lp_mask_bytes(2)),
            reason: Some("passed".to_string()),
            ..Default::default()
        },
    )
    .unwrap();
    let detail = storage::get_at(&storage_root, &sid).unwrap();
    // live policy 變更為 LP4（≠ snapshot LP2）→ REFERENCE_CHANGED
    let policy = policy_on(GPU_A, 4, 1 << 4);
    assert_eq!(
        apply_equivalent_decision(&backend, &topo(), &fixed_identity(), &detail, &policy, 3)
            .unwrap_err(),
        codes::BENCHMARK_EQUIVALENT_REFERENCE_CHANGED
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn check_apply_equivalent_requires_validation_then_can_apply() {
    let dir = temp_dir("check_equiv");
    let storage_root = dir.join("benchmarks");
    std::fs::create_dir_all(&storage_root).unwrap();
    let backend = FakeBackend::new(vec![device(GPU_A)]);
    let sid = equivalent_session(&storage_root, &topo(), GPU_A, &[3, 5]);
    // 尚未驗證 → equivalent_mode=true、requires_safety_validation=true、can_apply=false
    let st = check_apply_at(&backend, &topo(), &fixed_identity(), &storage_root, &sid);
    assert!(st.equivalent_mode);
    assert_eq!(st.allowed_lps, vec![3, 5]);
    assert!(st.requires_safety_validation);
    assert!(!st.can_apply);
    assert_eq!(
        st.reason.as_deref(),
        Some(codes::BENCHMARK_EQUIVALENT_VALIDATION_REQUIRED)
    );
    // 寫入 Passed validation（ref snapshot=LP2），live policy 仍為 LP2 → can_apply=true、requires=false
    write_equivalent_validation(
        &storage_root,
        &sid,
        EquivalentSafetyValidation {
            status: EquivalentSafetyStatus::Passed,
            selected_lp: Some(3),
            reference_lp: Some(2),
            rounds: 3,
            reference_policy_mask: Some(single_lp_mask_bytes(2)),
            reason: Some("passed".to_string()),
            ..Default::default()
        },
    )
    .unwrap();
    backend.set_policy(policy_on(GPU_A, 4, 1 << 2));
    let st = check_apply_at(&backend, &topo(), &fixed_identity(), &storage_root, &sid);
    assert!(st.equivalent_mode);
    assert!(st.can_apply);
    assert!(!st.requires_safety_validation);
    assert_eq!(st.reason, None);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn check_apply_legacy_path_unchanged_for_passed_session() {
    let dir = temp_dir("check_legacy");
    let storage_root = dir.join("benchmarks");
    std::fs::create_dir_all(&storage_root).unwrap();
    let backend = FakeBackend::new(vec![device(GPU_A)]);
    let sid = completed_session(&storage_root, &topo(), GPU_A, 3);
    let st = check_apply_at(&backend, &topo(), &fixed_identity(), &storage_root, &sid);
    assert!(st.can_apply);
    assert!(!st.equivalent_mode);
    assert!(st.allowed_lps.is_empty());
    assert!(!st.requires_safety_validation);
    let _ = std::fs::remove_dir_all(&dir);
}

/// asset 解析/驗證失敗不得留下永久 Pending；assets 就緒後可重試成功。
#[test]
fn begin_equivalent_validation_asset_failure_leaves_no_pending_and_is_retryable() {
    let dir = temp_dir("begin_assets");
    let storage_root = dir.join("benchmarks");
    std::fs::create_dir_all(&storage_root).unwrap();
    let sid = equivalent_session(&storage_root, &topo(), GPU_A, &[3, 5]);

    // asset 驗證失敗 → 不寫 Pending，session validation 保持 None（可重試）
    let err = begin_equivalent_validation(
        &storage_root,
        &sid,
        3,
        2,
        Some(single_lp_mask_bytes(2)),
        Err(codes::BENCHMARK_ASSETS_MISSING.to_string()),
    )
    .unwrap_err();
    assert_eq!(err, codes::BENCHMARK_ASSETS_MISSING);
    assert!(
        storage::get_at(&storage_root, &sid)
            .unwrap()
            .equivalent_safety_validation
            .is_none(),
        "asset 失敗不得留下 Pending"
    );

    // assets 就緒 → 重試成功寫入 Pending
    let assets = BenchmarkAssets {
        presentmon: PathBuf::from("pm"),
        vulkan_workload: PathBuf::from("vk"),
        d3d9_workload: PathBuf::from("d3d9"),
    };
    let out = begin_equivalent_validation(
        &storage_root,
        &sid,
        3,
        2,
        Some(single_lp_mask_bytes(2)),
        Ok(assets),
    )
    .unwrap();
    assert_eq!(out.presentmon, PathBuf::from("pm"));
    let v = storage::get_at(&storage_root, &sid)
        .unwrap()
        .equivalent_safety_validation
        .unwrap();
    assert_eq!(v.status, EquivalentSafetyStatus::Pending);
    let _ = std::fs::remove_dir_all(&dir);
}

/// validation_running 以 OP_VALIDATION reservation 辨識（不讀 session status）。
#[test]
fn validation_running_reflects_reservation() {
    let (m, _fake) = manager_with_gpu(GPU_A);
    assert!(!m.validation_running());
    let guard = m.reserve(OP_VALIDATION).unwrap();
    assert!(m.validation_running());
    drop(guard);
    assert!(!m.validation_running());
}

/// check_apply：live reference policy 變更 → can_apply=false、reason=REFERENCE_CHANGED。
#[test]
fn check_apply_equivalent_rejects_reference_changed() {
    let dir = temp_dir("check_ref_changed");
    let storage_root = dir.join("benchmarks");
    std::fs::create_dir_all(&storage_root).unwrap();
    let backend = FakeBackend::new(vec![device(GPU_A)]);
    let sid = equivalent_session(&storage_root, &topo(), GPU_A, &[3, 5]);
    write_equivalent_validation(
        &storage_root,
        &sid,
        EquivalentSafetyValidation {
            status: EquivalentSafetyStatus::Passed,
            selected_lp: Some(3),
            reference_lp: Some(2),
            rounds: 3,
            reference_policy_mask: Some(single_lp_mask_bytes(2)),
            reason: Some("passed".to_string()),
            ..Default::default()
        },
    )
    .unwrap();
    // live policy 仍是 LP2 → can_apply
    backend.set_policy(policy_on(GPU_A, 4, 1 << 2));
    let st = check_apply_at(&backend, &topo(), &fixed_identity(), &storage_root, &sid);
    assert!(st.can_apply);

    // live policy 變更為 LP4 → 拒絕
    backend.set_policy(policy_on(GPU_A, 4, 1 << 4));
    let st = check_apply_at(&backend, &topo(), &fixed_identity(), &storage_root, &sid);
    assert!(!st.can_apply);
    assert_eq!(
        st.reason.as_deref(),
        Some(codes::BENCHMARK_EQUIVALENT_REFERENCE_CHANGED)
    );
    assert!(st.requires_safety_validation);
    let _ = std::fs::remove_dir_all(&dir);
}
#[test]
fn physical_core_apply_rolls_back_faults_and_preserves_pending_recovery() {
    for mode in [
        "write", "restart", "read", "mismatch", "clear", "restore", "missing",
    ] {
        let root = temp_dir(&format!("physical_apply_{mode}"));
        let backend = Arc::new(FakeBackend::new(if mode == "missing" {
            vec![]
        } else {
            vec![device(GPU_A)]
        }));
        let baseline = policy_on(GPU_A, 4, 4); // E-core policy is a valid restore target.
        backend.set_policy(baseline.clone());
        let mut manager = BenchmarkManager::new(backend.clone());
        manager.sleeper = Arc::new(NoopSleeper);
        let topo = build_topology(vec![(vec![0, 1], 1, true), (vec![2], 0, false)]);
        if mode == "write" {
            backend.fail_next_write();
        }
        if mode == "restart" {
            backend.fail_next_restart.store(true, Ordering::SeqCst);
        }
        if mode == "read" {
            backend.fail_nth_read(2);
        }
        if mode == "mismatch" {
            backend.fail_nth_read_mismatch(2);
        }
        if mode == "clear" {
            recovery::inject::fail_next_clear();
        }
        if mode == "restore" {
            backend.disable_fails.store(true, Ordering::SeqCst);
        }
        let journal = root.join("journal.json");
        let restore = root.join("restore.json");
        assert!(
            manager
                .apply_core_at(
                    &topo,
                    &fixed_identity(),
                    GPU_A,
                    0,
                    None,
                    &root,
                    &journal,
                    &restore
                )
                .is_err(),
            "{mode}"
        );
        assert_eq!(backend.current_policy(GPU_A), baseline, "{mode}");
        if mode == "restore" {
            assert!(journal.exists());
            assert!(manager.recovery_required());
        } else {
            assert!(!journal.exists());
            assert!(!manager.recovery_required());
        }
    }
}

#[test]
fn legacy_gpu_restore_remains_valid_and_is_journaled_on_failure() {
    let root = temp_dir("physical_legacy_restore");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    let original = policy_on(GPU_A, 4, 0x84); // arbitrary multi-LP policy, not a candidate
    let mut manager = BenchmarkManager::new(backend.clone());
    manager.sleeper = Arc::new(NoopSleeper);
    let journal = root.join("journal.json");
    let restore = root.join("restore.json");
    write_restore_record(&restore, &original).unwrap();
    backend.fail_next_restart.store(true, Ordering::SeqCst);
    assert!(manager.restore_previous_at(&journal, &restore).is_err());
    assert!(journal.exists());
    assert!(restore.exists());
    assert!(manager.recovery_required());
    manager.restore_previous_at(&journal, &restore).unwrap();
    assert_eq!(backend.current_policy(GPU_A), original);
    assert!(!journal.exists());
    assert!(!manager.recovery_required());
}

#[test]
fn all_gpu_operations_block_close_and_updates_exclude_mutations() {
    let m = BenchmarkManager::new(Arc::new(FakeBackend::new(vec![])));
    for kind in [OP_BENCHMARK, OP_MUTATION] {
        let guard = m.reserve(kind).unwrap();
        assert!(m.refuse_exit_if_running().is_err());
        assert!(!m.can_exit());
        assert!(m.begin_update().is_err());
        assert!(m.state_snapshot().gpu_busy);
        drop(guard);
    }
    m.begin_update().unwrap();
    assert!(m.reserve(OP_BENCHMARK).is_err());
    assert!(m.reserve(OP_MUTATION).is_err());
    assert!(m.can_exit()); // installer may restart after acquiring exclusion
    m.end_update();
    assert!(!m.state_snapshot().gpu_busy);
    assert!(m.can_exit());
}

// ── MSI 模式 ────────────────────────────────────────────────────────

#[test]
fn msi_monitor_record_preserves_device_identity_and_restore_compatibility() {
    let dir = temp_dir("msi_monitor_record");
    let path = dir.join("msi.json");
    let snapshot = RegistryValueSnapshot::dword(0);
    write_msi_monitor_record(&path, &snapshot, GPU_A).unwrap();
    let value: serde_json::Value =
        serde_json::from_str(&crate::state_auth::auth_read(&path).unwrap()).unwrap();
    assert_eq!(value["instanceId"].as_str(), Some(GPU_A));
    assert_eq!(load_msi_record(&path).unwrap().unwrap(), snapshot);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn drift_polling_keeps_cached_result_without_postponing_next_check() {
    let (manager, _) = manager_with_gpu(GPU_A);
    let checked_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        - 1_000;
    manager
        .drift_checked_at
        .store(checked_at, Ordering::Relaxed);
    manager.state.write().unwrap().policy_drift = Some(DriftStatus::Drifted);
    for _ in 0..3 {
        assert_eq!(
            manager.state_snapshot().policy_drift,
            Some(DriftStatus::Drifted)
        );
        assert_eq!(manager.drift_checked_at.load(Ordering::Relaxed), checked_at);
    }
}

#[test]
fn msi_reapply_upgrades_legacy_monitor_record_without_restarting() {
    let dir = temp_dir("msi_monitor_upgrade");
    let path = dir.join("msi.json");
    let original = RegistryValueSnapshot::dword(0);
    crate::state_auth::auth_write(&path, &serde_json::to_string(&original).unwrap()).unwrap();
    let backend = FakeBackend::new(vec![device(GPU_A)]);
    backend.set_msi(GPU_A, Some(1));
    apply_msi_to_gpu(&backend, &NoopSleeper, GPU_A, &path).unwrap();
    assert_eq!(backend.restart_count(), 0);
    assert_eq!(load_msi_record(&path).unwrap().unwrap(), original);
    let value: serde_json::Value =
        serde_json::from_str(&crate::state_auth::auth_read(&path).unwrap()).unwrap();
    assert_eq!(value["instanceId"].as_str(), Some(GPU_A));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn msi_apply_enables_restarts_and_records_snapshot() {
    let dir = temp_dir("msi_on");
    let record = dir.join("msi.json");
    let backend = FakeBackend::new(vec![device(GPU_A)]);
    backend.set_msi(GPU_A, Some(0)); // 行線中斷

    apply_msi_to_gpu(&backend, &NoopSleeper, GPU_A, &record).unwrap();

    assert_eq!(backend.msi_value(GPU_A), Some(1));
    assert_eq!(backend.restart_count(), 1);
    assert!(record.exists(), "還原記錄應寫入");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn msi_apply_absent_value_also_records_absent_snapshot() {
    let dir = temp_dir("msi_absent");
    let record = dir.join("msi.json");
    let backend = FakeBackend::new(vec![device(GPU_A)]);
    // 不 set_msi → 值不存在

    apply_msi_to_gpu(&backend, &NoopSleeper, GPU_A, &record).unwrap();
    assert_eq!(backend.msi_value(GPU_A), Some(1));

    // 還原 → 回到「不存在」
    restore_msi_to_gpu(&backend, &NoopSleeper, GPU_A, &record).unwrap();
    assert_eq!(backend.msi_value(GPU_A), None);
    assert!(!record.exists(), "還原後記錄應清除");
    assert_eq!(backend.restart_count(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn msi_apply_is_noop_when_already_enabled() {
    let dir = temp_dir("msi_noop");
    let record = dir.join("msi.json");
    let backend = FakeBackend::new(vec![device(GPU_A)]);
    backend.set_msi(GPU_A, Some(1));

    apply_msi_to_gpu(&backend, &NoopSleeper, GPU_A, &record).unwrap();

    assert_eq!(backend.restart_count(), 0, "已啟用不得重啟裝置");
    assert!(!record.exists(), "no-op 不得寫還原記錄");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn msi_apply_rolls_back_when_write_fails() {
    let dir = temp_dir("msi_rollback");
    let record = dir.join("msi.json");
    let backend = FakeBackend::new(vec![device(GPU_A)]);
    backend.set_msi(GPU_A, Some(0));
    // fake 的 MSI write 無故障注入；以 restart 失敗驗證 rollback 路徑
    //（寫入失敗與重啟失敗走同一個 rollback_msi）。
    backend.fail_next_restart.store(true, Ordering::SeqCst);
    let err = apply_msi_to_gpu(&backend, &NoopSleeper, GPU_A, &record).unwrap_err();
    assert_eq!(err.code, codes::GPU_RESTART_FAILED);
    assert_eq!(backend.msi_value(GPU_A), Some(0), "rollback 應寫回原值");
    assert!(!record.exists(), "rollback 應清記錄");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn msi_restore_without_record_fails() {
    let dir = temp_dir("msi_restore_norecord");
    let record = dir.join("msi.json");
    let backend = FakeBackend::new(vec![device(GPU_A)]);
    assert!(restore_msi_to_gpu(&backend, &NoopSleeper, GPU_A, &record).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

// ── 政策漂移偵測 ────────────────────────────────────────────────────

#[test]
fn policy_drift_reports_match_drifted_and_none() {
    let dir = temp_dir("drift");
    let record = dir.join("applied.json");
    let backend = FakeBackend::new(vec![device(GPU_A)]);
    // 無記錄 → None
    assert_eq!(check_policy_drift_at(&backend, &record), DriftStatus::None);
    // 套用 LP 5（經 apply_mask_to_gpu 正式路徑）→ Match
    apply_mask_to_gpu(
        &backend,
        &NoopSleeper,
        GPU_A,
        single_lp_mask_bytes(5),
        &dir.join("journal.json"),
        &dir.join("restore.json"),
    )
    .unwrap();
    write_applied_record(
        &record,
        &AppliedPolicyRecord {
            instance_id: GPU_A.to_string(),
            core_id: 2,
            lp_indices: vec![5],
            override_bytes: single_lp_mask_bytes(5),
            applied_at: chrono::Local::now().to_rfc3339(),
        },
    )
    .unwrap();
    assert_eq!(check_policy_drift_at(&backend, &record), DriftStatus::Match);
    // 外部改寫（mask 變為 LP 9）→ Drifted
    backend.set_policy(policy_on(GPU_A, 4, 1 << 9));
    assert_eq!(
        check_policy_drift_at(&backend, &record),
        DriftStatus::Drifted
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn policy_drift_tampered_record_reports_none() {
    let dir = temp_dir("drift_tamper");
    let record = dir.join("applied.json");
    let backend = FakeBackend::new(vec![device(GPU_A)]);
    // 未經 HMAC 寫入的偽造記錄 → fail-closed 視為無記錄
    std::fs::write(&record, r#"{"instanceId":"x","coreId":1}"#).unwrap();
    assert_eq!(check_policy_drift_at(&backend, &record), DriftStatus::None);
    let _ = std::fs::remove_dir_all(&dir);
}
