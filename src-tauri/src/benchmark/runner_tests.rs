//! runner 的單元測試(自 runner.rs 機械搬移,路徑不變:super = runner)。
use super::fake::{
    CancelAfterSleeper, FakeCancel, FakeEnvironmentProbe, FakeProcessRunner, FakeWindowController,
};
use super::*;
use crate::benchmark::assets::{self, BenchmarkAssets};
use crate::gpu::fake::FakeBackend;
use crate::gpu::{AffinityPolicy, GpuDevice, NoopSleeper, RegistryValueSnapshot};
use crate::topology::{build_topology, Topology};
use std::sync::atomic::Ordering;
use uuid::Uuid;

const GPU_A: &str = r"PCI\VEN_FAKE&DEV_1";

fn fixed_identity() -> CpuIdentity {
    CpuIdentity {
        architecture: 9,
        family: 6,
        model: 183,
        stepping: 1,
    }
}

fn topo() -> Topology {
    build_topology((0..8u32).map(|c| (vec![c], 0, false)).collect())
}

fn device(instance: &str) -> GpuDevice {
    GpuDevice {
        instance_id: instance.to_string(),
        friendly_name: format!("GPU {instance}"),
    }
}

/// 建立可通過 assets::verify 的暫存資源：從 vendored 資源目錄連結真實檔案。
/// verify 以內嵌 digest 比對，偽造內容無法通過；測試不會寫入這些檔，
/// hard link 安全（跨磁碟時退回 copy）。
/// d3d9-workload.exe 是 build 產物、不在 git 內：CI checkout 缺檔時以假內容
/// 補位（該環境 D3D9 digest 為 None，verify 僅檢查存在）。
fn make_assets(dir: &std::path::Path) -> BenchmarkAssets {
    std::fs::create_dir_all(dir).unwrap();
    let vendored = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources/benchmark");
    for file in [
        assets::PRESENTMON_FILE,
        assets::VULKAN_WORKLOAD_FILE,
        assets::D3D9_WORKLOAD_FILE,
    ] {
        let dst = dir.join(file);
        if dst.exists() {
            continue;
        }
        let src = vendored.join(file);
        if src.exists() {
            std::fs::hard_link(&src, &dst)
                .or_else(|_| std::fs::copy(&src, &dst).map(|_| ()))
                .unwrap();
        } else {
            std::fs::write(&dst, b"placeholder-d3d9-not-vendored").unwrap();
        }
    }
    assets::load(dir)
}

fn temp_root(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("pacedock_runner_{}_{}", std::process::id(), name));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn base_config() -> BenchmarkConfig {
    BenchmarkConfig {
        gpu_instance_id: Some(GPU_A.to_string()),
        workload: WorkloadKind::Vulkan,
        sample_secs: 3,
        warm_up_secs: 1,
        repetitions: 5,
        // 候選 LP 不得含 physical Core 0（測試拓撲 core 0 = LP 0）
        candidate_lps: vec![1, 2, 3],
        // 測試預設走 legacy Fixed 路徑（不跑校準），Adaptive 另有專屬測試。
        fps_cap_policy: FpsCapPolicy::Fixed,
        ..Default::default()
    }
}

/// 明確自訂尺寸 → workload_command 從 config.width/height 直接組出 args，
/// 不被 product default 覆寫（D3D9 路徑直接讀欄位，最貼近序列化後的值）。
#[test]
fn workload_command_preserves_explicit_dimensions() {
    let dir = temp_root("wl_cmd_dims");
    let assets = make_assets(&dir);
    let config = BenchmarkConfig {
        workload: WorkloadKind::D3D9,
        fullscreen: false,
        width: 800,
        height: 600,
        ..Default::default()
    };
    let (_exe, args) = workload_command(&assets, &config, config.fps_cap);
    assert!(args.contains(&"--width=800".to_string()));
    assert!(args.contains(&"--height=600".to_string()));
    assert!(args.contains(&"--fullscreen=0".to_string()));
    assert!(!args.contains(&"--width=1280".to_string()));
    assert!(!args.contains(&"--height=720".to_string()));
    let _ = std::fs::remove_dir_all(&dir);
}

/// 一組有效的 fake CSV（LP 不同 frametime；frametime 交替使 MAD > 0）
fn csv_for_lp(lp: u32) -> String {
    // LP 越低 fps 越高（frametime 越低）→ 讓 best_lp 可預期
    let base = 20.0 - (lp as f64) * 2.0; // LP0=20ms(50fps), LP1=18ms, LP2=16ms
    csv_with_base(base)
}

/// 依指定 base frametime 產生交替 frametime 的 fake CSV（MAD > 0；三位小數精確）。
fn csv_with_base(base: f64) -> String {
    let mut s = String::from("Application,ProcessID,msBetweenPresents\n");
    for i in 0..50 {
        let ft = if i % 2 == 0 { base } else { base + 0.5 };
        s.push_str(&format!("\"w (1)\",1,{ft:.3}\n"));
    }
    s
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn build_ctx(
    root: &std::path::Path,
    backend: Arc<dyn GpuBackend>,
    processes: Arc<dyn ProcessRunner>,
    cancel: Arc<dyn CancelSignal>,
    sleeper: Arc<dyn Sleep>,
    config: BenchmarkConfig,
    journal: &std::path::Path,
    on_progress: Option<Box<dyn FnMut(&BenchmarkProgress) + Send>>,
) -> RunContext {
    let session_id = Uuid::new_v4().to_string();
    let assets = make_assets(&root.join("assets"));
    RunContext {
        backend,
        sleeper,
        processes,
        cancel,
        env: Arc::new(FakeEnvironmentProbe::new()),
        topo: topo(),
        capture_quality: Default::default(),
        cpu_identity: fixed_identity(),
        assets,
        storage_root: root.join("benchmarks"),
        journal_path: journal.to_path_buf(),
        session_id,
        config,
        on_progress: on_progress.unwrap_or_else(|| Box::new(|_| {})),
        baseline: None,
        owned_processes: Vec::new(),
        window: Arc::new(fake::FakeWindow::new()),
        window_control: Arc::new(fake::FakeWindowController::new()),
        layout: None,
        on_integrity: Box::new(|_| {}),
        window_retries: 0,
        last_integrity: None,
    }
}

#[test]
fn progress_phase_decodes_raw_round_namespaces() {
    assert_eq!(
        progress_phase("collecting", Some(0)),
        (Some(BenchmarkPhase::Screening), Some(1))
    );
    assert_eq!(
        progress_phase("collecting", Some(SCREENING_ROUNDS)),
        (Some(BenchmarkPhase::Refinement), Some(1))
    );
    assert_eq!(
        progress_phase("collecting", Some(CONFIRMATION_ROUND_BASE + 2)),
        (Some(BenchmarkPhase::Confirmation), Some(3))
    );
    assert_eq!(
        progress_phase("collecting", Some(REVERSE_ROUND_BASE)),
        (Some(BenchmarkPhase::ReverseConfirmation), Some(1))
    );
    assert_eq!(
        progress_phase("collecting", Some(EQUIVALENT_VALIDATION_ROUND_BASE + 1)),
        (Some(BenchmarkPhase::EquivalentValidation), Some(2))
    );
    assert_eq!(progress_phase("calibrating", Some(0)), (None, None));
}

#[test]
fn panic_failure_kills_owned_processes_and_persists_failed_session() {
    let root = temp_root("panic_failure");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    let processes = Arc::new(FakeProcessRunner::new());
    let mut ctx = build_ctx(
        &root,
        backend as Arc<dyn GpuBackend>,
        processes.clone() as Arc<dyn ProcessRunner>,
        Arc::new(FakeCancel::new()) as Arc<dyn CancelSignal>,
        Arc::new(NoopSleeper) as Arc<dyn Sleep>,
        base_config(),
        &journal,
        None,
    );
    ctx.owned_processes.push(4242);

    let result = panic_failure(&mut ctx);
    assert_eq!(result.status, SessionStatus::Failed);
    assert_eq!(result.error.as_deref(), Some(codes::BENCHMARK_RUNNER_PANIC));
    assert!(processes.killed_log().contains(&4242));
    let stored = storage::get_at(&ctx.storage_root, &ctx.session_id).unwrap();
    assert_eq!(stored.summary.status, SessionStatus::Failed);
    assert_eq!(
        stored.summary.error.as_deref(),
        Some(codes::BENCHMARK_RUNNER_PANIC)
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// 建出一個 `window_control` 為自備 `FakeWindowController` 的 ctx（供 report_integrity 測試）。
fn ctx_with_window_control(
    root: &std::path::Path,
    journal: &std::path::Path,
) -> (RunContext, Arc<FakeWindowController>) {
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    let processes = Arc::new(FakeProcessRunner::new());
    let cancel = FakeCancel::new();
    let mut ctx = build_ctx(
        root,
        backend as Arc<dyn GpuBackend>,
        processes as Arc<dyn ProcessRunner>,
        Arc::new(cancel) as Arc<dyn CancelSignal>,
        Arc::new(NoopSleeper) as Arc<dyn Sleep>,
        base_config(),
        journal,
        None,
    );
    let wc = Arc::new(FakeWindowController::new());
    ctx.window_control = wc.clone();
    (ctx, wc)
}

#[test]
fn report_integrity_foreground_loss_requests_center_restore() {
    let root = temp_root("center_req");
    let journal = root.join("journal.json");
    let (mut ctx, wc) = ctx_with_window_control(&root, &journal);

    let loss = WindowIntegritySnapshot {
        foreground: false,
        ..Default::default()
    };
    report_integrity(&mut ctx, &loss, None);
    assert!(wc.center_requested.load(Ordering::SeqCst));

    // 重複相同 snapshot 不影響正確性：旗標維持 true、不 panic
    report_integrity(&mut ctx, &loss, None);
    assert!(wc.center_requested.load(Ordering::SeqCst));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn report_integrity_non_foreground_failure_does_not_request_center() {
    let root = temp_root("center_no_req");
    let journal = root.join("journal.json");
    let (mut ctx, wc) = ctx_with_window_control(&root, &journal);

    // minimized/position 失敗但 foreground 仍 true → 不要求置中
    let other = WindowIntegritySnapshot {
        foreground: true,
        minimized: true,
        position_ok: false,
        ..Default::default()
    };
    report_integrity(&mut ctx, &other, None);
    assert!(!wc.center_requested.load(Ordering::SeqCst));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn success_completes_with_best_lp_and_restores_exact_policy() {
    let root = temp_root("success");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    // 原始策略（DevicePolicy=4, override mask=0b1000 → LP 3）
    let baseline = AffinityPolicy {
        instance_id: GPU_A.to_string(),
        device_policy: RegistryValueSnapshot::dword(4),
        assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
    };
    backend.set_policy(baseline.clone());
    let processes = Arc::new(FakeProcessRunner::new());
    // 每 LP 不同 frametime（LP 越大越快）→ LP3 決定性勝出，產生 verified best。
    for lp in [1u32, 2, 3] {
        processes
            .presentmon_csv_by_lp
            .lock()
            .unwrap()
            .insert(lp, csv_for_lp(lp));
    }
    let cancel = FakeCancel::new();

    let mut ctx = build_ctx(
        &root,
        backend.clone() as Arc<dyn GpuBackend>,
        processes.clone() as Arc<dyn ProcessRunner>,
        Arc::new(cancel) as Arc<dyn CancelSignal>,
        Arc::new(NoopSleeper) as Arc<dyn Sleep>,
        base_config(),
        &journal,
        None,
    );
    let result = run_benchmark(&mut ctx);

    assert_eq!(
        result.status,
        SessionStatus::Completed,
        "err={:?}",
        result.error
    );
    assert!(result.best_lp.is_some());
    // 策略還原到原始（逐位元組）
    assert_eq!(backend.current_policy(GPU_A), baseline);
    assert!(!journal.exists(), "成功後日誌應清除");
    // session 已寫入
    let detail = storage::get_at(&ctx.storage_root, &ctx.session_id).unwrap();
    assert_eq!(detail.summary.status, SessionStatus::Completed);
    assert_eq!(detail.results.len(), 3);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn sync_workload_affinity_true_still_skips_workload_affinity() {
    // sync_workload_affinity 已棄用：即使傳入 true，runner 也不得
    // 設定 workload affinity。ProcessRunner trait 已無 affinity API surface，
    // 完成即驗證不受影響。
    let root = temp_root("sync_skip");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    let processes = Arc::new(FakeProcessRunner::new());
    processes
        .presentmon_csv
        .lock()
        .unwrap()
        .push_str(&csv_for_lp(0));
    let cancel = FakeCancel::new();
    let mut config = base_config();
    config.sync_workload_affinity = true; // 舊語意，但 runner 必須忽略

    let mut ctx = build_ctx(
        &root,
        backend as Arc<dyn GpuBackend>,
        processes.clone() as Arc<dyn ProcessRunner>,
        Arc::new(cancel) as Arc<dyn CancelSignal>,
        Arc::new(NoopSleeper) as Arc<dyn Sleep>,
        config,
        &journal,
        None,
    );
    let result = run_benchmark(&mut ctx);
    assert_eq!(
        result.status,
        SessionStatus::Completed,
        "sync_workload_affinity=true 仍不得影響 runner；應正常完成"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn success_merges_screening_refinement_and_confirmation_rounds() {
    // 新排程：3 篩選 round 全 LP + 2 refinement round（Top3）+ 5 確認 round（Top2）。
    // 同內容 CSV → 平手 → finalists = [1, 2]；確認效應為 0 → Equivalent，5 round 提早停。
    let root = temp_root("merge");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    let processes = Arc::new(FakeProcessRunner::new());
    processes
        .presentmon_csv
        .lock()
        .unwrap()
        .push_str(&csv_for_lp(0));
    let cancel = FakeCancel::new();
    let config = base_config(); // candidate_lps [1,2,3]

    let mut ctx = build_ctx(
        &root,
        backend as Arc<dyn GpuBackend>,
        processes as Arc<dyn ProcessRunner>,
        Arc::new(cancel) as Arc<dyn CancelSignal>,
        Arc::new(NoopSleeper) as Arc<dyn Sleep>,
        config,
        &journal,
        None,
    );
    let result = run_benchmark(&mut ctx);
    assert_eq!(result.status, SessionStatus::Completed);
    let sample_by_lp: HashMap<u32, u32> = result
        .detail
        .results
        .iter()
        .map(|r| (r.lp, r.sample_count))
        .collect();
    // finalists（LP1, LP2）合併 3 個 selection round + 5 確認 = 8 round；
    // 非 finalist（LP3）只測 3 個 selection round。
    assert_eq!(sample_by_lp[&1], 400);
    assert_eq!(sample_by_lp[&2], 400);
    assert_eq!(sample_by_lp[&3], 150);
    let _ = std::fs::remove_dir_all(&root);
}

// ── 可靠性（reliability）判定 ──────────────────────────────────────────

/// 寫出一個 (round, lp) 的 CSV，樣本 frametime 在 `frame_ms` 與 `frame_ms + 0.5`
/// 交替（越低 FPS 越高）。交替兩值使 MAD > 0，供 log-ratio 確認分數使用。
fn write_round_csv(dir: &Path, round: u32, lp: u32, frame_ms: f64) -> CapturedCsv {
    let path = dir.join(format!("round-{round}-lp-{lp}.csv"));
    let mut s = String::from("Application,ProcessID,msBetweenPresents\n");
    for i in 0..50 {
        let ft = if i % 2 == 0 { frame_ms } else { frame_ms + 0.5 };
        s.push_str(&format!("\"w (1)\",1,{ft:.3}\n"));
    }
    std::fs::write(&path, &s).unwrap();
    captured_csv(&path)
}

/// 完整（completed）且四項指標齊備的 LpResult fixture
fn lp_res(lp: u32, avg: f64, p1: f64, p01: f64, stdev: f64) -> LpResult {
    LpResult {
        lp,
        avg_fps: Some(avg),
        p1_low: Some(p1),
        p01_low: Some(p01),
        stdev_fps: Some(stdev),
        completed: true,
        ..Default::default()
    }
}

/// 建置 K 個確認 round（round CONFIRMATION_ROUND_BASE..+K）的
/// (candidate, runner) CSV map，逐 round 固定 frametime。
fn confirmation_csvs(
    dir: &Path,
    candidate: u32,
    runner: u32,
    k: u32,
    c_frame: f64,
    r_frame: f64,
) -> RoundCsvs {
    let mut m: RoundCsvs = HashMap::new();
    for round in CONFIRMATION_ROUND_BASE..(CONFIRMATION_ROUND_BASE + k) {
        m.entry(candidate)
            .or_default()
            .insert(round, write_round_csv(dir, round, candidate, c_frame));
        m.entry(runner)
            .or_default()
            .insert(round, write_round_csv(dir, round, runner, r_frame));
    }
    m
}

/// 建置確認 CSV map，逐 round 給定不同 (candidate, runner) frametime（供
/// 一致性規則測試：單一 round 效應刻意低於門檻）。
fn confirmation_csvs_varied(
    dir: &Path,
    candidate: u32,
    runner: u32,
    frames: &[(f64, f64)],
) -> RoundCsvs {
    let mut m: RoundCsvs = HashMap::new();
    for (i, &(cf, rf)) in frames.iter().enumerate() {
        let round = CONFIRMATION_ROUND_BASE + i as u32;
        m.entry(candidate)
            .or_default()
            .insert(round, write_round_csv(dir, round, candidate, cf));
        m.entry(runner)
            .or_default()
            .insert(round, write_round_csv(dir, round, runner, rf));
    }
    m
}

/// 由既有 CSV 檔建 CapturedCsv（測試用；等同 capture 完成當下的綁定）
fn captured_csv(path: &Path) -> CapturedCsv {
    CapturedCsv::capture(path).expect("測試 CSV 應可讀取解析")
}

/// capture 前的 stale CSV 無法刪除（被鎖定）→ fail closed，不進行 capture
#[test]
fn run_capture_fails_closed_when_stale_csv_cannot_be_removed() {
    let root = temp_root("stale_lock");
    let csv = root.join("out.csv");
    std::fs::write(
        &csv,
        "Application,ProcessID,msBetweenPresents\n\"w\",1,10.0\n",
    )
    .unwrap();

    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    let processes = Arc::new(FakeProcessRunner::new());
    processes
        .presentmon_csv
        .lock()
        .unwrap()
        .push_str(&csv_for_lp(0));
    let mut ctx = build_ctx(
        &root,
        backend as Arc<dyn GpuBackend>,
        processes as Arc<dyn ProcessRunner>,
        Arc::new(FakeCancel::new()) as Arc<dyn CancelSignal>,
        Arc::new(NoopSleeper) as Arc<dyn Sleep>,
        base_config(),
        &root.join("journal.json"),
        None,
    );

    // Windows 上以無 FILE_SHARE_DELETE 的 share mode 開檔即鎖定刪除
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        let _lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(1) // FILE_SHARE_READ only
            .open(&csv)
            .unwrap();
        assert!(
            std::fs::remove_file(&csv).is_err(),
            "測試前提：share_mode(1) 的 handle 應鎖住刪除"
        );
        let err =
            run_capture(&mut ctx, 0, 1, 999, &csv, 1, 0, 8192, 1, Rect::default()).unwrap_err();
        assert_eq!(
            err,
            codes::BENCHMARK_CAPTURE_MISSING,
            "鎖定的 stale CSV 必須 fail closed"
        );
    }
    #[cfg(not(windows))]
    {
        let _ = &csv;
        let _ = &mut ctx;
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// 驗證後遭置換的 CSV：digest 綁定必須偵測並拒絕
#[test]
fn captured_csv_detects_post_capture_replacement() {
    let root = temp_root("csv_bind");
    let path = root.join("out.csv");
    std::fs::write(&path, csv_for_lp(0)).unwrap();
    let captured = captured_csv(&path);
    assert!(captured.read().is_ok(), "原始內容應可讀取");

    std::fs::write(&path, csv_for_lp(1)).unwrap();
    assert!(
        captured.read().is_err(),
        "capture 後被置換的內容必須被 digest 檢查拒絕"
    );
    let _ = std::fs::remove_dir_all(&root);
}

// ── evaluate_forward（effects → verdict）──

#[test]
fn evaluate_forward_passes_when_all_effects_above() {
    let dir = temp_root("eval_pass");
    let round_csvs = confirmation_csvs(&dir, 0, 1, 3, 10.0, 11.0);
    assert!(matches!(
        evaluate_forward(&round_csvs, 0, 1, 3, CONFIRMATION_ROUND_BASE),
        ForwardVerdict::CandidatePassed
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn evaluate_forward_continues_when_one_effect_below_threshold() {
    let dir = temp_root("eval_early_consistent");
    let round_csvs =
        confirmation_csvs_varied(&dir, 0, 1, &[(10.0, 11.0), (10.0, 11.0), (10.0, 10.05)]);
    assert!(matches!(
        evaluate_forward(&round_csvs, 0, 1, 3, CONFIRMATION_ROUND_BASE),
        ForwardVerdict::Continue
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn evaluate_forward_equivalent_at_5_6_7_rounds() {
    let dir = temp_root("eval_equiv");
    // 7 輪皆可忽略差異 → K=5/6/7 皆 Equivalent；K<5 只做 decisive → Continue。
    let round_csvs = confirmation_csvs(&dir, 0, 1, 7, 10.0, 10.05);
    assert!(matches!(
        evaluate_forward(&round_csvs, 0, 1, 3, CONFIRMATION_ROUND_BASE),
        ForwardVerdict::Continue
    ));
    assert!(matches!(
        evaluate_forward(&round_csvs, 0, 1, 4, CONFIRMATION_ROUND_BASE),
        ForwardVerdict::Continue
    ));
    for k in [5u32, 6, 7] {
        assert_eq!(
            evaluate_forward(&round_csvs, 0, 1, k, CONFIRMATION_ROUND_BASE),
            ForwardVerdict::Equivalent,
            "K={k} 應判定 Equivalent"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn evaluate_forward_decisive_win_precedes_equivalent_at_six_rounds() {
    let dir = temp_root("eval_decisive6");
    // K=6 但候選決定性勝出（10 vs 11）→ 優先回 CandidatePassed，不誤判 Equivalent。
    let round_csvs = confirmation_csvs(&dir, 0, 1, 6, 10.0, 11.0);
    assert_eq!(
        evaluate_forward(&round_csvs, 0, 1, 6, CONFIRMATION_ROUND_BASE),
        ForwardVerdict::CandidatePassed
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn evaluate_forward_continues_on_straddle() {
    let dir = temp_root("eval_straddle");
    let mut round_csvs: RoundCsvs = HashMap::new();
    let frames = [(10.0, 11.0), (11.0, 10.0), (10.0, 11.0)];
    for (i, &(cf, rf)) in frames.iter().enumerate() {
        let round = CONFIRMATION_ROUND_BASE + i as u32;
        round_csvs
            .entry(0)
            .or_default()
            .insert(round, write_round_csv(&dir, round, 0, cf));
        round_csvs
            .entry(1)
            .or_default()
            .insert(round, write_round_csv(&dir, round, 1, rf));
    }
    assert!(matches!(
        evaluate_forward(&round_csvs, 0, 1, 3, CONFIRMATION_ROUND_BASE),
        ForwardVerdict::Continue
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn evaluate_forward_reversal_when_runner_decisively_beats_candidate() {
    let dir = temp_root("eval_reversal");
    // runner(1) 每個確認 round 都明顯較快（10 vs 11）→ 反相 criteria 勝出。
    let round_csvs = confirmation_csvs(&dir, 0, 1, 3, 11.0, 10.0);
    assert!(matches!(
        evaluate_forward(&round_csvs, 0, 1, 3, CONFIRMATION_ROUND_BASE),
        ForwardVerdict::RunnerUpReversal
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn evaluate_forward_continues_when_evidence_missing() {
    let dir = temp_root("eval_missing");
    let mut round_csvs: RoundCsvs = HashMap::new();
    for round in CONFIRMATION_ROUND_BASE..(CONFIRMATION_ROUND_BASE + 2) {
        round_csvs
            .entry(0)
            .or_default()
            .insert(round, write_round_csv(&dir, round, 0, 10.0));
    }
    for round in CONFIRMATION_ROUND_BASE..(CONFIRMATION_ROUND_BASE + 3) {
        round_csvs
            .entry(1)
            .or_default()
            .insert(round, write_round_csv(&dir, round, 1, 12.0));
    }
    assert!(matches!(
        evaluate_forward(&round_csvs, 0, 1, 3, CONFIRMATION_ROUND_BASE),
        ForwardVerdict::Continue
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

// ── 確認一致性門檻（confirmation_passed）──

#[test]
fn confirmation_passed_consistency_thresholds_3_to_7() {
    let rails = true;
    // K=3：需 3/3
    assert!(!confirmation_passed(&[1.0, 1.0, 0.0], 1.0, rails));
    assert!(confirmation_passed(&[1.0, 1.0, 1.0], 1.0, rails));
    // K=4：需 4/4
    assert!(!confirmation_passed(&[1.0, 1.0, 1.0, 0.0], 1.0, rails));
    assert!(confirmation_passed(&[1.0; 4], 1.0, rails));
    // K=5：需 ≥4/5
    assert!(confirmation_passed(&[1.0, 1.0, 1.0, 1.0, 0.0], 1.0, rails));
    assert!(!confirmation_passed(&[1.0, 1.0, 1.0, 0.0, 0.0], 1.0, rails));
    // K=6：需 ≥5/6
    assert!(confirmation_passed(
        &[1.0, 1.0, 1.0, 1.0, 1.0, 0.0],
        1.0,
        rails
    ));
    assert!(!confirmation_passed(
        &[1.0, 1.0, 1.0, 1.0, 0.0, 0.0],
        1.0,
        rails
    ));
    // K=7：需 ≥6/7
    assert!(confirmation_passed(
        &[1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 0.0],
        1.0,
        rails
    ));
    assert!(!confirmation_passed(
        &[1.0, 1.0, 1.0, 1.0, 1.0, 0.0, 0.0],
        1.0,
        rails
    ));
    // bootstrap 區間下界必須 > 門檻（等於門檻 → false）
    assert!(!confirmation_passed(
        &[1.0; 3],
        COMPOSITE_ADVANTAGE_MIN_PCT,
        rails
    ));
    // 護欄倒退 → false
    assert!(!confirmation_passed(&[1.0; 3], 1.0, false));
}

// ── 等效判定（raw median evidence + 單輪禁制）──

/// 含 MAD/spike 的 LpResult fixture（等效判定用）。
fn lp_raw(lp: u32, avg: f64, p1: f64, p01: f64, mad: f64, spike: f64) -> LpResult {
    LpResult {
        lp,
        avg_fps: Some(avg),
        p1_low: Some(p1),
        p01_low: Some(p01),
        frametime_mad_pct: Some(mad),
        spike_rate_pct: Some(spike),
        completed: true,
        ..Default::default()
    }
}

/// 把單一 (candidate, runner) 複製成 K 個完全相同的配對（模擬逐 round 重複量測）。
fn identical_pairs(c: LpResult, r: LpResult, k: usize) -> Vec<(LpResult, LpResult)> {
    (0..k).map(|_| (c.clone(), r.clone())).collect()
}

#[test]
fn equivalent_finalists_screenshot_case() {
    // 截圖型案例：avg 約 0、p1 −1.09%、p01 約 0、MAD/spike 極小 → Equivalent
    let c = lp_raw(0, 100.0, 90.0, 80.0, 5.0, 0.5);
    let r = lp_raw(1, 100.05, 91.0, 80.05, 5.2, 0.52);
    let pairs = identical_pairs(c, r, 5);
    let ev = equivalent_evidence(&pairs);
    // p1 改善 = (90 − 91)/91 × 100 ≈ −1.099%
    assert!(
        (ev.p1_improvement_pct.unwrap() + 1.099).abs() < 0.01,
        "p1={:?}",
        ev.p1_improvement_pct
    );
    assert!(ev.avg_improvement_pct.unwrap().abs() <= 0.5);
    assert!(ev.mad_delta_pp.unwrap().abs() <= 0.5);
    assert!(ev.spike_delta_pp.unwrap().abs() <= 0.10);
    assert!(
        equivalent_finalists(&pairs),
        "截圖型案例應判定為 Equivalent"
    );
}

#[test]
fn equivalent_medians_boundary_in_and_out() {
    // 界線內：五項皆在門檻內 → Equivalent
    let c = lp_raw(0, 100.0, 90.0, 80.0, 5.0, 0.5);
    let r_in = lp_raw(1, 100.4, 91.26, 81.5, 5.4, 0.58);
    let pairs_in = identical_pairs(c.clone(), r_in, 5);
    assert!(equivalent_finalists(&pairs_in), "界線內應 Equivalent");
    // 界線外：avg 差 >0.5%（100 vs 100.6 → −0.596%）→ 非 Equivalent
    let r_out = lp_raw(1, 100.6, 90.0, 80.0, 5.0, 0.5);
    let pairs_out = identical_pairs(c, r_out, 5);
    assert!(!equivalent_finalists(&pairs_out), "avg 超界應非 Equivalent");
}

#[test]
fn equivalent_single_round_severe_regression_forbidden() {
    // 4 輪可忽略 + 1 輪嚴重退步（avg 差 >3%、p1 差 >5%）→ 禁止 Equivalent
    let c_good = lp_raw(0, 100.0, 90.0, 80.0, 5.0, 0.5);
    let r_good = lp_raw(1, 100.05, 90.9, 80.1, 5.1, 0.51);
    let c_bad = lp_raw(0, 100.0, 90.0, 80.0, 5.0, 0.5);
    let r_bad = lp_raw(1, 104.0, 95.0, 84.0, 5.0, 0.5);
    let mut pairs = identical_pairs(c_good, r_good, 4);
    pairs.push((c_bad, r_bad));
    // 中位數仍落在門檻內，但單輪嚴重退步 → 禁制生效
    assert!(
        equivalent_medians_ok(&equivalent_evidence(&pairs)),
        "前置：中位數應仍在門檻內"
    );
    assert!(
        !equivalent_finalists(&pairs),
        "單輪嚴重退步應禁止 Equivalent"
    );
}

// ── 7 輪 Inconclusive + 反向預算 ──

#[test]
fn evaluate_forward_seven_round_straddle_is_inconclusive() {
    let dir = temp_root("eval_inconclusive");
    let mut round_csvs: RoundCsvs = HashMap::new();
    for i in 0..CONFIRMATION_MAX_ROUNDS {
        let round = CONFIRMATION_ROUND_BASE + i;
        let (cf, rf) = if i % 2 == 0 {
            (10.0, 11.0)
        } else {
            (11.0, 10.0)
        };
        round_csvs
            .entry(0)
            .or_default()
            .insert(round, write_round_csv(&dir, round, 0, cf));
        round_csvs
            .entry(1)
            .or_default()
            .insert(round, write_round_csv(&dir, round, 1, rf));
    }
    // 效應逐輪交替正負 → 各輪皆 Continue（含第 5、7 輪非 Equivalent）→ Inconclusive
    for r in [3u32, 4, 5, 6, 7] {
        assert_eq!(
            evaluate_forward(&round_csvs, 0, 1, r, CONFIRMATION_ROUND_BASE),
            ForwardVerdict::Continue,
            "round {r} 應 Continue"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn reverse_max_rounds_respects_budget_and_min_three() {
    assert_eq!(reverse_max_rounds(3), 5);
    assert_eq!(reverse_max_rounds(5), 5);
    assert_eq!(reverse_max_rounds(6), 4);
    assert_eq!(reverse_max_rounds(7), 3, "forward 7 → reverse 最多 3");
    // 總 pair ≤ 10，且反向至少 3（前向 ≤7 必成立）
    for f in CONFIRMATION_MIN_ROUNDS..=CONFIRMATION_MAX_ROUNDS {
        assert!(f + reverse_max_rounds(f) <= TOTAL_PAIR_BUDGET);
        assert!(reverse_max_rounds(f) >= CONFIRMATION_MIN_ROUNDS);
    }
}

// ── compute_reliability（verdict → status + evidence）──

#[test]
fn compute_reliability_maps_candidate_passed() {
    let dir = temp_root("rel_pass");
    let round_csvs = confirmation_csvs(&dir, 0, 1, 3, 10.0, 11.0);
    let results = vec![
        lp_res(0, 100.0, 100.0, 100.0, 0.0),
        lp_res(1, 90.909, 90.909, 90.909, 0.0),
    ];
    let rel = compute_reliability(
        &round_csvs,
        &results,
        &[0, 1],
        3,
        Some(ForwardVerdict::CandidatePassed),
        false,
        false,
        0,
    );
    assert_eq!(rel.status, ReliabilityStatus::Passed);
    assert_eq!(rel.candidate_lp, Some(0));
    assert_eq!(rel.runner_up_lp, Some(1));
    assert_eq!(rel.evaluated_rounds, 3);
    assert_eq!(rel.screening_rounds, SCREENING_ROUNDS);
    assert_eq!(rel.confirmation_rounds, 3);
    assert_eq!(rel.stopping_reason, "passed");
    assert_eq!(rel.forward_verdict, "passed");
    assert_eq!(rel.candidate_wins, 3);
    assert_eq!(rel.per_round_winners, vec![Some(0); 3]);
    assert!(rel.composite_advantage_pct.unwrap() > COMPOSITE_ADVANTAGE_MIN_PCT);
    assert!(rel.ci_lower_pct.unwrap() > COMPOSITE_ADVANTAGE_MIN_PCT);
    assert!(!rel.reverse_ran);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn compute_reliability_maps_reverse_passed() {
    let dir = temp_root("rel_reverse");
    let round_csvs = confirmation_csvs(&dir, 0, 1, 3, 10.0, 11.0);
    let results = vec![
        lp_res(0, 100.0, 100.0, 100.0, 0.0),
        lp_res(1, 90.909, 90.909, 90.909, 0.0),
    ];
    let rel = compute_reliability(
        &round_csvs,
        &results,
        &[0, 1],
        3,
        Some(ForwardVerdict::RunnerUpReversal),
        true,
        true,
        3,
    );
    assert_eq!(rel.status, ReliabilityStatus::Passed);
    assert_eq!(rel.stopping_reason, "reverse_passed");
    assert_eq!(rel.forward_verdict, "reversal");
    assert!(rel.reverse_ran);
    assert_eq!(rel.reverse_verdict, "passed");
    assert_eq!(rel.reverse_candidate_lp, Some(1));
    assert_eq!(rel.reverse_rounds, 3);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn compute_reliability_maps_equivalent() {
    let dir = temp_root("rel_equiv");
    let round_csvs = confirmation_csvs(&dir, 0, 1, 3, 10.0, 10.05);
    let results = vec![
        lp_res(0, 100.0, 100.0, 100.0, 0.0),
        lp_res(1, 99.502, 99.502, 99.502, 0.0),
    ];
    let rel = compute_reliability(
        &round_csvs,
        &results,
        &[0, 1],
        3,
        Some(ForwardVerdict::Equivalent),
        false,
        false,
        0,
    );
    assert_eq!(rel.status, ReliabilityStatus::Equivalent);
    assert_eq!(rel.stopping_reason, "equivalent");
    assert_eq!(rel.forward_verdict, "equivalent");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn compute_reliability_sets_algorithm_version_and_equivalent_evidence() {
    let dir = temp_root("rel_evidence");
    let round_csvs = confirmation_csvs(&dir, 0, 1, 5, 10.0, 10.05);
    let results = vec![
        lp_res(0, 100.0, 100.0, 100.0, 0.0),
        lp_res(1, 99.502, 99.502, 99.502, 0.0),
    ];
    let rel = compute_reliability(
        &round_csvs,
        &results,
        &[0, 1],
        5,
        Some(ForwardVerdict::Equivalent),
        false,
        false,
        0,
    );
    assert_eq!(rel.algorithm_version, 2);
    assert!(rel.equivalent_avg_improvement_pct.is_some());
    assert!(rel.equivalent_p1_improvement_pct.is_some());
    assert!(rel.equivalent_p01_improvement_pct.is_some());
    assert!(rel.equivalent_mad_delta_pp.is_some());
    assert!(rel.equivalent_spike_delta_pp.is_some());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn compute_reliability_inconclusive_on_reversal_without_reverse_pass() {
    let dir = temp_root("rel_rev_fail");
    let round_csvs = confirmation_csvs(&dir, 0, 1, 3, 10.0, 11.0);
    let results = vec![
        lp_res(0, 100.0, 100.0, 100.0, 0.0),
        lp_res(1, 90.909, 90.909, 90.909, 0.0),
    ];
    // RunnerUpReversal 但反向驗證未 Passed → Inconclusive，不得套用。
    let rel = compute_reliability(
        &round_csvs,
        &results,
        &[0, 1],
        3,
        Some(ForwardVerdict::RunnerUpReversal),
        true,
        false,
        3,
    );
    assert_eq!(rel.status, ReliabilityStatus::Inconclusive);
    assert_eq!(rel.reverse_verdict, "inconclusive");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn compute_reliability_ignores_screening_rounds() {
    let dir = temp_root("rel_no_leak");
    let mut round_csvs: RoundCsvs = HashMap::new();
    // 篩選 round（0..SCREENING_ROUNDS）候選較慢；確認 round（base..）候選較快。
    for round in 0..SCREENING_ROUNDS {
        round_csvs
            .entry(0)
            .or_default()
            .insert(round, write_round_csv(&dir, round, 0, 20.0));
        round_csvs
            .entry(1)
            .or_default()
            .insert(round, write_round_csv(&dir, round, 1, 10.0));
    }
    for round in CONFIRMATION_ROUND_BASE..(CONFIRMATION_ROUND_BASE + 3) {
        round_csvs
            .entry(0)
            .or_default()
            .insert(round, write_round_csv(&dir, round, 0, 10.0));
        round_csvs
            .entry(1)
            .or_default()
            .insert(round, write_round_csv(&dir, round, 1, 11.0));
    }
    let results = vec![
        lp_res(0, 100.0, 100.0, 100.0, 0.0),
        lp_res(1, 90.909, 90.909, 90.909, 0.0),
    ];
    let rel = compute_reliability(
        &round_csvs,
        &results,
        &[0, 1],
        3,
        Some(ForwardVerdict::CandidatePassed),
        false,
        false,
        0,
    );
    assert_eq!(rel.status, ReliabilityStatus::Passed);
    assert_eq!(rel.candidate_lp, Some(0));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn compute_reliability_inconclusive_without_two_finalists() {
    let dir = temp_root("rel_one_lp");
    let round_csvs: RoundCsvs = HashMap::new();
    let results = vec![lp_res(0, 100.0, 100.0, 100.0, 0.0)];
    let rel = compute_reliability(&round_csvs, &results, &[], 0, None, false, false, 0);
    assert_eq!(rel.status, ReliabilityStatus::Inconclusive);
    assert_eq!(rel.candidate_lp, None);
    assert_eq!(rel.runner_up_lp, None);
    assert_eq!(rel.stopping_reason, "inconclusive");
    let _ = std::fs::remove_dir_all(&dir);
}

/// bootstrap 穩定性區間完全確定：相同輸入兩次呼叫逐位元組相等；常數效應 → 區間
/// 退化為點；有散布 → 下界 ≤ 點估計 ≤ 上界。
#[test]
fn paired_bootstrap_interval_is_deterministic() {
    let e = vec![1.0, 2.0, 3.0, 4.0, 5.0];
    let a = paired_bootstrap_interval(&e);
    let b = paired_bootstrap_interval(&e);
    assert_eq!(a.0, b.0);
    assert_eq!(a.1, b.1);
    assert_eq!(a.2, b.2);
    assert_eq!(a.0, 3.0);
    assert!(a.1 <= a.0 && a.0 <= a.2);
    assert!(a.1 < a.2);
    let (p, lo, hi) = paired_bootstrap_interval(&[0.25, 0.25, 0.25]);
    assert_eq!(p, 0.25);
    assert_eq!(lo, 0.25);
    assert_eq!(hi, 0.25);
}

#[test]
fn cancel_before_start_marks_cancelled_and_restores() {
    let root = temp_root("cancel0");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    let baseline = AffinityPolicy {
        instance_id: GPU_A.to_string(),
        device_policy: RegistryValueSnapshot::dword(4),
        assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
    };
    backend.set_policy(baseline.clone());
    let processes = Arc::new(FakeProcessRunner::new());
    let cancel = FakeCancel::new();
    cancel.set(true); // 一開始就取消

    let mut ctx = build_ctx(
        &root,
        backend.clone() as Arc<dyn GpuBackend>,
        processes.clone() as Arc<dyn ProcessRunner>,
        Arc::new(cancel) as Arc<dyn CancelSignal>,
        Arc::new(NoopSleeper) as Arc<dyn Sleep>,
        base_config(),
        &journal,
        None,
    );
    let result = run_benchmark(&mut ctx);

    assert_eq!(result.status, SessionStatus::Cancelled);
    assert!(processes.spawn_log().is_empty(), "取消時不該啟動 workload");
    assert_eq!(backend.current_policy(GPU_A), baseline, "策略不得被改動");
    assert!(!journal.exists());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn cancel_during_lp_kills_owned_processes_and_restores() {
    let root = temp_root("cancelmid");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    let baseline = AffinityPolicy {
        instance_id: GPU_A.to_string(),
        device_policy: RegistryValueSnapshot::dword(4),
        assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
    };
    backend.set_policy(baseline.clone());
    let processes = Arc::new(FakeProcessRunner::new());
    processes
        .presentmon_csv
        .lock()
        .unwrap()
        .push_str(&csv_for_lp(0));
    let cancel = Arc::new(FakeCancel::new());
    // 環境閘（5×1000ms）+ restart 穩定（5000ms）之後、warmup 期間的 13000ms 處取消
    let sleeper = Arc::new(CancelAfterSleeper::new(cancel.clone(), 13000));

    let mut ctx = build_ctx(
        &root,
        backend.clone() as Arc<dyn GpuBackend>,
        processes.clone() as Arc<dyn ProcessRunner>,
        cancel as Arc<dyn CancelSignal>,
        sleeper.clone() as Arc<dyn Sleep>,
        base_config(),
        &journal,
        None,
    );
    let result = run_benchmark(&mut ctx);

    assert_eq!(result.status, SessionStatus::Cancelled);
    // 取消在 warmup 期間（16000ms 前）就被偵測，未睡滿 stabilize+warmup
    assert!(sleeper.elapsed_ms() < 16000);
    // workload 已啟動，取消後必須被終止
    assert!(processes
        .spawn_log()
        .iter()
        .any(|(n, _, _)| !n.contains("PresentMon")));
    assert!(
        !processes.killed_log().is_empty(),
        "取消時必須終止 owned 子程序"
    );
    assert_eq!(backend.current_policy(GPU_A), baseline, "必須還原原始策略");
    assert!(!journal.exists());
    let _ = std::fs::remove_dir_all(&root);
}

/// 取消清理階段以有序、單調（0..100）的 cancel_progress 事件回報：
/// stopping → restoring → finalizing，且不碰 benchmark `percentage` 語意。
#[test]
fn cancel_cleanup_emits_ordered_progress() {
    let root = temp_root("cancelprogress");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    let baseline = AffinityPolicy {
        instance_id: GPU_A.to_string(),
        device_policy: RegistryValueSnapshot::dword(4),
        assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
    };
    backend.set_policy(baseline.clone());
    let processes = Arc::new(FakeProcessRunner::new());
    processes
        .presentmon_csv
        .lock()
        .unwrap()
        .push_str(&csv_for_lp(0));
    let cancel = Arc::new(FakeCancel::new());
    // 環境閘（5×1000ms）+ restart 穩定（5000ms）之後、warmup 期間的 13000ms 處取消
    let sleeper = Arc::new(CancelAfterSleeper::new(cancel.clone(), 13000));

    let events: std::sync::Mutex<Vec<BenchmarkProgress>> = std::sync::Mutex::new(Vec::new());
    let ev = std::sync::Arc::new(events);
    let ev_clone = ev.clone();
    let mut ctx = build_ctx(
        &root,
        backend.clone() as Arc<dyn GpuBackend>,
        processes.clone() as Arc<dyn ProcessRunner>,
        cancel as Arc<dyn CancelSignal>,
        sleeper.clone() as Arc<dyn Sleep>,
        base_config(),
        &journal,
        Some(Box::new(move |p| {
            ev_clone.lock().unwrap().push(p.clone());
        })),
    );
    let result = run_benchmark(&mut ctx);

    assert_eq!(result.status, SessionStatus::Cancelled);
    // 提取取消事件（有 cancel_stage 者）
    let cancels: Vec<(String, u32)> = ev
        .lock()
        .unwrap()
        .iter()
        .filter_map(|p| {
            p.cancel_stage
                .as_ref()
                .map(|s| (s.clone(), p.cancel_progress.unwrap_or(0)))
        })
        .collect();
    let stages: Vec<&str> = cancels.iter().map(|(s, _)| s.as_str()).collect();
    let pos = |s: &str| {
        stages
            .iter()
            .position(|x| *x == s)
            .unwrap_or_else(|| panic!("缺少取消階段 {s}: {stages:?}"))
    };
    assert!(pos("stopping") < pos("restoring"));
    assert!(pos("restoring") < pos("finalizing"));
    // 百分比 0..100 且單調不倒退
    let mut last = 0u32;
    for (_s, pct) in &cancels {
        assert!(*pct <= 100, "取消百分比不可超過 100: {pct}");
        assert!(*pct >= last, "取消百分比不可倒退: {pct} < {last}");
        last = *pct;
    }
    assert_eq!(last, 100, "取消最終進度應達 100");
    // 取消事件不得改寫 benchmark percentage（維持 0，不冒充 benchmark 進度）
    for p in ev
        .lock()
        .unwrap()
        .iter()
        .filter(|p| p.cancel_stage.is_some())
    {
        assert_eq!(p.percentage, 0, "取消事件 percentage 應保持 0");
    }
    assert_eq!(backend.current_policy(GPU_A), baseline, "必須還原原始策略");
    assert!(!journal.exists());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn presentmon_command_uses_process_id() {
    // PresentMon 必須以 -process_id 篩選已 spawn 的 workload PID，確保 Vulkan workload
    // 正確收集 present 事件（-process_name 在此情境不建立 CSV）
    let cfg = base_config();
    let args = presentmon_command(
        cfg.sample_secs,
        PRESENTMON_CIRCULAR_BUFFER_SIZE,
        1234,
        Path::new("x.csv"),
        "test-session",
    );
    let id_idx = args.iter().position(|a| a == "--process_id").unwrap();
    assert_eq!(args[id_idx + 1], "1234");
    assert!(
        !args.iter().any(|a| a == "--process_name"),
        "不該使用 -process_name"
    );
    // 仍有 output/timed
    assert!(args.iter().any(|a| a == "--output_file"));
    assert!(args.iter().any(|a| a == "--timed"));
}

#[test]
fn presentmon_command_uses_process_id_for_d3d9() {
    let mut cfg = base_config();
    cfg.workload = WorkloadKind::D3D9;
    let args = presentmon_command(
        cfg.sample_secs,
        PRESENTMON_CIRCULAR_BUFFER_SIZE,
        5678,
        Path::new("x.csv"),
        "test-session",
    );
    let id_idx = args.iter().position(|a| a == "--process_id").unwrap();
    assert_eq!(args[id_idx + 1], "5678");
    assert!(
        !args.iter().any(|a| a == "--process_name"),
        "不該使用 -process_name"
    );
}

#[test]
fn validate_config_rejects_zero_sample_secs_but_ignores_repetitions() {
    let t = topo();
    let mut c = base_config();
    c.sample_secs = 0;
    assert_eq!(
        validate_config(&c, &t).unwrap_err(),
        codes::BENCHMARK_INVALID_CONFIG
    );
    c.sample_secs = 3;
    // 新排程固定 3 篩選 + 2 refinement + 3..=7 確認，`repetitions` 欄位被忽略（保留供舊 session 相容）。
    for legacy in [2u32, 3, 4, 5, 6, 7, 8] {
        c.repetitions = legacy;
        assert!(
            validate_config(&c, &t).is_ok(),
            "repetitions={legacy} 應被忽略"
        );
    }
}

#[test]
fn presentmon_command_includes_stale_session_cleanup() {
    // 上游 AutoGpuAffinity 語意：先停掉殘留 ETL session，避免 stale session 卡住 capture
    let cfg = base_config();
    let args = presentmon_command(
        cfg.sample_secs,
        PRESENTMON_CIRCULAR_BUFFER_SIZE,
        1234,
        Path::new("x.csv"),
        "test-session",
    );
    assert!(
        args.iter().any(|a| a == "--stop_existing_session"),
        "必須含 -stop_existing_session"
    );
    assert!(
        args.iter().any(|a| a == "--no_console_stats"),
        "必須含 --no_console_stats"
    );
    // -terminate_after_timed 讓 PresentMon 收集完自行退出，runner 才能有界等待
    assert!(args.iter().any(|a| a == "--terminate_after_timed"));
    assert!(args
        .windows(2)
        .any(|w| { w[0] == "--session_name" && w[1] == "test-session" }));
    assert!(args.iter().any(|a| a == "--v1_metrics"));
    assert!(args.windows(2).any(|w| {
        w[0] == "--set_circular_buffer_size" && w[1] == PRESENTMON_CIRCULAR_BUFFER_SIZE.to_string()
    }));
}

/// 低負載追蹤：停用 GPU/input（統計不需要），保留 display（frame 來源）。
/// 停用 display 會使 CSV 完全無法建立（本次 regression 根因）。
#[test]
fn presentmon_command_uses_low_overhead_tracking() {
    let cfg = base_config();
    let args = presentmon_command(
        cfg.sample_secs,
        PRESENTMON_CIRCULAR_BUFFER_SIZE,
        1234,
        Path::new("x.csv"),
        "test-session",
    );
    assert!(
        args.iter().any(|a| a == "--no_track_gpu"),
        "必須停用 GPU 追蹤以降低 ETW 負載: {args:?}"
    );
    assert!(
        args.iter().any(|a| a == "--no_track_input"),
        "必須停用 input 追蹤以降低 ETW 負載: {args:?}"
    );
    assert!(
        !args.iter().any(|a| a == "--no_track_display"),
        "不得停用 display 追蹤（唯一 frame 來源）: {args:?}"
    );
}

#[test]
fn presentmon_captures_by_process_id_from_spawned_workload() {
    let root = temp_root("pmpid");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    let processes = Arc::new(FakeProcessRunner::new());
    processes
        .presentmon_csv
        .lock()
        .unwrap()
        .push_str(&csv_for_lp(0));
    let cancel = FakeCancel::new();

    let mut ctx = build_ctx(
        &root,
        backend as Arc<dyn GpuBackend>,
        processes.clone() as Arc<dyn ProcessRunner>,
        Arc::new(cancel) as Arc<dyn CancelSignal>,
        Arc::new(NoopSleeper) as Arc<dyn Sleep>,
        base_config(),
        &journal,
        None,
    );
    let result = run_benchmark(&mut ctx);
    assert_eq!(
        result.status,
        SessionStatus::Completed,
        "err={:?}",
        result.error
    );

    let log = processes.spawn_log();
    // 依 spawn 順序：每個 PresentMon 用 -process_id 對應已 spawn 的 workload PID
    let mut workload_pids: Vec<u32> = Vec::new();
    let mut presentmon_seen = 0u32;
    for (name, pid, args) in &log {
        if name.contains("PresentMon") {
            presentmon_seen += 1;
            let id_idx = args.iter().position(|a| a == "--process_id").unwrap();
            let pm_filter_pid: u32 = args[id_idx + 1].parse().unwrap();
            assert!(
                workload_pids.contains(&pm_filter_pid),
                "PresentMon -process_id {pm_filter_pid} 必須對應已 spawn 的 workload PID"
            );
            assert!(
                !args.iter().any(|a| a == "--process_name"),
                "不該使用 -process_name"
            );
        } else {
            workload_pids.push(*pid);
        }
    }
    assert!(presentmon_seen >= 1, "至少一次 PresentMon capture");
    assert!(!workload_pids.is_empty(), "每個 round 都要啟動 workload");
    // 連續 capture 每個都新鮮有效：capture 後清理
    for wl_pid in &workload_pids {
        assert!(
            processes.killed_log().contains(wl_pid),
            "workload {wl_pid} 必須被清理"
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn workload_launch_failure_fails_and_restores() {
    let root = temp_root("wlfail");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    let baseline = AffinityPolicy {
        instance_id: GPU_A.to_string(),
        device_policy: RegistryValueSnapshot::dword(4),
        assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
    };
    backend.set_policy(baseline.clone());
    let processes = Arc::new(FakeProcessRunner::new());
    processes.fail_workload.store(true, Ordering::SeqCst);
    let cancel = FakeCancel::new();

    let mut ctx = build_ctx(
        &root,
        backend.clone() as Arc<dyn GpuBackend>,
        processes as Arc<dyn ProcessRunner>,
        Arc::new(cancel) as Arc<dyn CancelSignal>,
        Arc::new(NoopSleeper) as Arc<dyn Sleep>,
        base_config(),
        &journal,
        None,
    );
    let result = run_benchmark(&mut ctx);

    assert_eq!(result.status, SessionStatus::Failed);
    assert_eq!(
        result.error.as_deref(),
        Some(codes::BENCHMARK_WORKLOAD_FAILED)
    );
    assert_eq!(
        backend.current_policy(GPU_A),
        baseline,
        "失敗後必須還原原始策略"
    );
    assert!(!journal.exists());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn missing_csv_fails_with_partial_results_and_no_recommendation() {
    let root = temp_root("nocsvar");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    let baseline = AffinityPolicy {
        instance_id: GPU_A.to_string(),
        device_policy: RegistryValueSnapshot::dword(4),
        assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
    };
    backend.set_policy(baseline.clone());
    let processes = Arc::new(FakeProcessRunner::new());
    // LP 1 有資料；LP 2/3 的 PresentMon spawn 失敗 → 該 LP 無 CSV
    processes
        .presentmon_csv
        .lock()
        .unwrap()
        .push_str(&csv_for_lp(0));
    processes.fail_lp(2);
    processes.fail_lp(3);
    let cancel = FakeCancel::new();

    let mut ctx = build_ctx(
        &root,
        backend.clone() as Arc<dyn GpuBackend>,
        processes as Arc<dyn ProcessRunner>,
        Arc::new(cancel) as Arc<dyn CancelSignal>,
        Arc::new(NoopSleeper) as Arc<dyn Sleep>,
        base_config(), // candidate_lps 1,2,3
        &journal,
        None,
    );
    let result = run_benchmark(&mut ctx);
    assert_eq!(result.status, SessionStatus::Failed);
    assert_eq!(result.best_lp, None, "失敗不該有推薦");
    // 部分結果保留（LP 1 有完成）
    assert!(!result.detail.results.is_empty());
    // 部分 CSV 檔案保留
    let session_dir = ctx.storage_root.join(&ctx.session_id);
    let files = std::fs::read_dir(&session_dir).unwrap().count();
    assert!(files >= 1, "partial CSV 應保留");
    assert_eq!(
        backend.current_policy(GPU_A),
        baseline,
        "失敗後必須還原原始策略"
    );
    assert!(!journal.exists(), "還原成功應清日誌");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn invalid_csv_content_fails_session() {
    let root = temp_root("badcsv");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    let processes = Arc::new(FakeProcessRunner::new());
    // 垃圾 CSV（無 msBetweenPresents 欄）→ capture 驗證即失敗
    processes
        .presentmon_csv
        .lock()
        .unwrap()
        .push_str("garbage,not,csv\n1,2,3\n");
    let cancel = FakeCancel::new();

    let mut ctx = build_ctx(
        &root,
        backend as Arc<dyn GpuBackend>,
        processes as Arc<dyn ProcessRunner>,
        Arc::new(cancel) as Arc<dyn CancelSignal>,
        Arc::new(NoopSleeper) as Arc<dyn Sleep>,
        base_config(),
        &journal,
        None,
    );
    let result = run_benchmark(&mut ctx);
    assert_eq!(result.status, SessionStatus::Failed);
    assert_eq!(
        result.error.as_deref(),
        Some(codes::BENCHMARK_CAPTURE_EMPTY)
    );
    assert_eq!(result.best_lp, None);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn restore_failure_keeps_journal_and_marks_recovery_required() {
    let root = temp_root("restorefail");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    let baseline = AffinityPolicy {
        instance_id: GPU_A.to_string(),
        device_policy: RegistryValueSnapshot::dword(4),
        assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
    };
    backend.set_policy(baseline.clone());
    let processes = Arc::new(FakeProcessRunner::new());
    processes
        .presentmon_csv
        .lock()
        .unwrap()
        .push_str(&csv_for_lp(0));
    let cancel = FakeCancel::new();
    // 終結還原的 restart 一直失敗
    backend.disable_fails.store(true, Ordering::SeqCst);

    let mut ctx = build_ctx(
        &root,
        backend.clone() as Arc<dyn GpuBackend>,
        processes as Arc<dyn ProcessRunner>,
        Arc::new(cancel) as Arc<dyn CancelSignal>,
        Arc::new(NoopSleeper) as Arc<dyn Sleep>,
        base_config(),
        &journal,
        None,
    );
    let result = run_benchmark(&mut ctx);
    // 第一 LP 的 apply restart 就失敗 → 已寫入策略，還原 restart 也失敗
    assert_eq!(result.status, SessionStatus::Failed);
    assert!(result.recovery_required, "還原失敗必須要求 recovery");
    assert!(journal.exists(), "還原失敗必須保留日誌");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn progress_events_emitted_with_stages() {
    let root = temp_root("progress");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    let processes = Arc::new(FakeProcessRunner::new());
    processes
        .presentmon_csv
        .lock()
        .unwrap()
        .push_str(&csv_for_lp(0));
    let cancel = FakeCancel::new();
    let events: std::sync::Mutex<Vec<BenchmarkProgress>> = std::sync::Mutex::new(Vec::new());
    let ev = std::sync::Arc::new(events);
    let ev_clone = ev.clone();

    let mut ctx = build_ctx(
        &root,
        backend as Arc<dyn GpuBackend>,
        processes as Arc<dyn ProcessRunner>,
        Arc::new(cancel) as Arc<dyn CancelSignal>,
        Arc::new(NoopSleeper) as Arc<dyn Sleep>,
        base_config(),
        &journal,
        Some(Box::new(move |p| {
            ev_clone.lock().unwrap().push(p.clone());
        })),
    );
    run_benchmark(&mut ctx);
    let stages: Vec<String> = ev.lock().unwrap().iter().map(|p| p.stage.clone()).collect();
    assert!(stages.contains(&"applying".to_string()));
    assert!(stages.contains(&"collecting".to_string()));
    assert!(stages.contains(&"finalizing".to_string()));
    assert!(stages.iter().all(|s| !s.is_empty()));
    let _ = std::fs::remove_dir_all(&root);
}

// ── PresentMon capture 可靠性回歸測試 ────────────────────────────────

#[test]
fn presentmon_timeout_fails_and_restores_with_persisted_error() {
    let root = temp_root("pmtimeout");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    let baseline = AffinityPolicy {
        instance_id: GPU_A.to_string(),
        device_policy: RegistryValueSnapshot::dword(4),
        assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
    };
    backend.set_policy(baseline.clone());
    let processes = Arc::new(FakeProcessRunner::new());
    // PresentMon 卡住：wait_exit 一直回傳「未退出」
    processes.presentmon_timeout.store(true, Ordering::SeqCst);
    let cancel = FakeCancel::new();
    let mut config = base_config();
    config.candidate_lps = vec![1];

    let mut ctx = build_ctx(
        &root,
        backend.clone() as Arc<dyn GpuBackend>,
        processes.clone() as Arc<dyn ProcessRunner>,
        Arc::new(cancel) as Arc<dyn CancelSignal>,
        Arc::new(NoopSleeper) as Arc<dyn Sleep>,
        config,
        &journal,
        None,
    );
    let result = run_benchmark(&mut ctx);
    assert_eq!(result.status, SessionStatus::Failed);
    assert_eq!(
        result.error.as_deref(),
        Some(codes::BENCHMARK_PRESENTMON_TIMEOUT)
    );
    // 卡住的 PresentMon 與 workload 都必須被終止
    assert!(!processes.killed_log().is_empty());
    // 策略還原、日誌清除
    assert_eq!(backend.current_policy(GPU_A), baseline);
    assert!(!journal.exists());
    // 失敗原因已持久化（reload 後 UI 可顯示）
    let detail = storage::get_at(&ctx.storage_root, &ctx.session_id).unwrap();
    assert_eq!(
        detail.summary.error.as_deref(),
        Some(codes::BENCHMARK_PRESENTMON_TIMEOUT)
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn presentmon_no_output_file_fails() {
    let root = temp_root("nofile");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    let baseline = AffinityPolicy {
        instance_id: GPU_A.to_string(),
        device_policy: RegistryValueSnapshot::dword(4),
        assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
    };
    backend.set_policy(baseline.clone());
    let processes = Arc::new(FakeProcessRunner::new());
    // PresentMon 正常退出但沒寫出 CSV（stale session / 依賴缺失）
    processes
        .presentmon_write_csv
        .store(false, Ordering::SeqCst);
    let cancel = FakeCancel::new();
    let mut config = base_config();
    config.candidate_lps = vec![1];

    let mut ctx = build_ctx(
        &root,
        backend.clone() as Arc<dyn GpuBackend>,
        processes.clone() as Arc<dyn ProcessRunner>,
        Arc::new(cancel) as Arc<dyn CancelSignal>,
        Arc::new(NoopSleeper) as Arc<dyn Sleep>,
        config,
        &journal,
        None,
    );
    let result = run_benchmark(&mut ctx);
    assert_eq!(result.status, SessionStatus::Failed);
    assert_eq!(
        result.error.as_deref(),
        Some(codes::BENCHMARK_CAPTURE_MISSING)
    );
    assert_eq!(result.best_lp, None, "缺檔不該有推薦");
    assert_eq!(backend.current_policy(GPU_A), baseline, "必須還原策略");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn presentmon_header_only_output_fails_as_empty() {
    let root = temp_root("emptycsv");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    let baseline = AffinityPolicy {
        instance_id: GPU_A.to_string(),
        device_policy: RegistryValueSnapshot::dword(4),
        assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
    };
    backend.set_policy(baseline.clone());
    let processes = Arc::new(FakeProcessRunner::new());
    // 只有 header、沒有任何 frametime 資料
    processes
        .presentmon_csv
        .lock()
        .unwrap()
        .push_str("Application,ProcessID,msBetweenPresents\n");
    let cancel = FakeCancel::new();
    let mut config = base_config();
    config.candidate_lps = vec![1];

    let mut ctx = build_ctx(
        &root,
        backend.clone() as Arc<dyn GpuBackend>,
        processes.clone() as Arc<dyn ProcessRunner>,
        Arc::new(cancel) as Arc<dyn CancelSignal>,
        Arc::new(NoopSleeper) as Arc<dyn Sleep>,
        config,
        &journal,
        None,
    );
    let result = run_benchmark(&mut ctx);
    assert_eq!(result.status, SessionStatus::Failed);
    assert_eq!(
        result.error.as_deref(),
        Some(codes::BENCHMARK_CAPTURE_EMPTY)
    );
    assert_eq!(backend.current_policy(GPU_A), baseline);
    let _ = std::fs::remove_dir_all(&root);
}

/// 核心回歸：stale CSV（上一個 session 殘留、格式看似有效）絕不能被當成
/// 本次 capture 的輸出。capture 前必須先清除 stale 檔，本次沒產出新檔即失敗。
#[test]
fn stale_csv_cannot_be_mistaken_for_fresh_output() {
    let root = temp_root("stale");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    let baseline = AffinityPolicy {
        instance_id: GPU_A.to_string(),
        device_policy: RegistryValueSnapshot::dword(4),
        assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
    };
    backend.set_policy(baseline.clone());
    let processes = Arc::new(FakeProcessRunner::new());
    // 本次 capture 不產出任何新檔
    processes
        .presentmon_write_csv
        .store(false, Ordering::SeqCst);
    let cancel = FakeCancel::new();
    let mut config = base_config();
    config.candidate_lps = vec![1];

    let mut ctx = build_ctx(
        &root,
        backend.clone() as Arc<dyn GpuBackend>,
        processes.clone() as Arc<dyn ProcessRunner>,
        Arc::new(cancel) as Arc<dyn CancelSignal>,
        Arc::new(NoopSleeper) as Arc<dyn Sleep>,
        config,
        &journal,
        None,
    );
    // 在目標路徑預放「看似有效」的 stale CSV（50 個 frametime）
    let session_dir = ctx.storage_root.join(&ctx.session_id);
    let csv = session_dir.join("round-0-lp-1.csv");
    std::fs::create_dir_all(&session_dir).unwrap();
    std::fs::write(&csv, csv_for_lp(0)).unwrap();

    let result = run_benchmark(&mut ctx);
    // stale 不能讓 session「成功」：沒有新鮮輸出 → 失敗
    assert_eq!(
        result.status,
        SessionStatus::Failed,
        "stale CSV 不得被當成成功輸出"
    );
    assert_eq!(
        result.error.as_deref(),
        Some(codes::BENCHMARK_CAPTURE_MISSING)
    );
    // stale 檔已在 capture 前被確定性清除，不會殘留在最終 session
    assert!(!csv.exists(), "stale CSV 必須在 capture 前被刪除");
    assert_eq!(result.best_lp, None);
    assert_eq!(backend.current_policy(GPU_A), baseline);
    let _ = std::fs::remove_dir_all(&root);
}

/// 多個連續 LP capture：每個 LP 都必須有「新鮮有效」的 CSV 才算成功；
/// 前一個 LP 的輸出不能讓後續 LP 誤判。LP1 有效、LP3 無輸出 → 失敗且保留 LP1/LP2 部分結果。
#[test]
fn sequential_lp_captures_require_fresh_valid_csv_each() {
    let root = temp_root("seqfresh");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    let baseline = AffinityPolicy {
        instance_id: GPU_A.to_string(),
        device_policy: RegistryValueSnapshot::dword(4),
        assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
    };
    backend.set_policy(baseline.clone());
    let processes = Arc::new(FakeProcessRunner::new());
    // 對所有 LP 都寫有效 CSV；但 LP3 的 PresentMon 不產出檔案
    processes
        .presentmon_csv
        .lock()
        .unwrap()
        .push_str(&csv_for_lp(0));
    processes.fail_lp(3); // LP3 PresentMon spawn 失敗 → 該 LP 無新鮮 CSV
    let cancel = FakeCancel::new();
    let mut config = base_config();
    config.candidate_lps = vec![1, 2, 3];

    let mut ctx = build_ctx(
        &root,
        backend.clone() as Arc<dyn GpuBackend>,
        processes.clone() as Arc<dyn ProcessRunner>,
        Arc::new(cancel) as Arc<dyn CancelSignal>,
        Arc::new(NoopSleeper) as Arc<dyn Sleep>,
        config,
        &journal,
        None,
    );
    let result = run_benchmark(&mut ctx);
    assert_eq!(result.status, SessionStatus::Failed);
    assert_eq!(
        result.error.as_deref(),
        Some(codes::BENCHMARK_PRESENTMON_FAILED)
    );
    // LP1、LP2 已驗證的部分結果保留；LP3 未完成
    assert!(
        result.detail.results.iter().all(|r| r.completed),
        "保留的部分結果必須都已完成"
    );
    assert!(result.detail.results.len() < 3, "LP3 失敗不該有結果");
    assert_eq!(result.best_lp, None);
    assert_eq!(backend.current_policy(GPU_A), baseline);
    let _ = std::fs::remove_dir_all(&root);
}

/// 讀取某 capture 的診斷檔
fn read_diag(session_dir: &Path, round: u32, lp: u32) -> CaptureDiagnostics {
    let p = session_dir
        .join("diag")
        .join(format!("capture-round-{round}-lp-{lp}.json"));
    let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("診斷檔讀取失敗 {p:?}: {e}"));
    serde_json::from_str(&text).unwrap()
}

/// 成功 capture 必須寫出診斷：workload 全程存活、PM 正常退出、CSV 存在。
#[test]
fn success_capture_writes_diagnostics() {
    let root = temp_root("diagok");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    backend.set_policy(AffinityPolicy {
        instance_id: GPU_A.to_string(),
        device_policy: RegistryValueSnapshot::dword(4),
        assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
    });
    let processes = Arc::new(FakeProcessRunner::new());
    processes
        .presentmon_csv
        .lock()
        .unwrap()
        .push_str(&csv_for_lp(0));
    let cancel = FakeCancel::new();
    let mut config = base_config();
    config.candidate_lps = vec![1];

    let mut ctx = build_ctx(
        &root,
        backend.clone() as Arc<dyn GpuBackend>,
        processes as Arc<dyn ProcessRunner>,
        Arc::new(cancel) as Arc<dyn CancelSignal>,
        Arc::new(NoopSleeper) as Arc<dyn Sleep>,
        config,
        &journal,
        None,
    );
    let result = run_benchmark(&mut ctx);
    assert_eq!(
        result.status,
        SessionStatus::Completed,
        "err={:?}",
        result.error
    );

    let session_dir = ctx.storage_root.join(&ctx.session_id);
    let d = read_diag(&session_dir, 0, 1);
    assert_eq!(d.round, 0);
    assert_eq!(d.lp, 1);
    assert!(!d.started_at.is_empty(), "要有起始時間戳");
    assert!(d.finished_at.is_some(), "要有結束時間戳");
    assert_ne!(d.workload_pid, 0);
    assert!(d.workload_alive_before_pm, "啟動 PM 前 workload 必須活著");
    assert!(
        d.workload_alive_after_capture,
        "capture 後 workload 應仍活著"
    );
    assert_eq!(d.workload_exit_code, None, "存活中不該有 exit code");
    assert_ne!(d.presentmon_pid, 0);
    assert_eq!(d.presentmon_exit_code, Some(0), "PM 正常退出 exit code 0");
    assert!(d.wait_completed);
    assert!(!d.wait_timed_out);
    assert_eq!(d.wait_error, None);
    assert!(d.csv_exists, "成功 capture 必須有 CSV");
    assert!(d.csv_size_bytes > 0);
    assert_eq!(d.error, None);
    // bounded output tail 有被擷取（fake 有記錄）
    assert!(d.presentmon_stderr.contains("fake-presentmon-stderr"));
    // 診斷要記住 PresentMon 的篩選種類與值（未來 session 才看得出匹配目標）
    assert_eq!(d.capture_filter_kind, "process_id");
    assert!(
        !d.capture_filter_value.is_empty(),
        "capture_filter_value 必須記錄 PID"
    );
    let _: u32 = d
        .capture_filter_value
        .parse()
        .expect("capture_filter_value 必須為十進位 PID");
    let _ = std::fs::remove_dir_all(&root);
}

/// 缺檔失敗（presentmon 正常退出但沒產出 CSV）也要寫診斷，且 session.json
/// 仍可正常讀取（診斷檔不影響既有匯入/相容性）。
#[test]
fn missing_output_writes_diagnostics_and_keeps_session_readable() {
    let root = temp_root("diagmiss");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    backend.set_policy(AffinityPolicy {
        instance_id: GPU_A.to_string(),
        device_policy: RegistryValueSnapshot::dword(4),
        assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
    });
    let processes = Arc::new(FakeProcessRunner::new());
    processes
        .presentmon_write_csv
        .store(false, Ordering::SeqCst);
    let cancel = FakeCancel::new();
    let mut config = base_config();
    config.candidate_lps = vec![1];

    let mut ctx = build_ctx(
        &root,
        backend.clone() as Arc<dyn GpuBackend>,
        processes.clone() as Arc<dyn ProcessRunner>,
        Arc::new(cancel) as Arc<dyn CancelSignal>,
        Arc::new(NoopSleeper) as Arc<dyn Sleep>,
        config,
        &journal,
        None,
    );
    let result = run_benchmark(&mut ctx);
    assert_eq!(result.status, SessionStatus::Failed);
    assert_eq!(
        result.error.as_deref(),
        Some(codes::BENCHMARK_CAPTURE_MISSING)
    );

    // 診斷檔在失敗後仍存在
    let session_dir = ctx.storage_root.join(&ctx.session_id);
    let d = read_diag(&session_dir, 0, 1);
    assert!(d.workload_alive_before_pm, "啟動 PM 前 workload 活著");
    assert!(d.workload_alive_after_capture);
    assert_eq!(d.presentmon_exit_code, Some(0), "PM 正常退出但無輸出");
    assert!(d.wait_completed);
    assert!(!d.wait_timed_out);
    assert!(!d.csv_exists, "缺檔：csv 不該存在");
    assert_eq!(d.csv_size_bytes, 0);
    assert_eq!(d.error.as_deref(), Some(codes::BENCHMARK_CAPTURE_MISSING));
    // 失敗路徑也保留篩選資訊（PM 用 -process_id 對應的 workload PID）
    assert_eq!(d.capture_filter_kind, "process_id");
    assert!(
        !d.capture_filter_value.is_empty(),
        "capture_filter_value 必須記錄 PID"
    );
    let _: u32 = d
        .capture_filter_value
        .parse()
        .expect("capture_filter_value 必須為十進位 PID");

    // session.json 仍可正常讀取（診斷檔不破壞舊相容性）
    let detail = storage::get_at(&ctx.storage_root, &ctx.session_id).unwrap();
    assert_eq!(detail.summary.status, SessionStatus::Failed);
    assert_eq!(
        detail.summary.error.as_deref(),
        Some(codes::BENCHMARK_CAPTURE_MISSING)
    );
    // list 也照常（診斷檔只是多餘檔案）
    let summaries = storage::list_at(&ctx.storage_root).unwrap();
    assert!(summaries.iter().any(|s| s.id == ctx.session_id));
    // 診斷檔計入總位元組（dir_size 掃全部檔案）
    assert!(detail.summary.total_bytes > 0);
    let _ = std::fs::remove_dir_all(&root);
}

/// PresentMon 逾時卡住也寫診斷：wait_timed_out=true、PM exit code 未知。
#[test]
fn presentmon_timeout_writes_diagnostics() {
    let root = temp_root("diagtimeout");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    backend.set_policy(AffinityPolicy {
        instance_id: GPU_A.to_string(),
        device_policy: RegistryValueSnapshot::dword(4),
        assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
    });
    let processes = Arc::new(FakeProcessRunner::new());
    processes.presentmon_timeout.store(true, Ordering::SeqCst);
    let cancel = FakeCancel::new();
    let mut config = base_config();
    config.candidate_lps = vec![1];

    let mut ctx = build_ctx(
        &root,
        backend.clone() as Arc<dyn GpuBackend>,
        processes.clone() as Arc<dyn ProcessRunner>,
        Arc::new(cancel) as Arc<dyn CancelSignal>,
        Arc::new(NoopSleeper) as Arc<dyn Sleep>,
        config,
        &journal,
        None,
    );
    let result = run_benchmark(&mut ctx);
    assert_eq!(result.status, SessionStatus::Failed);
    assert_eq!(
        result.error.as_deref(),
        Some(codes::BENCHMARK_PRESENTMON_TIMEOUT)
    );

    let session_dir = ctx.storage_root.join(&ctx.session_id);
    let d = read_diag(&session_dir, 0, 1);
    assert!(d.wait_completed);
    assert!(d.wait_timed_out, "逾時必須記錄");
    assert_eq!(d.presentmon_exit_code, None, "未退出不該有 exit code");
    assert_eq!(
        d.error.as_deref(),
        Some(codes::BENCHMARK_PRESENTMON_TIMEOUT)
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// 相容性：session 資料夾裡有 diag 檔案時，get/list/delete 照常運作，
/// 且不含 diag 的舊 session.json 仍可載入（diag 是獨立檔案，不進 schema）。
#[test]
fn diagnostics_files_do_not_break_old_session_compat() {
    let root = temp_root("diagcompat");
    let storage_root = root.join("benchmarks");
    let id = Uuid::new_v4().to_string();
    let detail = SessionDetail {
        summary: crate::benchmark::SessionSummary {
            id: id.clone(),
            status: SessionStatus::Failed,
            started_at: "2026-08-11T00:00:00Z".into(),
            finished_at: Some("2026-08-11T00:01:00Z".into()),
            gpu_name: "Fake GPU".into(),
            gpu_instance_id: GPU_A.to_string(),
            cpu_fingerprint: "fixture".into(),
            best_lp: None,
            reliability: ReliabilitySummary::default(),
            severe_lps: vec![],
            sample_count: 0,
            total_bytes: 0,
            config: base_config(),
            error: Some(codes::BENCHMARK_CAPTURE_MISSING.to_string()),
            ..Default::default()
        },
        results: vec![],
        samples: vec![],
        ..Default::default()
    };
    storage::save_session_at(&storage_root, &detail).unwrap();
    // 放診斷檔（模擬新版本寫入）
    let diag_dir = storage_root.join(&id).join("diag");
    std::fs::create_dir_all(&diag_dir).unwrap();
    std::fs::write(
        diag_dir.join("capture-round-0-lp-0.json"),
        r#"{"round":0,"lp":0,"csvExists":false}"#,
    )
    .unwrap();

    let loaded = storage::get_at(&storage_root, &id).unwrap();
    assert_eq!(loaded.summary.status, SessionStatus::Failed);
    assert_eq!(
        loaded.summary.error.as_deref(),
        Some(codes::BENCHMARK_CAPTURE_MISSING)
    );
    let list = storage::list_at(&storage_root).unwrap();
    assert_eq!(list.len(), 1);
    assert!(list[0].total_bytes > 0, "診斷檔計入 total_bytes");
    storage::delete_at(&storage_root, &id).unwrap();
    assert!(!storage_root.join(&id).exists());
    let _ = std::fs::remove_dir_all(&root);
}

// ── capture retry 回歸測試 ─────────────────────────────────────────────

/// 第一次 capture MISSING → retry 後用新 workload PID 成功 → session Completed
#[test]
fn retry_missing_recovers_on_second_attempt() {
    let root = temp_root("retry_ok");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    backend.set_policy(AffinityPolicy {
        instance_id: GPU_A.to_string(),
        device_policy: RegistryValueSnapshot::dword(4),
        assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
    });
    let processes = Arc::new(FakeProcessRunner::new());
    processes
        .presentmon_csv
        .lock()
        .unwrap()
        .push_str(&csv_for_lp(0));
    // LP 1 第一次 missing（不寫 CSV），第二次成功
    processes.first_attempt_missing.lock().unwrap().insert(1);
    let cancel = FakeCancel::new();
    let mut config = base_config();
    config.candidate_lps = vec![1];

    let mut ctx = build_ctx(
        &root,
        backend.clone() as Arc<dyn GpuBackend>,
        processes.clone() as Arc<dyn ProcessRunner>,
        Arc::new(cancel) as Arc<dyn CancelSignal>,
        Arc::new(NoopSleeper) as Arc<dyn Sleep>,
        config,
        &journal,
        None,
    );
    let result = run_benchmark(&mut ctx);

    assert_eq!(
        result.status,
        SessionStatus::Completed,
        "retry 後應成功: err={:?}",
        result.error
    );
    // 單一 LP（N=1）無確認階段 → Inconclusive，無 best_lp；此測試重點是 retry 回收
    assert_eq!(result.best_lp, None);
    // 單一 LP 只跑一輪短篩：初次套用 + capture recovery 各重啟一次，
    // 加上終結還原 1 次 = 3 次。
    assert_eq!(
        backend.restart_count(),
        3,
        "missing capture retry 必須先重新啟動 GPU，再建立新 workload"
    );
    // 一輪中有兩個 workload PID（attempt 1 + retry）。
    let log = processes.spawn_log();
    let wl_pids: Vec<u32> = log
        .iter()
        .filter(|(n, _, _)| !n.contains("PresentMon"))
        .map(|(_, p, _)| *p)
        .collect();
    assert_eq!(wl_pids.len(), 2, "第一次 + retry 應各建立一個 workload PID");
    for p in &wl_pids {
        assert!(
            processes.killed_log().contains(p),
            "workload {p} 必須被清理"
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// 所有 capture attempt 都 MISSING → 最終失敗，無部分結果且 sample_count 為 0
#[test]
fn retry_missing_both_fails_cleanly() {
    let root = temp_root("retry_fail");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    backend.set_policy(AffinityPolicy {
        instance_id: GPU_A.to_string(),
        device_policy: RegistryValueSnapshot::dword(4),
        assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
    });
    let processes = Arc::new(FakeProcessRunner::new());
    // LP 1: 第一次與所有 retry 都 missing
    processes.first_attempt_missing.lock().unwrap().insert(1);
    processes
        .second_attempt_also_missing
        .lock()
        .unwrap()
        .insert(1);
    let cancel = FakeCancel::new();
    let mut config = base_config();
    config.candidate_lps = vec![1];

    let mut ctx = build_ctx(
        &root,
        backend.clone() as Arc<dyn GpuBackend>,
        processes.clone() as Arc<dyn ProcessRunner>,
        Arc::new(cancel) as Arc<dyn CancelSignal>,
        Arc::new(NoopSleeper) as Arc<dyn Sleep>,
        config,
        &journal,
        None,
    );
    let result = run_benchmark(&mut ctx);

    assert_eq!(result.status, SessionStatus::Failed);
    assert_eq!(
        result.error.as_deref(),
        Some(codes::BENCHMARK_CAPTURE_MISSING)
    );
    assert_eq!(result.best_lp, None, "失敗不該有推薦");
    assert!(
        result.detail.results.is_empty(),
        "無 LP 成功，不該有部分結果"
    );
    assert_eq!(result.detail.summary.sample_count, 0);
    // 每次 attempt 各 spawn 一個 workload
    let log = processes.spawn_log();
    let wl_pids: Vec<u32> = log
        .iter()
        .filter(|(n, _, _)| !n.contains("PresentMon"))
        .map(|(_, p, _)| *p)
        .collect();
    assert_eq!(
        wl_pids.len(),
        MAX_CAPTURE_ATTEMPTS as usize,
        "每次 capture attempt 各一個 workload"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// 尚無任何成功 capture 時，第一個候選 LP 經所有 retry 仍 MISSING →
/// fail-fast：立即終止 session，不再跑剩餘 LP/round，且進入 cleanup/restore。
#[test]
fn first_lp_missing_fails_fast_without_running_remaining_lps() {
    let root = temp_root("ff_missing");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    let baseline = AffinityPolicy {
        instance_id: GPU_A.to_string(),
        device_policy: RegistryValueSnapshot::dword(4),
        assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
    };
    backend.set_policy(baseline.clone());
    let processes = Arc::new(FakeProcessRunner::new());
    // LP 1 所有 attempt 都 MISSING
    processes.first_attempt_missing.lock().unwrap().insert(1);
    processes
        .second_attempt_also_missing
        .lock()
        .unwrap()
        .insert(1);
    let cancel = FakeCancel::new();
    let mut config = base_config();
    config.candidate_lps = vec![1, 2, 3];

    let mut ctx = build_ctx(
        &root,
        backend.clone() as Arc<dyn GpuBackend>,
        processes.clone() as Arc<dyn ProcessRunner>,
        Arc::new(cancel) as Arc<dyn CancelSignal>,
        Arc::new(NoopSleeper) as Arc<dyn Sleep>,
        config,
        &journal,
        None,
    );
    let result = run_benchmark(&mut ctx);

    assert_eq!(result.status, SessionStatus::Failed);
    assert_eq!(
        result.error.as_deref(),
        Some(codes::BENCHMARK_CAPTURE_MISSING)
    );
    assert_eq!(result.best_lp, None);
    assert!(
        result.detail.results.is_empty(),
        "無 LP 成功，不該有部分結果"
    );
    // 只測第一個 LP（其 MAX_CAPTURE_ATTEMPTS 次），後續 LP 不得 spawn workload
    let wl_spawns = processes
        .spawn_log()
        .iter()
        .filter(|(n, _, _)| !n.contains("PresentMon"))
        .count();
    assert_eq!(
        wl_spawns, MAX_CAPTURE_ATTEMPTS as usize,
        "fail-fast：僅第一個 LP 的 attempts，不跑剩餘 LP"
    );
    // 進入既有 cleanup/restore
    assert_eq!(backend.current_policy(GPU_A), baseline, "必須還原策略");
    assert!(!journal.exists(), "還原成功應清日誌");
    let _ = std::fs::remove_dir_all(&root);
}

/// 已有成功 capture（LP 1 完成）時，後續 LP（LP 2）經所有 retry 仍 MISSING →
/// 隔離該 LP 並繼續，保留已收集的部分結果，最終 Failed 且無推薦。
#[test]
fn later_lp_missing_isolates_and_continues() {
    let root = temp_root("isolate_missing");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    let baseline = AffinityPolicy {
        instance_id: GPU_A.to_string(),
        device_policy: RegistryValueSnapshot::dword(4),
        assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
    };
    backend.set_policy(baseline.clone());
    let processes = Arc::new(FakeProcessRunner::new());
    // LP 1 有資料；LP 2 所有 attempt 都 MISSING
    processes
        .presentmon_csv
        .lock()
        .unwrap()
        .push_str(&csv_for_lp(0));
    processes.first_attempt_missing.lock().unwrap().insert(2);
    processes
        .second_attempt_also_missing
        .lock()
        .unwrap()
        .insert(2);
    let cancel = FakeCancel::new();
    let mut config = base_config();
    config.candidate_lps = vec![1, 2];

    let mut ctx = build_ctx(
        &root,
        backend.clone() as Arc<dyn GpuBackend>,
        processes.clone() as Arc<dyn ProcessRunner>,
        Arc::new(cancel) as Arc<dyn CancelSignal>,
        Arc::new(NoopSleeper) as Arc<dyn Sleep>,
        config,
        &journal,
        None,
    );
    let result = run_benchmark(&mut ctx);

    assert_eq!(result.status, SessionStatus::Failed);
    assert_eq!(
        result.error.as_deref(),
        Some(codes::BENCHMARK_CAPTURE_MISSING)
    );
    assert_eq!(result.best_lp, None, "部分失敗不該有推薦");
    assert!(
        !result.detail.results.is_empty(),
        "已有成功 capture 應保留部分結果"
    );
    // LP 1 每 round 一次成功（3 篩選 round）+ LP 2 每 round 三次 attempt（3 篩選 round）
    let wl_spawns = processes
        .spawn_log()
        .iter()
        .filter(|(n, _, _)| !n.contains("PresentMon"))
        .count();
    assert_eq!(
        wl_spawns,
        SCREENING_ROUNDS as usize + SCREENING_ROUNDS as usize * MAX_CAPTURE_ATTEMPTS as usize,
        "LP1 每 round 一次 + LP2 每 round 三次 attempts"
    );
    assert_eq!(backend.current_policy(GPU_A), baseline, "必須還原策略");
    assert!(!journal.exists(), "還原成功應清日誌");
    let _ = std::fs::remove_dir_all(&root);
}

/// 第一次 capture EMPTY（僅 header）→ retry 成功
#[test]
fn retry_empty_recovers_on_second_attempt() {
    let root = temp_root("retry_empty");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    backend.set_policy(AffinityPolicy {
        instance_id: GPU_A.to_string(),
        device_policy: RegistryValueSnapshot::dword(4),
        assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
    });
    let processes = Arc::new(FakeProcessRunner::new());
    // full CSV for retry
    processes
        .presentmon_csv
        .lock()
        .unwrap()
        .push_str(&csv_for_lp(0));
    // LP 1: 第一次 empty（header-only）
    processes.first_attempt_empty.lock().unwrap().insert(1);
    let cancel = FakeCancel::new();
    let mut config = base_config();
    config.candidate_lps = vec![1];

    let mut ctx = build_ctx(
        &root,
        backend.clone() as Arc<dyn GpuBackend>,
        processes.clone() as Arc<dyn ProcessRunner>,
        Arc::new(cancel) as Arc<dyn CancelSignal>,
        Arc::new(NoopSleeper) as Arc<dyn Sleep>,
        config,
        &journal,
        None,
    );
    let result = run_benchmark(&mut ctx);

    assert_eq!(
        result.status,
        SessionStatus::Completed,
        "empty retry 後應成功: err={:?}",
        result.error
    );
    // 單一 LP（N=1）無確認階段 → Inconclusive，無 best_lp；此測試重點是 empty retry 回收
    assert_eq!(result.best_lp, None);
    let _ = std::fs::remove_dir_all(&root);
}

/// PresentMon spawn 失敗不觸發 retry（非 MISSING/EMPTY）
#[test]
fn presentmon_spawn_failure_does_not_retry() {
    let root = temp_root("noretry_pmfail");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    backend.set_policy(AffinityPolicy {
        instance_id: GPU_A.to_string(),
        device_policy: RegistryValueSnapshot::dword(4),
        assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
    });
    let processes = Arc::new(FakeProcessRunner::new());
    // PresentMon spawn 失敗 → BENCHMARK_PRESENTMON_FAILED，不該 retry
    processes.fail_lp(1);
    let cancel = FakeCancel::new();
    let mut config = base_config();
    config.candidate_lps = vec![1];

    let mut ctx = build_ctx(
        &root,
        backend.clone() as Arc<dyn GpuBackend>,
        processes.clone() as Arc<dyn ProcessRunner>,
        Arc::new(cancel) as Arc<dyn CancelSignal>,
        Arc::new(NoopSleeper) as Arc<dyn Sleep>,
        config,
        &journal,
        None,
    );
    let result = run_benchmark(&mut ctx);

    assert_eq!(result.status, SessionStatus::Failed);
    assert_eq!(
        result.error.as_deref(),
        Some(codes::BENCHMARK_PRESENTMON_FAILED)
    );
    // 只有一次 PresentMon spawn（沒 retry）
    let pm_spawns = processes
        .spawn_log()
        .iter()
        .filter(|(n, _, _)| n.contains("PresentMon"))
        .count();
    assert_eq!(pm_spawns, 1, "PresentMon spawn 失敗不該 retry");
    let _ = std::fs::remove_dir_all(&root);
}

/// ETW events lost 訊號的純函式偵測：大小寫、變體、誤判防護。
#[test]
fn stderr_etw_loss_detection() {
    // positive：數量前綴 / "Lost N" / 無數量 "were lost" / 大小寫變體
    assert!(stderr_has_etw_loss("Lost 9000000 ETW events"));
    assert!(stderr_has_etw_loss("ETW events lost"));
    assert!(stderr_has_etw_loss("123 ETW events lost"));
    assert!(stderr_has_etw_loss("Lost 123 ETW events"));
    assert!(stderr_has_etw_loss("ETW events were lost"));
    assert!(stderr_has_etw_loss("warning: events lost during capture"));
    assert!(stderr_has_etw_loss("Lost 123 events"));
    assert!(stderr_has_etw_loss("123 ETW EVENTS LOST"));
    assert!(stderr_has_etw_loss("EtW EvEnTs WeRe LoSt"));
    // negative：明確零遺失 / 否定表述不得誤判
    assert!(!stderr_has_etw_loss("0 ETW events lost"));
    assert!(!stderr_has_etw_loss("no ETW events lost"));
    assert!(!stderr_has_etw_loss("no events were lost"));
    assert!(!stderr_has_etw_loss("lost 0 events"));
    assert!(!stderr_has_etw_loss("without events lost"));
    assert!(!stderr_has_etw_loss("fake-presentmon-stderr"));
    assert!(!stderr_has_etw_loss(""));
    assert!(!stderr_has_etw_loss("etw session started"));
    assert!(!stderr_has_etw_loss("no lost here"));
    // 混合否定／肯定子句：否定只在其子句內生效，不得抑制另一子句的真正 loss
    assert!(stderr_has_etw_loss(
        "no events were lost; 9000 ETW events lost"
    ));
    assert!(stderr_has_etw_loss("0 events lost, 5000 ETW events lost"));
    assert!(stderr_has_etw_loss("error code 0; lost 5 ETW events"));
    assert!(stderr_has_etw_loss("lost 0 events; lost 9000 ETW events"));
    assert!(!stderr_has_etw_loss("no events lost; no ETW events lost"));
}

/// 解析 overflowed present events 數量（實測 PresentMon 措辭）。
#[test]
fn parse_overflowed_present_events_extracts_count() {
    let real = "warning: 47123 overflowed present events detected. This could be due to a high-fps application.\nConsider increasing the present event circular buffer size to a value larger than the default of 2048, e.g., --set_circular_buffer_size 4096.";
    assert_eq!(parse_overflowed_present_events(real), Some(47123));
    assert_eq!(
            parse_overflowed_present_events(
                "warning: 131072 overflowed present events detected. This could be due to a high-fps application."
            ),
            Some(131072)
        );
    assert_eq!(
        parse_overflowed_present_events("0 overflowed present events detected"),
        None
    );
    assert_eq!(
        parse_overflowed_present_events("no overflowed present events"),
        None
    );
    assert_eq!(
        parse_overflowed_present_events("fake-presentmon-stderr"),
        None
    );
    assert_eq!(parse_overflowed_present_events(""), None);
}

/// 解析 ETW events/buffers lost 數量（實測 PresentMon 措辭）。
#[test]
fn parse_etw_events_lost_extracts_count() {
    assert_eq!(
        parse_etw_events_lost("warning: 9000000 ETW events were lost."),
        Some(9000000)
    );
    assert_eq!(
        parse_etw_events_lost("warning: 3 ETW buffers were lost."),
        Some(3)
    );
    assert_eq!(parse_etw_events_lost("0 ETW events lost"), None);
    assert_eq!(parse_etw_events_lost("no events were lost"), None);
}

/// calibration_buffer：`max(8192, next_power_of_two(cap*8))`。
#[test]
fn calibration_buffer_sizes_correctly() {
    assert_eq!(calibration_buffer(240), 8192);
    assert_eq!(calibration_buffer(500), 8192);
    assert_eq!(calibration_buffer(1000), 8192);
    assert_eq!(calibration_buffer(2000), 16384);
    assert_eq!(calibration_buffer(4000), 32768);
}

/// 校準決策：增益 <10% 選「剛測試的新 tier」，非前一 tier。
#[test]
fn calibration_selects_new_clean_tier_when_gain_below_threshold() {
    // 前一 clean tier FPS=240、新 tier 500 FPS=250（增益 ~4.2% <10%）→ 選 500。
    assert_eq!(
        calibration_clean_decision(Some(240.0), 500, 250.0, false),
        Some(500)
    );
    // 增益 >10% → 繼續（None）。
    assert_eq!(
        calibration_clean_decision(Some(240.0), 500, 300.0, false),
        None
    );
    // 最後一個 tier clean → 選它（不管增益）。
    assert_eq!(
        calibration_clean_decision(Some(2000.0), 4000, 2100.0, true),
        Some(4000)
    );
    // 首 tier（無前一 clean FPS）→ 繼續。
    assert_eq!(calibration_clean_decision(None, 240, 240.0, false), None);
}

/// 有效 CSV + overflow warning → 拒絕（與 CSV 內容無關）。
#[test]
fn valid_csv_with_overflow_warning_is_rejected() {
    let dir = temp_root("integrity_overflow");
    let csv = dir.join("round-0-lp-1.csv");
    std::fs::write(
            &csv,
            "Application,ProcessID,msBetweenPresents,TimeInSeconds\n\"w\",1,10.0,0.0\n\"w\",1,10.0,1.0\n\"w\",1,10.0,2.0\n\"w\",1,10.0,3.0\n",
        )
        .unwrap();
    let integ = assess_capture_integrity(&csv, 3, 47123, false);
    assert_eq!(
        integ.code.as_deref(),
        Some(codes::BENCHMARK_CAPTURE_OVERFLOW)
    );
    assert_eq!(integ.reason.as_deref(), Some("overflowed_present_events"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// 有效 CSV + ETW lost → 拒絕。
#[test]
fn valid_csv_with_etw_loss_is_rejected() {
    let dir = temp_root("integrity_etw");
    let csv = dir.join("round-0-lp-1.csv");
    std::fs::write(
            &csv,
            "Application,ProcessID,msBetweenPresents,TimeInSeconds\n\"w\",1,10.0,0.0\n\"w\",1,10.0,1.0\n\"w\",1,10.0,2.0\n\"w\",1,10.0,3.0\n",
        )
        .unwrap();
    let integ = assess_capture_integrity(&csv, 3, 0, true);
    assert_eq!(
        integ.code.as_deref(),
        Some(codes::BENCHMARK_CAPTURE_ETW_LOST)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// 觀測時長 < 95% sample_secs → CSV_INVALID（duration）。
#[test]
fn capture_duration_below_95pct_is_rejected() {
    let dir = temp_root("integrity_duration");
    let csv = dir.join("round-0-lp-1.csv");
    std::fs::write(
            &csv,
            "Application,ProcessID,msBetweenPresents,TimeInSeconds\n\"w\",1,10.0,0.0\n\"w\",1,10.0,1.0\n\"w\",1,10.0,2.0\n",
        )
        .unwrap();
    let integ = assess_capture_integrity(&csv, 3, 0, false);
    assert_eq!(integ.code.as_deref(), Some(codes::BENCHMARK_CSV_INVALID));
    assert_eq!(integ.reason.as_deref(), Some("duration"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// 非單調 capture 時間 → CSV_INVALID（monotonic）。
#[test]
fn non_monotonic_capture_time_is_rejected() {
    let dir = temp_root("integrity_monotonic");
    let csv = dir.join("round-0-lp-1.csv");
    std::fs::write(
            &csv,
            "Application,ProcessID,msBetweenPresents,TimeInSeconds\n\"w\",1,10.0,3.0\n\"w\",1,10.0,1.0\n\"w\",1,10.0,2.0\n",
        )
        .unwrap();
    let integ = assess_capture_integrity(&csv, 3, 0, false);
    assert_eq!(integ.code.as_deref(), Some(codes::BENCHMARK_CSV_INVALID));
    assert_eq!(integ.reason.as_deref(), Some("monotonic"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// 完整有效 CSV（含 TimeInSeconds，時長 ≥95%）→ 通過，duration 記錄。
#[test]
fn valid_csv_passes_integrity_with_duration() {
    let dir = temp_root("integrity_ok");
    let csv = dir.join("round-0-lp-1.csv");
    std::fs::write(
            &csv,
            "Application,ProcessID,msBetweenPresents,TimeInSeconds\n\"w\",1,10.0,0.0\n\"w\",1,10.0,1.0\n\"w\",1,10.0,2.0\n\"w\",1,10.0,3.0\n",
        )
        .unwrap();
    let integ = assess_capture_integrity(&csv, 3, 0, false);
    assert_eq!(integ.code, None);
    assert_eq!(integ.reason, None);
    assert_eq!(integ.duration_secs, Some(3.0));
    let _ = std::fs::remove_dir_all(&dir);
}

/// ETW events lost + 無 CSV → 專用錯誤、不可重試、fail-fast（後續 LP 不跑），
/// 且進入既有 cleanup/restore（策略還原、日誌清除），診斷反映真實策略。
#[test]
fn etw_loss_fails_fast_and_does_not_retry() {
    let root = temp_root("etw_failfast");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    let baseline = AffinityPolicy {
        instance_id: GPU_A.to_string(),
        device_policy: RegistryValueSnapshot::dword(4),
        assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
    };
    backend.set_policy(baseline.clone());
    let processes = Arc::new(FakeProcessRunner::new());
    processes.presentmon_etw_loss.store(true, Ordering::SeqCst);
    let cancel = FakeCancel::new();
    let mut config = base_config();
    config.candidate_lps = vec![1, 2, 3];

    let mut ctx = build_ctx(
        &root,
        backend.clone() as Arc<dyn GpuBackend>,
        processes.clone() as Arc<dyn ProcessRunner>,
        Arc::new(cancel) as Arc<dyn CancelSignal>,
        Arc::new(NoopSleeper) as Arc<dyn Sleep>,
        config,
        &journal,
        None,
    );
    let result = run_benchmark(&mut ctx);

    assert_eq!(result.status, SessionStatus::Failed);
    assert_eq!(
        result.error.as_deref(),
        Some(codes::BENCHMARK_CAPTURE_ETW_LOST)
    );
    // 不可重試：只有一次 PresentMon spawn（ETW loss 不進 retry loop）
    let pm_spawns = processes
        .spawn_log()
        .iter()
        .filter(|(n, _, _)| n.contains("PresentMon"))
        .count();
    assert_eq!(pm_spawns, 1, "ETW loss 不得重試");
    // fail-fast：只測第一個 LP（round 0 的 LP1），後續 LP 不得繼續
    let wl_spawns = processes
        .spawn_log()
        .iter()
        .filter(|(n, _, _)| !n.contains("PresentMon"))
        .count();
    assert_eq!(wl_spawns, 1, "後續 LP 不得繼續");
    assert!(
        result.detail.results.is_empty(),
        "無 LP 成功，不該有部分結果"
    );
    // 進入既有 cleanup/restore
    assert_eq!(backend.current_policy(GPU_A), baseline, "必須還原策略");
    assert!(!journal.exists(), "還原成功應清日誌");
    // 診斷反映真實策略
    let session_dir = ctx.storage_root.join(&ctx.session_id);
    let d = read_diag(&session_dir, 0, 1);
    assert!(d.etw_events_lost, "診斷必須記錄 ETW loss");
    assert_eq!(d.error.as_deref(), Some(codes::BENCHMARK_CAPTURE_ETW_LOST));
    let _ = std::fs::remove_dir_all(&root);
}

/// overflow retry 後摘要仍保留第一次 overflow count，且 total/valid/invalid 累計正確。
#[test]
fn overflow_retry_preserves_first_overflow_count_in_summary() {
    let root = temp_root("overflow_retry_summary");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    backend.set_policy(AffinityPolicy {
        instance_id: GPU_A.to_string(),
        device_policy: RegistryValueSnapshot::dword(4),
        assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
    });
    let processes = Arc::new(FakeProcessRunner::new());
    processes
        .presentmon_csv
        .lock()
        .unwrap()
        .push_str(&csv_for_lp(0));
    // LP 1 每個 round 的第一次 attempt 都 overflow（stderr 帶 47123），retry 乾淨。
    processes.first_attempt_overflow.lock().unwrap().insert(1);
    let cancel = FakeCancel::new();
    let mut config = base_config();
    config.candidate_lps = vec![1];

    let mut ctx = build_ctx(
        &root,
        backend.clone() as Arc<dyn GpuBackend>,
        processes.clone() as Arc<dyn ProcessRunner>,
        Arc::new(cancel) as Arc<dyn CancelSignal>,
        Arc::new(NoopSleeper) as Arc<dyn Sleep>,
        config,
        &journal,
        None,
    );
    let result = run_benchmark(&mut ctx);
    assert_eq!(
        result.status,
        SessionStatus::Completed,
        "err={:?}",
        result.error
    );

    let q = &result.detail.summary.capture_quality;
    // 每 round：第一次 overflow（attempt 1）+ retry 成功（attempt 2）= 2 attempts。
    assert_eq!(q.total_captures, SCREENING_ROUNDS * 2);
    assert_eq!(q.valid_captures, SCREENING_ROUNDS);
    assert_eq!(q.invalid_captures, SCREENING_ROUNDS);
    assert_eq!(q.overflowed_present_events, 47123 * SCREENING_ROUNDS as u64);
    assert_eq!(q.etw_events_lost, 0);
    assert!(q.integrity_passed, "全部正式 capture 完整且 session 完成");
    let _ = std::fs::remove_dir_all(&root);
}

/// ETW loss 失敗摘要：失敗也保存已知累計（total/invalid/etw_events_lost），
/// 且 integrity_passed 為 false。
#[test]
fn etw_loss_summary_records_failure_accumulation() {
    let root = temp_root("etw_loss_summary");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    backend.set_policy(AffinityPolicy {
        instance_id: GPU_A.to_string(),
        device_policy: RegistryValueSnapshot::dword(4),
        assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
    });
    let processes = Arc::new(FakeProcessRunner::new());
    processes.presentmon_etw_loss.store(true, Ordering::SeqCst);
    let cancel = FakeCancel::new();
    let mut config = base_config();
    config.candidate_lps = vec![1];

    let mut ctx = build_ctx(
        &root,
        backend.clone() as Arc<dyn GpuBackend>,
        processes.clone() as Arc<dyn ProcessRunner>,
        Arc::new(cancel) as Arc<dyn CancelSignal>,
        Arc::new(NoopSleeper) as Arc<dyn Sleep>,
        config,
        &journal,
        None,
    );
    let result = run_benchmark(&mut ctx);
    assert_eq!(result.status, SessionStatus::Failed);
    assert_eq!(
        result.error.as_deref(),
        Some(codes::BENCHMARK_CAPTURE_ETW_LOST)
    );

    let q = &result.detail.summary.capture_quality;
    assert_eq!(q.total_captures, 1, "ETW loss 不可重試，只 1 次 attempt");
    assert_eq!(q.valid_captures, 0);
    assert_eq!(q.invalid_captures, 1);
    assert_eq!(q.etw_events_lost, 9000000);
    assert_eq!(q.overflowed_present_events, 0);
    assert!(!q.integrity_passed, "失敗 session 不該 integrity_passed");
    let _ = std::fs::remove_dir_all(&root);
}

/// retry 期間 cancel → Cancelled，不清除已收集的部分
#[test]
fn cancel_during_retry_aborts_and_restores() {
    let root = temp_root("retry_cancel");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    let baseline = AffinityPolicy {
        instance_id: GPU_A.to_string(),
        device_policy: RegistryValueSnapshot::dword(4),
        assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
    };
    backend.set_policy(baseline.clone());
    let processes = Arc::new(FakeProcessRunner::new());
    processes
        .presentmon_csv
        .lock()
        .unwrap()
        .push_str(&csv_for_lp(0));
    // LP 1 第一次 missing → 觸發 retry；retry 期間 cancel
    processes.first_attempt_missing.lock().unwrap().insert(1);
    let cancel = Arc::new(FakeCancel::new());
    // 累計 13000ms 取消：落在 retry restart 穩定（11000..16000ms）期間，
    // 此時不應建立 retry workload。
    let sleeper = Arc::new(CancelAfterSleeper::new(cancel.clone(), 13000));
    let mut config = base_config();
    config.candidate_lps = vec![1];

    let mut ctx = build_ctx(
        &root,
        backend.clone() as Arc<dyn GpuBackend>,
        processes.clone() as Arc<dyn ProcessRunner>,
        cancel as Arc<dyn CancelSignal>,
        sleeper as Arc<dyn Sleep>,
        config,
        &journal,
        None,
    );
    let result = run_benchmark(&mut ctx);

    assert_eq!(result.status, SessionStatus::Cancelled);
    assert_eq!(backend.current_policy(GPU_A), baseline, "必須還原原始策略");
    assert!(!journal.exists());
    let _ = std::fs::remove_dir_all(&root);
}

/// capture wait（PresentMon 卡住不退出）期間取消 → 提前中斷、終止 owned、
/// 還原策略、狀態 Cancelled。
#[test]
fn cancel_during_capture_wait_interrupts_and_kills() {
    let root = temp_root("cancel_capture");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    let baseline = AffinityPolicy {
        instance_id: GPU_A.to_string(),
        device_policy: RegistryValueSnapshot::dword(4),
        assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
    };
    backend.set_policy(baseline.clone());
    let processes = Arc::new(FakeProcessRunner::new());
    // PresentMon 卡住：wait_exit 一直 Ok(false)
    processes.presentmon_timeout.store(true, Ordering::SeqCst);
    let cancel = Arc::new(FakeCancel::new());
    // stabilize(5000) + warmup(6000) = 11000ms 後進入 capture wait；
    // 11500ms 處取消（capture 開始後 ~500ms），不該等到 18s 逾時。
    let sleeper = Arc::new(CancelAfterSleeper::new(cancel.clone(), 11500));
    let mut config = base_config();
    config.candidate_lps = vec![1];

    let mut ctx = build_ctx(
        &root,
        backend.clone() as Arc<dyn GpuBackend>,
        processes.clone() as Arc<dyn ProcessRunner>,
        cancel as Arc<dyn CancelSignal>,
        sleeper as Arc<dyn Sleep>,
        config,
        &journal,
        None,
    );
    let result = run_benchmark(&mut ctx);

    assert_eq!(result.status, SessionStatus::Cancelled);
    assert!(
        !processes.killed_log().is_empty(),
        "取消時必須終止 owned 子程序"
    );
    assert_eq!(backend.current_policy(GPU_A), baseline, "必須還原原始策略");
    assert!(!journal.exists());
    let _ = std::fs::remove_dir_all(&root);
}

/// Vulkan windowed → 對 workload PID 要求 client area 設成 config width×height
#[test]
fn windowed_vulkan_resizes_client_area_to_config() {
    let root = temp_root("win_vk");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    backend.set_policy(AffinityPolicy {
        instance_id: GPU_A.to_string(),
        device_policy: RegistryValueSnapshot::dword(4),
        assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
    });
    let processes = Arc::new(FakeProcessRunner::new());
    processes
        .presentmon_csv
        .lock()
        .unwrap()
        .push_str(&csv_for_lp(0));
    let cancel = FakeCancel::new();
    let window = Arc::new(fake::FakeWindow::new());
    let mut config = base_config();
    config.candidate_lps = vec![1];
    config.fullscreen = false;
    config.width = 640;
    config.height = 480;

    let mut ctx = build_ctx(
        &root,
        backend.clone() as Arc<dyn GpuBackend>,
        processes.clone() as Arc<dyn ProcessRunner>,
        Arc::new(cancel) as Arc<dyn CancelSignal>,
        Arc::new(NoopSleeper) as Arc<dyn Sleep>,
        config,
        &journal,
        None,
    );
    ctx.window = window.clone();
    let result = run_benchmark(&mut ctx);
    assert_eq!(
        result.status,
        SessionStatus::Completed,
        "err={:?}",
        result.error
    );

    // 每次 spawn workload（含 retry）都要求 resize 成 (640, 480)
    let wl_pids: Vec<u32> = processes
        .spawn_log()
        .iter()
        .filter(|(n, _, _)| !n.contains("PresentMon"))
        .map(|(_, p, _)| *p)
        .collect();
    let calls = window.calls_log();
    assert!(!calls.is_empty(), "windowed Vulkan 必須呼叫 resize");
    for (pid, w, h) in &calls {
        assert!(
            wl_pids.contains(pid),
            "resize 目標必須是 spawned workload PID"
        );
        assert_eq!(*w, 640);
        assert_eq!(*h, 480);
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// fullscreen=true → validate_config 拒絕（強制視窗模式）
#[test]
fn fullscreen_is_rejected_by_validate_config() {
    let mut config = base_config();
    config.fullscreen = true;
    let err = validate_config(&config, &topo()).unwrap_err();
    assert_eq!(err, codes::BENCHMARK_INVALID_CONFIG);
}

/// fullscreen=true → run_benchmark 立即 Failed（不進入 capture）
#[test]
fn fullscreen_benchmark_fails_fast() {
    let root = temp_root("fs_reject");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    backend.set_policy(AffinityPolicy {
        instance_id: GPU_A.to_string(),
        device_policy: RegistryValueSnapshot::dword(4),
        assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
    });
    let processes = Arc::new(FakeProcessRunner::new());
    processes
        .presentmon_csv
        .lock()
        .unwrap()
        .push_str(&csv_for_lp(0));
    let cancel = FakeCancel::new();
    let mut config = base_config();
    config.candidate_lps = vec![1];
    config.fullscreen = true;

    let mut ctx = build_ctx(
        &root,
        backend.clone() as Arc<dyn GpuBackend>,
        processes.clone() as Arc<dyn ProcessRunner>,
        Arc::new(cancel) as Arc<dyn CancelSignal>,
        Arc::new(NoopSleeper) as Arc<dyn Sleep>,
        config,
        &journal,
        None,
    );
    let result = run_benchmark(&mut ctx);
    assert_eq!(result.status, SessionStatus::Failed);
    assert_eq!(
        result.error.as_deref(),
        Some(codes::BENCHMARK_INVALID_CONFIG)
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// D3D9（即使非 fullscreen）→ 不強制 resize
#[test]
fn d3d9_does_not_resize() {
    let root = temp_root("d3d9");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    backend.set_policy(AffinityPolicy {
        instance_id: GPU_A.to_string(),
        device_policy: RegistryValueSnapshot::dword(4),
        assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
    });
    let processes = Arc::new(FakeProcessRunner::new());
    processes
        .presentmon_csv
        .lock()
        .unwrap()
        .push_str(&csv_for_lp(0));
    let cancel = FakeCancel::new();
    let window = Arc::new(fake::FakeWindow::new());
    let mut config = base_config();
    config.candidate_lps = vec![1];
    config.workload = WorkloadKind::D3D9;
    config.fullscreen = false;

    let mut ctx = build_ctx(
        &root,
        backend.clone() as Arc<dyn GpuBackend>,
        processes.clone() as Arc<dyn ProcessRunner>,
        Arc::new(cancel) as Arc<dyn CancelSignal>,
        Arc::new(NoopSleeper) as Arc<dyn Sleep>,
        config,
        &journal,
        None,
    );
    ctx.window = window.clone();
    let result = run_benchmark(&mut ctx);
    assert_eq!(result.status, SessionStatus::Completed);
    assert!(window.calls_log().is_empty(), "D3D9 不該 resize");
    let _ = std::fs::remove_dir_all(&root);
}

/// 內建 Vulkan windowed → 安裝關閉防護 + resize client area
#[test]
fn windowed_vulkan_guards_close_and_resizes() {
    let root = temp_root("guard_win");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    backend.set_policy(AffinityPolicy {
        instance_id: GPU_A.to_string(),
        device_policy: RegistryValueSnapshot::dword(4),
        assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
    });
    let processes = Arc::new(FakeProcessRunner::new());
    processes
        .presentmon_csv
        .lock()
        .unwrap()
        .push_str(&csv_for_lp(0));
    let cancel = FakeCancel::new();
    let window = Arc::new(fake::FakeWindow::new());
    let mut config = base_config();
    config.candidate_lps = vec![1];
    config.fullscreen = false;

    let mut ctx = build_ctx(
        &root,
        backend.clone() as Arc<dyn GpuBackend>,
        processes.clone() as Arc<dyn ProcessRunner>,
        Arc::new(cancel) as Arc<dyn CancelSignal>,
        Arc::new(NoopSleeper) as Arc<dyn Sleep>,
        config,
        &journal,
        None,
    );
    ctx.window = window.clone();
    let result = run_benchmark(&mut ctx);
    assert_eq!(result.status, SessionStatus::Completed);

    let wl_pids: Vec<u32> = processes
        .spawn_log()
        .iter()
        .filter(|(n, _, _)| !n.contains("PresentMon"))
        .map(|(_, p, _)| *p)
        .collect();
    let guards = window.guard_calls_log();
    assert!(!guards.is_empty(), "windowed Vulkan 必須安裝關閉防護");
    for pid in &guards {
        assert!(
            wl_pids.contains(pid),
            "guard 目標必須是 spawned workload PID"
        );
    }
    assert!(
        !window.calls_log().is_empty(),
        "windowed Vulkan 仍須 resize client area"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// D3D9 → 不安裝關閉防護、不 resize
#[test]
fn d3d9_does_not_guard() {
    let root = temp_root("guard_d3d9");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    backend.set_policy(AffinityPolicy {
        instance_id: GPU_A.to_string(),
        device_policy: RegistryValueSnapshot::dword(4),
        assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
    });
    let processes = Arc::new(FakeProcessRunner::new());
    processes
        .presentmon_csv
        .lock()
        .unwrap()
        .push_str(&csv_for_lp(0));
    let cancel = FakeCancel::new();
    let window = Arc::new(fake::FakeWindow::new());
    let mut config = base_config();
    config.candidate_lps = vec![1];
    config.workload = WorkloadKind::D3D9;
    config.fullscreen = false;

    let mut ctx = build_ctx(
        &root,
        backend.clone() as Arc<dyn GpuBackend>,
        processes.clone() as Arc<dyn ProcessRunner>,
        Arc::new(cancel) as Arc<dyn CancelSignal>,
        Arc::new(NoopSleeper) as Arc<dyn Sleep>,
        config,
        &journal,
        None,
    );
    ctx.window = window.clone();
    let result = run_benchmark(&mut ctx);
    assert_eq!(result.status, SessionStatus::Completed);
    assert!(window.guard_calls_log().is_empty(), "D3D9 不該安裝關閉防護");
    assert!(window.calls_log().is_empty(), "D3D9 不該 resize");
    let _ = std::fs::remove_dir_all(&root);
}

/// guard 安裝失敗（helper 回 Err）→ 只 log warn，benchmark 仍正常完成
#[test]
fn guard_failure_does_not_fail_benchmark() {
    let root = temp_root("guard_fail");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    backend.set_policy(AffinityPolicy {
        instance_id: GPU_A.to_string(),
        device_policy: RegistryValueSnapshot::dword(4),
        assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
    });
    let processes = Arc::new(FakeProcessRunner::new());
    processes
        .presentmon_csv
        .lock()
        .unwrap()
        .push_str(&csv_for_lp(0));
    let cancel = FakeCancel::new();
    let window = Arc::new(fake::FakeWindow::new());
    window
        .guard_result
        .lock()
        .unwrap()
        .replace(Err("guard 失敗".to_string()));
    let mut config = base_config();
    config.candidate_lps = vec![1];
    config.fullscreen = false;

    let mut ctx = build_ctx(
        &root,
        backend.clone() as Arc<dyn GpuBackend>,
        processes.clone() as Arc<dyn ProcessRunner>,
        Arc::new(cancel) as Arc<dyn CancelSignal>,
        Arc::new(NoopSleeper) as Arc<dyn Sleep>,
        config,
        &journal,
        None,
    );
    ctx.window = window.clone();
    let result = run_benchmark(&mut ctx);
    assert_eq!(
        result.status,
        SessionStatus::Completed,
        "guard 失敗不得中斷 benchmark: err={:?}",
        result.error
    );
    assert!(
        !window.guard_calls_log().is_empty(),
        "guard 失敗前仍應有呼叫紀錄"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// 驗證 retry 後所有新舊 workload/PresentMon PID 都被清理
#[test]
fn retry_cleans_up_all_pids() {
    let root = temp_root("retry_clean");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    backend.set_policy(AffinityPolicy {
        instance_id: GPU_A.to_string(),
        device_policy: RegistryValueSnapshot::dword(4),
        assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
    });
    let processes = Arc::new(FakeProcessRunner::new());
    processes
        .presentmon_csv
        .lock()
        .unwrap()
        .push_str(&csv_for_lp(0));
    // LP 1 第一次 missing → retry 成功
    processes.first_attempt_missing.lock().unwrap().insert(1);
    let cancel = FakeCancel::new();
    let mut config = base_config();
    config.candidate_lps = vec![1];

    let mut ctx = build_ctx(
        &root,
        backend.clone() as Arc<dyn GpuBackend>,
        processes.clone() as Arc<dyn ProcessRunner>,
        Arc::new(cancel) as Arc<dyn CancelSignal>,
        Arc::new(NoopSleeper) as Arc<dyn Sleep>,
        config,
        &journal,
        None,
    );
    let result = run_benchmark(&mut ctx);
    assert_eq!(result.status, SessionStatus::Completed);

    // 所有 spawned PIDs（workload + PresentMon）都必須被 kill
    let spawned: Vec<u32> = processes.spawn_log().iter().map(|(_, p, _)| *p).collect();
    let killed = processes.killed_log();
    for pid in &spawned {
        assert!(
            killed.contains(pid),
            "spawned PID {pid} 必須出現在 killed log"
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// retry 產生兩個 attempt 的診斷檔，且 attempt 欄位正確
#[test]
fn retry_writes_per_attempt_diagnostics() {
    let root = temp_root("diag_retry");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    backend.set_policy(AffinityPolicy {
        instance_id: GPU_A.to_string(),
        device_policy: RegistryValueSnapshot::dword(4),
        assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
    });
    let processes = Arc::new(FakeProcessRunner::new());
    processes
        .presentmon_csv
        .lock()
        .unwrap()
        .push_str(&csv_for_lp(0));
    // LP 1 第一次 missing → retry 成功
    processes.first_attempt_missing.lock().unwrap().insert(1);
    let cancel = FakeCancel::new();
    let mut config = base_config();
    config.candidate_lps = vec![1];

    let mut ctx = build_ctx(
        &root,
        backend.clone() as Arc<dyn GpuBackend>,
        processes.clone() as Arc<dyn ProcessRunner>,
        Arc::new(cancel) as Arc<dyn CancelSignal>,
        Arc::new(NoopSleeper) as Arc<dyn Sleep>,
        config,
        &journal,
        None,
    );
    let result = run_benchmark(&mut ctx);
    assert_eq!(result.status, SessionStatus::Completed);

    let session_dir = ctx.storage_root.join(&ctx.session_id);
    let diag_dir = session_dir.join("diag");

    // attempt 1 診斷檔
    let d1 = read_diag(&session_dir, 0, 1);
    assert_eq!(d1.attempt, 1);
    assert_eq!(d1.error.as_deref(), Some(codes::BENCHMARK_CAPTURE_MISSING));

    // attempt 2 診斷檔（獨立檔案）
    let d2_path = diag_dir.join("capture-round-0-lp-1-attempt-2.json");
    assert!(d2_path.exists(), "retry 診斷檔必須存在: {d2_path:?}");
    let d2_text =
        std::fs::read_to_string(&d2_path).unwrap_or_else(|e| panic!("讀取 retry 診斷檔失敗: {e}"));
    let d2: CaptureDiagnostics = serde_json::from_str(&d2_text).expect("retry 診斷 JSON 解析失敗");
    assert_eq!(d2.attempt, 2);
    assert_eq!(d2.error, None, "retry 成功 error 應為 None");
    assert_ne!(
        d2.workload_pid, d1.workload_pid,
        "retry 必須用新 workload PID"
    );
    assert!(
        d2.workload_pid != 0 && d1.workload_pid != 0,
        "兩次 attempt 都應有有效 workload PID"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// 部分失敗路徑的 summary.sample_count 反映已完成的 LP 結果
#[test]
fn partial_failure_sample_count_reflects_completed_lps() {
    let root = temp_root("partial_samples");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    backend.set_policy(AffinityPolicy {
        instance_id: GPU_A.to_string(),
        device_policy: RegistryValueSnapshot::dword(4),
        assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
    });
    let processes = Arc::new(FakeProcessRunner::new());
    // 所有 LP 共用一個有效 CSV（避免 csv_for_lp header 重複）
    processes
        .presentmon_csv
        .lock()
        .unwrap()
        .push_str(&csv_for_lp(0));
    // 中間的 LP 2 第一次與所有 retry 都 missing；LP 3 仍應繼續完成
    processes.first_attempt_missing.lock().unwrap().insert(2);
    processes
        .second_attempt_also_missing
        .lock()
        .unwrap()
        .insert(2);
    let cancel = FakeCancel::new();
    let mut config = base_config();
    config.candidate_lps = vec![1, 2, 3];

    let mut ctx = build_ctx(
        &root,
        backend.clone() as Arc<dyn GpuBackend>,
        processes.clone() as Arc<dyn ProcessRunner>,
        Arc::new(cancel) as Arc<dyn CancelSignal>,
        Arc::new(NoopSleeper) as Arc<dyn Sleep>,
        config,
        &journal,
        None,
    );
    let result = run_benchmark(&mut ctx);

    assert_eq!(result.status, SessionStatus::Failed);
    // LP1、LP3 各有 sample_count，證明中間 LP 失敗不會中止 session。
    assert_eq!(result.detail.results.len(), 2, "應保留 LP1, LP3 結果");
    assert_eq!(
        result
            .detail
            .results
            .iter()
            .map(|r| r.lp)
            .collect::<Vec<_>>(),
        vec![1, 3]
    );
    let total_samples: u32 = result.detail.results.iter().map(|r| r.sample_count).sum();
    assert_eq!(total_samples, 100, "短篩成功的 LP1、LP3 各保留 50 samples");
    assert_eq!(
        result.detail.summary.sample_count, total_samples,
        "summary.sample_count 必須等於已完成 LP 的 sample_count 總和"
    );
    let _ = std::fs::remove_dir_all(&root);
}

// ── 自適應排程（全 LP 短篩 + Top5 racing + Top3 refinement + 確認）測試 ──

/// 由 PresentMon spawn log 統計每 LP 實際被 capture 的 round 集合。
fn rounds_per_lp(processes: &FakeProcessRunner) -> HashMap<u32, Vec<u32>> {
    let mut map: HashMap<u32, Vec<u32>> = HashMap::new();
    for (name, _pid, args) in processes.spawn_log().iter() {
        if !name.contains("PresentMon") {
            continue;
        }
        let out = args
            .iter()
            .position(|a| a == "--output_file")
            .and_then(|i| args.get(i + 1))
            .cloned();
        let parsed = out.as_deref().and_then(|p| {
            let stem = std::path::Path::new(p).file_stem()?.to_str()?;
            let (head, lp_str) = stem.rsplit_once("-lp-")?;
            let lp: u32 = lp_str.parse().ok()?;
            let round: u32 = head.rsplit_once("round-")?.1.parse().ok()?;
            Some((round, lp))
        });
        if let Some((round, lp)) = parsed {
            map.entry(lp).or_default().push(round);
        }
    }
    for v in map.values_mut() {
        v.sort_unstable();
        v.dedup();
    }
    map
}

/// N=3：同內容 → 平手 → Equivalent 於 5 確認 round 提早停；精確
/// 3*3 + 2*3 + 2*5 = 25 次 capture，只有前 2 名 finalists 進確認 round（base..）。
#[test]
fn adaptive_run_exact_min_captures_and_only_top_two_confirmed() {
    let root = temp_root("adaptive_counts");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    let processes = Arc::new(FakeProcessRunner::new());
    processes
        .presentmon_csv
        .lock()
        .unwrap()
        .push_str(&csv_for_lp(0));
    let cancel = FakeCancel::new();
    let config = base_config(); // candidate_lps [1,2,3]

    let mut ctx = build_ctx(
        &root,
        backend as Arc<dyn GpuBackend>,
        processes.clone() as Arc<dyn ProcessRunner>,
        Arc::new(cancel) as Arc<dyn CancelSignal>,
        Arc::new(NoopSleeper) as Arc<dyn Sleep>,
        config,
        &journal,
        None,
    );
    let result = run_benchmark(&mut ctx);
    assert_eq!(
        result.status,
        SessionStatus::Completed,
        "err={:?}",
        result.error
    );

    let wl_spawns = processes
        .spawn_log()
        .iter()
        .filter(|(n, _, _)| !n.contains("PresentMon"))
        .count();
    assert_eq!(wl_spawns, 19, "N=3 應為 3 + 3 + 3 + 2*5 = 19 次 capture");

    // 同內容 CSV → 平手 → finalists = [1, 2]；確認 5 round 即停（Equivalent）。
    let b = CONFIRMATION_ROUND_BASE;
    let rl = rounds_per_lp(&processes);
    assert_eq!(rl[&1], vec![0, 1, 2, b, b + 1, b + 2, b + 3, b + 4]);
    assert_eq!(rl[&2], vec![0, 1, 2, b, b + 1, b + 2, b + 3, b + 4]);
    assert_eq!(rl[&3], vec![0, 1, 2]);
    assert_eq!(
        result.detail.summary.reliability.confirmation_rounds,
        EQUIVALENT_MIN_ROUNDS
    );
    assert_eq!(
        result.detail.summary.reliability.status,
        ReliabilityStatus::Equivalent
    );
    assert_eq!(result.best_lp, None, "Equivalent 不得有推薦");
    assert_eq!(result.detail.summary.verified_best_lp, None);
    assert_eq!(result.detail.summary.confirmation_winner_lp, None);
    assert_eq!(result.detail.summary.equivalent_finalist_lps, vec![1, 2]);
    let _ = std::fs::remove_dir_all(&root);
}

/// 候選略優（~0.78% avg）但不足 decisive、又超等效門檻 → 跑到 7 輪仍 Inconclusive。
#[test]
fn adaptive_run_seven_round_inconclusive_when_narrow_margin() {
    let root = temp_root("inconclusive7");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    let processes = Arc::new(FakeProcessRunner::new());
    // LP1=10.0（最快）、LP2=10.08（次）、LP3=12.0（淘汰）。
    processes
        .presentmon_csv_by_lp
        .lock()
        .unwrap()
        .insert(1, csv_with_base(10.0));
    processes
        .presentmon_csv_by_lp
        .lock()
        .unwrap()
        .insert(2, csv_with_base(10.08));
    processes
        .presentmon_csv_by_lp
        .lock()
        .unwrap()
        .insert(3, csv_with_base(12.0));
    let cancel = FakeCancel::new();
    let config = base_config(); // candidate_lps [1,2,3]

    let mut ctx = build_ctx(
        &root,
        backend as Arc<dyn GpuBackend>,
        processes.clone() as Arc<dyn ProcessRunner>,
        Arc::new(cancel) as Arc<dyn CancelSignal>,
        Arc::new(NoopSleeper) as Arc<dyn Sleep>,
        config,
        &journal,
        None,
    );
    let result = run_benchmark(&mut ctx);
    assert_eq!(
        result.status,
        SessionStatus::Completed,
        "err={:?}",
        result.error
    );
    assert_eq!(result.best_lp, None, "非 decisive 不得有推薦");
    assert_eq!(
        result.detail.summary.reliability.status,
        ReliabilityStatus::Inconclusive
    );
    assert_eq!(
        result.detail.summary.reliability.confirmation_rounds,
        CONFIRMATION_MAX_ROUNDS
    );
    // finalists [1,2] 各跑滿 7 確認 round；LP3 只測三層 selection。
    let b = CONFIRMATION_ROUND_BASE;
    let rl = rounds_per_lp(&processes);
    assert_eq!(
        rl[&1],
        vec![0, 1, 2, b, b + 1, b + 2, b + 3, b + 4, b + 5, b + 6]
    );
    assert_eq!(
        rl[&2],
        vec![0, 1, 2, b, b + 1, b + 2, b + 3, b + 4, b + 5, b + 6]
    );
    assert_eq!(rl[&3], vec![0, 1, 2]);
    let _ = std::fs::remove_dir_all(&root);
}

/// 篩選平手：同分數時依中位數 → worst-round → 較小 LP 決定，取前 2。
#[test]
fn select_finalists_deterministic_tie_picks_lower_lps() {
    let dir = temp_root("sel_tie");
    let mut round_csvs: RoundCsvs = HashMap::new();
    for lp in 0..3u32 {
        for round in 0..3u32 {
            round_csvs
                .entry(lp)
                .or_default()
                .insert(round, write_round_csv(&dir, round, lp, 10.0));
        }
    }
    let finalists = select_top_candidates(&round_csvs, SCREENING_ROUNDS, MAX_FINALISTS);
    assert_eq!(finalists, vec![0, 1]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 篩選少於兩個完整候選 → 回傳空（呼叫端跳過確認）。
#[test]
fn select_finalists_empty_when_fewer_than_two_complete() {
    let dir = temp_root("sel_few");
    let mut round_csvs: RoundCsvs = HashMap::new();
    // LP0 有三個完整 selection round。
    for round in 0..(SCREENING_ROUNDS + REFINEMENT_ROUNDS) {
        round_csvs
            .entry(0)
            .or_default()
            .insert(round, write_round_csv(&dir, round, 0, 10.0));
    }
    // LP1 只測 1 個篩選 round → 非完整候選
    round_csvs
        .entry(1)
        .or_default()
        .insert(0, write_round_csv(&dir, 0, 1, 11.0));
    assert!(select_top_candidates(
        &round_csvs,
        SCREENING_ROUNDS + REFINEMENT_ROUNDS,
        MAX_FINALISTS
    )
    .is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

/// 非 finalist（僅篩選 round）不得出現在確認勝者中；best 由確認推論的 finalists 決定。
#[test]
fn non_finalist_does_not_become_confirmation_winner() {
    let dir = temp_root("rel_adaptive");
    let mut round_csvs: RoundCsvs = HashMap::new();
    // finalists 0/1 各 5 確認 round（CONFIRMATION_ROUND_BASE..+5）
    for round in CONFIRMATION_ROUND_BASE..(CONFIRMATION_ROUND_BASE + 5) {
        round_csvs
            .entry(0)
            .or_default()
            .insert(round, write_round_csv(&dir, round, 0, 10.0));
        round_csvs
            .entry(1)
            .or_default()
            .insert(round, write_round_csv(&dir, round, 1, 11.0));
    }
    // 非 finalist LP2 只有篩選 round（8ms 最快），不得影響確認推論
    for round in 0..SCREENING_ROUNDS {
        round_csvs
            .entry(2)
            .or_default()
            .insert(round, write_round_csv(&dir, round, 2, 8.0));
    }
    let results = vec![
        lp_res(0, 100.0, 100.0, 100.0, 0.0),
        lp_res(1, 90.909, 90.909, 90.909, 0.0),
        lp_res(2, 125.0, 125.0, 125.0, 0.0),
    ];
    let rel = compute_reliability(
        &round_csvs,
        &results,
        &[0, 1],
        5,
        Some(ForwardVerdict::CandidatePassed),
        false,
        false,
        0,
    );
    assert_eq!(rel.status, ReliabilityStatus::Passed);
    assert_eq!(rel.candidate_lp, Some(0));
    assert_eq!(rel.runner_up_lp, Some(1));
    assert!(
        !rel.per_round_winners.contains(&Some(2)),
        "非 finalist 不得成為任何確認 round 勝者"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// N=1：只測 3 篩選 round 即停（無確認），session Completed 但可靠性
/// Inconclusive（少於兩個 finalists，無法 Passed/Equivalent）。
#[test]
fn single_lp_skips_confirmation_and_stays_inconclusive() {
    let root = temp_root("n1");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    let processes = Arc::new(FakeProcessRunner::new());
    processes
        .presentmon_csv
        .lock()
        .unwrap()
        .push_str(&csv_for_lp(0));
    let cancel = FakeCancel::new();
    let mut config = base_config();
    config.candidate_lps = vec![1];

    let mut ctx = build_ctx(
        &root,
        backend as Arc<dyn GpuBackend>,
        processes.clone() as Arc<dyn ProcessRunner>,
        Arc::new(cancel) as Arc<dyn CancelSignal>,
        Arc::new(NoopSleeper) as Arc<dyn Sleep>,
        config,
        &journal,
        None,
    );
    let result = run_benchmark(&mut ctx);
    assert_eq!(
        result.status,
        SessionStatus::Completed,
        "err={:?}",
        result.error
    );
    assert_eq!(result.best_lp, None, "單一 LP 不得有推薦");
    assert_eq!(
        result.detail.summary.reliability.status,
        ReliabilityStatus::Inconclusive
    );
    let wl_spawns = processes
        .spawn_log()
        .iter()
        .filter(|(n, _, _)| !n.contains("PresentMon"))
        .count();
    assert_eq!(wl_spawns, 1, "N=1 只需一輪短篩，無比較或確認");
    assert_eq!(rounds_per_lp(&processes)[&1], vec![0]);
    let _ = std::fs::remove_dir_all(&root);
}

/// 進度在兩階段過渡時單調遞增、最終達 100 且不超過 100。
#[test]
fn progress_monotonic_and_capped_at_100() {
    let root = temp_root("progress_adaptive");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    let processes = Arc::new(FakeProcessRunner::new());
    processes
        .presentmon_csv
        .lock()
        .unwrap()
        .push_str(&csv_for_lp(0));
    let cancel = FakeCancel::new();
    let events: Arc<std::sync::Mutex<Vec<BenchmarkProgress>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let ev = events.clone();

    let mut ctx = build_ctx(
        &root,
        backend as Arc<dyn GpuBackend>,
        processes as Arc<dyn ProcessRunner>,
        Arc::new(cancel) as Arc<dyn CancelSignal>,
        Arc::new(NoopSleeper) as Arc<dyn Sleep>,
        base_config(),
        &journal,
        Some(Box::new(move |p| ev.lock().unwrap().push(p.clone()))),
    );
    run_benchmark(&mut ctx);
    let pcts: Vec<u32> = events
        .lock()
        .unwrap()
        .iter()
        .map(|p| p.percentage)
        .collect();
    assert!(!pcts.is_empty());
    assert!(pcts.iter().all(|&p| p <= 100), "進度不得超過 100");
    assert_eq!(*pcts.last().unwrap(), 100, "最終進度應達 100");
    for w in pcts.windows(2) {
        assert!(w[0] <= w[1], "進度不得倒退: {pcts:?}");
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// 確認階段（round 3 起）收到取消 → Cancelled 並還原。
#[test]
fn cancel_during_confirmation_aborts_and_restores() {
    let root = temp_root("cancel_confirm");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    let baseline = AffinityPolicy {
        instance_id: GPU_A.to_string(),
        device_policy: RegistryValueSnapshot::dword(4),
        assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
    };
    backend.set_policy(baseline.clone());
    let processes = Arc::new(FakeProcessRunner::new());
    processes
        .presentmon_csv
        .lock()
        .unwrap()
        .push_str(&csv_for_lp(0));
    let cancel = Arc::new(FakeCancel::new());
    let cancel2 = cancel.clone();
    let config = base_config(); // candidate_lps [1,2,3]

    let mut ctx = build_ctx(
        &root,
        backend.clone() as Arc<dyn GpuBackend>,
        processes.clone() as Arc<dyn ProcessRunner>,
        cancel as Arc<dyn CancelSignal>,
        Arc::new(NoopSleeper) as Arc<dyn Sleep>,
        config,
        &journal,
        Some(Box::new(move |p| {
            // 進入確認階段（round 3）即觸發取消
            if p.round == Some(SCREENING_ROUNDS) {
                cancel2.set(true);
            }
        })),
    );
    let result = run_benchmark(&mut ctx);
    assert_eq!(result.status, SessionStatus::Cancelled);
    assert_eq!(backend.current_policy(GPU_A), baseline, "必須還原原始策略");
    assert!(!journal.exists());
    let _ = std::fs::remove_dir_all(&root);
}

/// 確認階段某 finalist 的 PresentMon spawn 失敗 → Failed 且還原。
#[test]
fn error_during_confirmation_fails_and_restores() {
    let root = temp_root("err_confirm");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    let baseline = AffinityPolicy {
        instance_id: GPU_A.to_string(),
        device_policy: RegistryValueSnapshot::dword(4),
        assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
    };
    backend.set_policy(baseline.clone());
    let processes = Arc::new(FakeProcessRunner::new());
    processes
        .presentmon_csv
        .lock()
        .unwrap()
        .push_str(&csv_for_lp(0));
    // 第一個 confirmation round 的 PresentMon spawn 失敗。
    processes
        .fail_presentmon_rounds
        .lock()
        .unwrap()
        .insert(CONFIRMATION_ROUND_BASE);
    let cancel = FakeCancel::new();
    let config = base_config(); // candidate_lps [1,2,3]

    let mut ctx = build_ctx(
        &root,
        backend.clone() as Arc<dyn GpuBackend>,
        processes.clone() as Arc<dyn ProcessRunner>,
        Arc::new(cancel) as Arc<dyn CancelSignal>,
        Arc::new(NoopSleeper) as Arc<dyn Sleep>,
        config,
        &journal,
        None,
    );
    let result = run_benchmark(&mut ctx);
    assert_eq!(result.status, SessionStatus::Failed);
    assert_eq!(
        result.error.as_deref(),
        Some(codes::BENCHMARK_PRESENTMON_FAILED)
    );
    assert_eq!(result.best_lp, None, "失敗不該有推薦");
    assert_eq!(
        result.detail.results.len(),
        3,
        "三層 selection 的部分結果應保留"
    );
    assert_eq!(backend.current_policy(GPU_A), baseline, "必須還原原始策略");
    assert!(!journal.exists());
    // 確認階段確實被觸及。
    let reached_confirmation = processes.spawn_log().iter().any(|(n, _, args)| {
        n.contains("PresentMon")
            && args
                .iter()
                .any(|a| a.contains(&format!("round-{}-lp-", CONFIRMATION_ROUND_BASE)))
    });
    assert!(
        reached_confirmation,
        "應在第一個 confirmation round 觸發失敗"
    );
    let _ = std::fs::remove_dir_all(&root);
}

// ── 等效安全驗證（run_equivalent_validation）──

fn equivalent_validation_baseline(reference_lp: u32) -> AffinityPolicy {
    AffinityPolicy {
        instance_id: GPU_A.to_string(),
        device_policy: RegistryValueSnapshot::dword(4),
        assignment_set_override: RegistryValueSnapshot::binary(single_lp_mask_bytes(reference_lp)),
    }
}

#[test]
fn run_equivalent_validation_passes_when_selected_not_worse() {
    let root = temp_root("equiv_val_pass");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    let baseline = equivalent_validation_baseline(2);
    backend.set_policy(baseline.clone());
    let processes = Arc::new(FakeProcessRunner::new());
    // selected(1) 與 reference(2) 內容相同 → selected 不更差 → Passed。
    processes
        .presentmon_csv_by_lp
        .lock()
        .unwrap()
        .insert(1, csv_with_base(10.0));
    processes
        .presentmon_csv_by_lp
        .lock()
        .unwrap()
        .insert(2, csv_with_base(10.0));
    let cancel = FakeCancel::new();

    let mut ctx = build_ctx(
        &root,
        backend.clone() as Arc<dyn GpuBackend>,
        processes.clone() as Arc<dyn ProcessRunner>,
        Arc::new(cancel) as Arc<dyn CancelSignal>,
        Arc::new(NoopSleeper) as Arc<dyn Sleep>,
        base_config(),
        &journal,
        None,
    );
    let outcome = run_equivalent_validation(&mut ctx, 1, 2, 240, 8192);
    assert_eq!(
        outcome.status,
        EquivalentSafetyStatus::Passed,
        "reason={:?}",
        outcome.reason
    );
    assert_eq!(outcome.rounds, EQUIVALENT_VALIDATION_ROUNDS);
    assert!(outcome.avg_improvement_pct.is_some());
    assert_eq!(
        backend.current_policy(GPU_A),
        baseline,
        "必須還原 reference policy"
    );
    assert!(!journal.exists());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn run_equivalent_validation_fails_when_selected_materially_worse() {
    let root = temp_root("equiv_val_fail");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    let baseline = equivalent_validation_baseline(2);
    backend.set_policy(baseline.clone());
    let processes = Arc::new(FakeProcessRunner::new());
    // selected(1) 12ms 明顯慢於 reference(2) 10ms → Failed。
    processes
        .presentmon_csv_by_lp
        .lock()
        .unwrap()
        .insert(1, csv_with_base(12.0));
    processes
        .presentmon_csv_by_lp
        .lock()
        .unwrap()
        .insert(2, csv_with_base(10.0));
    let cancel = FakeCancel::new();

    let mut ctx = build_ctx(
        &root,
        backend.clone() as Arc<dyn GpuBackend>,
        processes.clone() as Arc<dyn ProcessRunner>,
        Arc::new(cancel) as Arc<dyn CancelSignal>,
        Arc::new(NoopSleeper) as Arc<dyn Sleep>,
        base_config(),
        &journal,
        None,
    );
    let outcome = run_equivalent_validation(&mut ctx, 1, 2, 240, 8192);
    assert_eq!(outcome.status, EquivalentSafetyStatus::Failed);
    assert_eq!(backend.current_policy(GPU_A), baseline, "失敗仍須還原");
    assert!(!journal.exists());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn run_equivalent_validation_cancelled_and_restores() {
    let root = temp_root("equiv_val_cancel");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    let baseline = equivalent_validation_baseline(2);
    backend.set_policy(baseline.clone());
    let processes = Arc::new(FakeProcessRunner::new());
    processes
        .presentmon_csv_by_lp
        .lock()
        .unwrap()
        .insert(1, csv_with_base(10.0));
    processes
        .presentmon_csv_by_lp
        .lock()
        .unwrap()
        .insert(2, csv_with_base(10.0));
    let cancel = Arc::new(FakeCancel::new());
    let cancel2 = cancel.clone();

    let mut ctx = build_ctx(
        &root,
        backend.clone() as Arc<dyn GpuBackend>,
        processes.clone() as Arc<dyn ProcessRunner>,
        cancel as Arc<dyn CancelSignal>,
        Arc::new(NoopSleeper) as Arc<dyn Sleep>,
        base_config(),
        &journal,
        Some(Box::new(move |p| {
            // 進入第一個驗證 round 的 applying 即觸發取消。
            if p.round == Some(EQUIVALENT_VALIDATION_ROUND_BASE) && p.stage == "applying" {
                cancel2.set(true);
            }
        })),
    );
    let outcome = run_equivalent_validation(&mut ctx, 1, 2, 240, 8192);
    assert_eq!(outcome.status, EquivalentSafetyStatus::Cancelled);
    assert_eq!(
        backend.current_policy(GPU_A),
        baseline,
        "取消必須還原 reference policy"
    );
    assert!(!journal.exists());
    let _ = std::fs::remove_dir_all(&root);
}

/// 單向退步禁制：單輪 selected 明顯「改善」不得誤拒（反向前向 Equivalent 的雙向 abs）。
#[test]
fn equivalent_validation_regressed_improvement_not_rejected() {
    let selected = lp_raw(1, 110.0, 95.0, 85.0, 10.0, 1.0); // avg +10%、p1 較佳
    let reference = lp_raw(2, 100.0, 90.0, 80.0, 10.0, 1.0);
    assert!(!equivalent_validation_regressed(&[(selected, reference)]));
    // spike 單輪明顯「更好」（下降 >0.5pp）也不得拒
    let better_spike = lp_raw(1, 100.0, 90.0, 80.0, 10.0, 0.2);
    assert!(!equivalent_validation_regressed(&[(
        better_spike,
        lp_raw(2, 100.0, 90.0, 80.0, 10.0, 1.0)
    )]));
}

/// 單向退步禁制：avg / p1 / spike 任一單輪明顯退步 → 拒絕。
#[test]
fn equivalent_validation_regressed_rejects_material_regression() {
    let reference = lp_raw(2, 100.0, 90.0, 80.0, 10.0, 1.0);
    // avg 96 vs 100 → -4% < -3% → 退步
    assert!(equivalent_validation_regressed(&[(
        lp_raw(1, 96.0, 90.0, 80.0, 10.0, 1.0),
        reference.clone()
    )]));
    // p1 84 vs 90 → (84-90)/90 = -6.67% < -5% → 退步
    assert!(equivalent_validation_regressed(&[(
        lp_raw(1, 100.0, 84.0, 80.0, 10.0, 1.0),
        reference.clone()
    )]));
    // spike 1.7 vs 1.0 → +0.7pp > 0.5pp → 退步
    assert!(equivalent_validation_regressed(&[(
        lp_raw(1, 100.0, 90.0, 80.0, 10.0, 1.7),
        reference
    )]));
}

/// 視窗完整性持續異常（warmup 期間）→ session Failed（BENCHMARK_WINDOW_INTEGRITY）且策略還原。
#[test]
fn window_integrity_failure_fails_session_and_restores() {
    let root = temp_root("win_integrity");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    let baseline = AffinityPolicy {
        instance_id: GPU_A.to_string(),
        device_policy: RegistryValueSnapshot::dword(4),
        assignment_set_override: RegistryValueSnapshot::binary(vec![0x08]),
    };
    backend.set_policy(baseline.clone());
    let processes = Arc::new(FakeProcessRunner::new());
    processes
        .presentmon_csv
        .lock()
        .unwrap()
        .push_str(&csv_for_lp(0));
    let cancel = FakeCancel::new();
    let window = Arc::new(fake::FakeWindow::new());
    window.set_integrity_ok(false); // 前景/位置持續異常
    let mut config = base_config();
    config.candidate_lps = vec![1];

    let mut ctx = build_ctx(
        &root,
        backend.clone() as Arc<dyn GpuBackend>,
        processes.clone() as Arc<dyn ProcessRunner>,
        Arc::new(cancel) as Arc<dyn CancelSignal>,
        Arc::new(NoopSleeper) as Arc<dyn Sleep>,
        config,
        &journal,
        None,
    );
    ctx.window = window.clone();
    let result = run_benchmark(&mut ctx);

    assert_eq!(result.status, SessionStatus::Failed);
    assert_eq!(
        result.error.as_deref(),
        Some(codes::BENCHMARK_WINDOW_INTEGRITY)
    );
    // cleanup_run 還原原始策略、清除日誌
    assert_eq!(backend.current_policy(GPU_A), baseline);
    assert!(!journal.exists(), "還原成功後日誌應清除");
    let _ = std::fs::remove_dir_all(&root);
}

/// run_capture 期間視窗完整性破壞 → 回 BENCHMARK_WINDOW_INTEGRITY 且計 window_invalid。
#[test]
fn run_capture_window_integrity_fails_and_counts() {
    let root = temp_root("run_cap_win");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    let processes = Arc::new(FakeProcessRunner::new());
    processes
        .presentmon_csv
        .lock()
        .unwrap()
        .push_str(&csv_for_lp(0));
    let cancel = FakeCancel::new();
    let window = Arc::new(fake::FakeWindow::new());
    window.set_integrity_ok(false);
    let mut ctx = build_ctx(
        &root,
        backend.clone() as Arc<dyn GpuBackend>,
        processes.clone() as Arc<dyn ProcessRunner>,
        Arc::new(cancel) as Arc<dyn CancelSignal>,
        Arc::new(NoopSleeper) as Arc<dyn Sleep>,
        base_config(),
        &journal,
        None,
    );
    ctx.window = window.clone();
    let csv = root.join("out.csv");
    let expected = Rect::new(0, 0, 1280, 720);
    let err = run_capture(&mut ctx, 0, 1, 999, &csv, 1, 0, 8192, 1, expected).unwrap_err();
    assert_eq!(err, codes::BENCHMARK_WINDOW_INTEGRITY);
    assert_eq!(ctx.capture_quality.window_invalid_captures, 1);
    let _ = std::fs::remove_dir_all(&root);
}

/// run_capture 等待期間 workload 失去前景 → 要求置中還原（與 report_integrity 路徑一致）。
#[test]
fn run_capture_foreground_loss_requests_center_restore() {
    let root = temp_root("run_cap_center");
    let journal = root.join("journal.json");
    let (mut ctx, wc) = ctx_with_window_control(&root, &journal);
    let window = Arc::new(fake::FakeWindow::new());
    window.set_integrity_ok(false); // 前景失敗（foreground=false）
    ctx.window = window.clone();
    let csv = root.join("out.csv");
    let expected = Rect::new(0, 0, 1280, 720);
    let err = run_capture(&mut ctx, 0, 1, 999, &csv, 1, 0, 8192, 1, expected).unwrap_err();
    assert_eq!(err, codes::BENCHMARK_WINDOW_INTEGRITY);
    assert!(wc.center_requested.load(Ordering::SeqCst));
    let _ = std::fs::remove_dir_all(&root);
}

/// run_capture 等待期間僅 position/topmost 等失敗（foreground 仍 true）→ 不要求置中。
#[test]
fn run_capture_non_foreground_failure_does_not_request_center() {
    let root = temp_root("run_cap_nocenter");
    let journal = root.join("journal.json");
    let (mut ctx, wc) = ctx_with_window_control(&root, &journal);
    let window = Arc::new(fake::FakeWindow::new());
    window.set_integrity_snapshot(WindowIntegritySnapshot {
        foreground: true,
        position_ok: false,
        ..Default::default()
    });
    ctx.window = window.clone();
    let csv = root.join("out.csv");
    let expected = Rect::new(0, 0, 1280, 720);
    let err = run_capture(&mut ctx, 0, 1, 999, &csv, 1, 0, 8192, 1, expected).unwrap_err();
    assert_eq!(err, codes::BENCHMARK_WINDOW_INTEGRITY);
    assert!(!wc.center_requested.load(Ordering::SeqCst));
    let _ = std::fs::remove_dir_all(&root);
}
#[test]
fn quick_physical_captures_and_authenticated_apply_use_whole_cores() {
    use crate::benchmark::{
        manager::{core_apply::tested_target, BenchmarkManager},
        physical,
    };
    let root = temp_root("quick_physical_full");
    let journal = root.join("journal.json");
    let backend = Arc::new(FakeBackend::new(vec![device(GPU_A)]));
    let baseline = AffinityPolicy {
        instance_id: GPU_A.into(),
        device_policy: RegistryValueSnapshot::dword(4),
        assignment_set_override: RegistryValueSnapshot::binary(vec![0x04, 0]),
    };
    backend.set_policy(baseline.clone());
    let processes = Arc::new(FakeProcessRunner::new());
    for lp in [0, 2, 4] {
        processes
            .presentmon_csv_by_lp
            .lock()
            .unwrap()
            .insert(lp, csv_for_lp(lp));
    }
    let mut config = base_config();
    config.method_version = physical::METHOD_VERSION;
    config.candidate_lps.clear();
    config.sample_secs = 10;
    config.warm_up_secs = 3;
    let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = observed.clone();
    let gpu = backend.clone();
    let mut ctx = build_ctx(
        &root,
        backend.clone(),
        processes.clone(),
        Arc::new(FakeCancel::new()),
        Arc::new(NoopSleeper),
        config,
        &journal,
        Some(Box::new(move |p| {
            if p.stage == "collecting" {
                seen.lock()
                    .unwrap()
                    .push((p.target.clone(), gpu.current_policy(GPU_A)));
            }
        })),
    );
    ctx.topo = build_topology(vec![
        (vec![0, 1], 0, true),
        (vec![2, 3], 0, true),
        (vec![4, 63], 0, true),
    ]);
    let result = run_benchmark(&mut ctx);
    assert_eq!(
        result.status,
        SessionStatus::Completed,
        "{:?}",
        result.error
    );
    let q = result.detail.summary.quick.as_ref().unwrap();
    assert_eq!(q.screening.len(), 3);
    assert_eq!(q.retest.len(), 2);
    assert_eq!(result.detail.results.len(), 2);
    assert_eq!(result.detail.confirmation_results.len(), 0);
    assert_eq!(result.detail.summary.best_lp, None);
    assert_eq!(result.detail.summary.capture_quality.total_captures, 7); // 2 基線 + 5 候選
    assert_eq!(backend.restart_count(), 6); // 5 候選 captures + restoration（基線不重啟）
    assert_eq!(q.screening_order, physical::shuffled(&[0, 1, 2], q.seed));
    let expected: Vec<_> = q
        .screening_order
        .iter()
        .rev()
        .filter(|id| q.screening.iter().take(2).any(|r| r.target.core_id == **id))
        .copied()
        .collect();
    assert_eq!(q.retest_order, expected);
    assert_eq!(backend.current_policy(GPU_A), baseline);
    assert!(!journal.exists());
    for (target, policy) in observed.lock().unwrap().iter() {
        let target = target.as_ref().expect("core target on progress");
        assert_eq!(
            policy.assignment_set_override.bytes,
            Some(physical::mask(target, &ctx.topo).unwrap())
        );
    }
    let detail = storage::get_at_verified(&ctx.storage_root, &ctx.session_id).unwrap();
    let selected = q.retest[1].target.core_id; // runner-up is also selectable
    assert!(tested_target(&detail, &ctx.topo, &ctx.cpu_identity, selected).is_ok());
    let manager = BenchmarkManager::new(backend.clone());
    let restore = root.join("restore.json");
    manager
        .apply_core_at(
            &ctx.topo,
            &ctx.cpu_identity,
            GPU_A,
            selected,
            Some(&ctx.session_id),
            &ctx.storage_root,
            &journal,
            &restore,
        )
        .unwrap();
    assert_eq!(
        backend.current_policy(GPU_A).assignment_set_override.bytes,
        Some(physical::mask(&q.retest[1].target, &ctx.topo).unwrap())
    );
    crate::benchmark::manager::restore_previous_affinity(backend.as_ref(), &NoopSleeper, &restore)
        .unwrap();
    assert_eq!(backend.current_policy(GPU_A), baseline);
    let mut changed = ctx.topo.clone();
    changed.physical_cores[0].lp_indices = vec![0];
    assert!(tested_target(&detail, &changed, &ctx.cpu_identity, selected).is_err());
    let mut legacy = detail.clone();
    legacy.summary.quick = None;
    assert!(tested_target(&legacy, &ctx.topo, &ctx.cpu_identity, selected).is_err());
    for status in [SessionStatus::Failed, SessionStatus::Cancelled] {
        let mut invalid = detail.clone();
        invalid.summary.status = status;
        assert!(tested_target(&invalid, &ctx.topo, &ctx.cpu_identity, selected).is_err());
    }
    let file = ctx.storage_root.join(&ctx.session_id).join("session.json");
    let text = std::fs::read_to_string(&file).unwrap();
    std::fs::write(&file, text.replace("Completed", "Failed")).unwrap();
    assert!(manager
        .apply_core_at(
            &ctx.topo,
            &ctx.cpu_identity,
            GPU_A,
            selected,
            Some(&ctx.session_id),
            &ctx.storage_root,
            &journal,
            &restore
        )
        .is_err());
    assert_eq!(backend.current_policy(GPU_A), baseline);
}

#[test]
fn quick_single_cancel_and_capture_failure_restore() {
    use crate::benchmark::physical::{RankingStatus, METHOD_VERSION};
    for mode in [
        "single",
        "cancel",
        "capture",
        "restore",
        "write",
        "readback",
        "restart",
        "gpu_missing",
    ] {
        let root = temp_root(&format!("quick_{mode}"));
        let journal = root.join("journal.json");
        let backend = Arc::new(FakeBackend::new(if mode == "gpu_missing" {
            vec![]
        } else {
            vec![device(GPU_A)]
        }));
        let baseline = AffinityPolicy {
            instance_id: GPU_A.into(),
            ..Default::default()
        };
        let processes = Arc::new(FakeProcessRunner::new());
        *processes.presentmon_csv.lock().unwrap() = csv_for_lp(0);
        if mode == "capture" {
            processes.fail_presentmon_rounds.lock().unwrap().insert(1);
        }
        if mode == "restore" {
            backend.disable_fails.store(true, Ordering::SeqCst);
        }
        if mode == "write" {
            backend.fail_next_write();
        }
        if mode == "readback" {
            backend.fail_nth_read_mismatch(2);
        }
        if mode == "restart" {
            backend.fail_next_restart.store(true, Ordering::SeqCst);
        }
        let cancel = Arc::new(FakeCancel::new());
        let cancel_at_retest = cancel.clone();
        let mut config = base_config();
        config.method_version = METHOD_VERSION;
        config.candidate_lps.clear();
        let mut ctx = build_ctx(
            &root,
            backend.clone(),
            processes,
            cancel,
            Arc::new(NoopSleeper),
            config,
            &journal,
            Some(Box::new(move |p| {
                if mode == "cancel" && p.round == Some(1) {
                    cancel_at_retest.set(true);
                }
            })),
        );
        ctx.topo = build_topology(vec![(vec![0, 63], 0, true)]);
        let result = run_benchmark(&mut ctx);
        if mode == "single" {
            assert_eq!(
                result.status,
                SessionStatus::Completed,
                "{:?}",
                result.error
            );
            assert_eq!(
                result.detail.summary.quick.as_ref().unwrap().status,
                RankingStatus::SingleCandidate
            );
            assert_eq!(result.detail.summary.capture_quality.total_captures, 4);
        // 2 基線 + 2 候選
        } else if mode == "cancel" {
            assert_eq!(result.status, SessionStatus::Cancelled);
        } else {
            assert_eq!(result.status, SessionStatus::Failed, "{mode}");
        }
        if mode == "restore" {
            assert!(journal.exists());
            assert!(result.recovery_required);
        } else {
            assert_eq!(backend.current_policy(GPU_A), baseline);
            assert!(!journal.exists());
        }
    }
}
