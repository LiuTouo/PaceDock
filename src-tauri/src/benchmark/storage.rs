//! 基準測試 session 儲存：`%APPDATA%\PaceDock\benchmarks\<uuid>`。
//! session.json 原子寫入；CSV 目錄 path helper；歷史 list/get/delete 與
//! 嚴謹路徑驗證（只接受合法 UUID，杜絕穿越）；總位元組數回報。
//! 永不自動刪除歷史。

use std::path::{Path, PathBuf};

use uuid::Uuid;

use super::{SessionDetail, SessionSummary};
use crate::config;
use crate::error::codes;

/// benchmarks 根目錄
pub fn benchmarks_root() -> PathBuf {
    config::config_dir().join("benchmarks")
}

/// 驗證 session id 並回傳其資料夾路徑。只接受合法 UUID。
fn session_dir_at(root: &Path, id: &str) -> Result<PathBuf, String> {
    Uuid::parse_str(id).map_err(|_| codes::BENCHMARK_INVALID_SESSION_ID.to_string())?;
    Ok(root.join(id))
}

// Task 2 的 runner 才會呼叫下面這些寫入/路徑 helper；目前僅測試覆蓋。

/// 儲存/更新 session.json（原子寫入）
#[allow(dead_code)]
pub fn session_json_path(id: &str) -> Result<PathBuf, String> {
    Ok(session_dir_at(&benchmarks_root(), id)?.join("session.json"))
}

/// 未處理的原始取樣 CSV（Task 2 runner 寫入；目前只提供路徑）
#[allow(dead_code)]
pub fn csv_metrics_path(id: &str) -> Result<PathBuf, String> {
    Ok(session_dir_at(&benchmarks_root(), id)?.join("metrics.csv"))
}

/// 逐 LP 最終結果 CSV（Task 2 runner 寫入；目前只提供路徑）
#[allow(dead_code)]
pub fn csv_results_path(id: &str) -> Result<PathBuf, String> {
    Ok(session_dir_at(&benchmarks_root(), id)?.join("results.csv"))
}

/// 儲存/更新 session.json（原子寫入）
#[allow(dead_code)]
pub fn save_session(detail: &SessionDetail) -> Result<(), String> {
    save_session_at(&benchmarks_root(), detail)
}

#[allow(dead_code)]
pub fn save_session_at(root: &Path, detail: &SessionDetail) -> Result<(), String> {
    let dir = session_dir_at(root, &detail.summary.id)?;
    let path = dir.join("session.json");
    let text = serde_json::to_string_pretty(detail)
        .map_err(|e| format!("{}: {e}", codes::BENCHMARK_STORAGE_FAILED))?;
    // session 結果會驅動 apply_best（提升權限 GPU mutation）— 以 HMAC 認證寫入
    crate::state_auth::auth_write(&path, &text)
}

/// 歷史摘要列表（依 startedAt 降冪）
pub fn list() -> Result<Vec<SessionSummary>, String> {
    list_at(&benchmarks_root())
}

pub fn list_at(root: &Path) -> Result<Vec<SessionSummary>, String> {
    let mut out = Vec::new();
    if !root.exists() {
        return Ok(out);
    }
    for entry in
        std::fs::read_dir(root).map_err(|e| format!("{}: {e}", codes::BENCHMARK_STORAGE_FAILED))?
    {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if Uuid::parse_str(&name).is_err() {
            continue; // 非 UUID 資料夾（不相干）忽略
        }
        if let Ok(Some(mut summary)) = read_summary(&path.join("session.json")) {
            summary.total_bytes = dir_size(&path);
            out.push(summary);
        }
    }
    out.sort_by(|a, b| {
        let a_time = chrono::DateTime::parse_from_rfc3339(&a.started_at)
            .ok()
            .map(|value| value.timestamp_millis());
        let b_time = chrono::DateTime::parse_from_rfc3339(&b.started_at)
            .ok()
            .map(|value| value.timestamp_millis());
        b_time.cmp(&a_time).then_with(|| a.id.cmp(&b.id))
    });
    Ok(out)
}

/// 讀單一 session 完整內容
pub fn get(id: &str) -> Result<SessionDetail, String> {
    get_at(&benchmarks_root(), id)
}

pub fn get_at(root: &Path, id: &str) -> Result<SessionDetail, String> {
    let dir = session_dir_at(root, id)?;
    let path = dir.join("session.json");
    let text = std::fs::read_to_string(&path)
        .map_err(|_| codes::BENCHMARK_SESSION_NOT_FOUND.to_string())?;
    let mut detail: SessionDetail = serde_json::from_str(&text)
        .map_err(|e| format!("{}: {e}", codes::BENCHMARK_STORAGE_FAILED))?;
    detail.summary.total_bytes = dir_size(&dir);
    Ok(detail)
}

/// 供特權消費端（apply_best）讀取：內容必須通過 HMAC 認證。
/// 未認證的舊 session 或遭竄改檔案一律拒絕（fail closed）。
pub fn get_at_verified(root: &Path, id: &str) -> Result<SessionDetail, String> {
    let dir = session_dir_at(root, id)?;
    let path = dir.join("session.json");
    if !path.exists() {
        return Err(codes::BENCHMARK_SESSION_NOT_FOUND.to_string());
    }
    let text = crate::state_auth::auth_read(&path)?;
    let mut detail: SessionDetail = serde_json::from_str(&text)
        .map_err(|e| format!("{}: {e}", codes::BENCHMARK_STORAGE_FAILED))?;
    detail.summary.total_bytes = dir_size(&dir);
    Ok(detail)
}

/// 刪除整個 session 資料夾。要求存在且 id 合法；永不自動刪除。
pub fn delete(id: &str) -> Result<(), String> {
    delete_at(&benchmarks_root(), id)
}

pub fn delete_at(root: &Path, id: &str) -> Result<(), String> {
    let dir = session_dir_at(root, id)?;
    if !dir.exists() {
        return Err(codes::BENCHMARK_SESSION_NOT_FOUND.to_string());
    }
    std::fs::remove_dir_all(&dir).map_err(|e| format!("{}: {e}", codes::BENCHMARK_STORAGE_FAILED))
}

/// 整個 benchmarks 根的總位元組數
pub fn total_bytes() -> u64 {
    total_bytes_at(&benchmarks_root())
}

pub fn total_bytes_at(root: &Path) -> u64 {
    dir_size(root)
}

fn read_summary(path: &Path) -> Result<Option<SessionSummary>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return Ok(None), // 壞檔或無 session.json → 當作不存在
    };
    match serde_json::from_str::<SessionDetail>(&text) {
        Ok(detail) => Ok(Some(detail.summary)),
        Err(e) => {
            log::warn!("session.json 解析失敗 {}: {e}", path.display());
            Ok(None)
        }
    }
}

fn dir_size(dir: &Path) -> u64 {
    let mut total = 0u64;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_dir() {
                total += dir_size(&path);
            } else if let Ok(meta) = entry.metadata() {
                total += meta.len();
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::benchmark::{BenchmarkConfig, LpResult, SessionStatus};

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pacedock_bench_test_{}_{}",
            std::process::id(),
            name
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn sample_detail(id: &str) -> SessionDetail {
        SessionDetail {
            summary: crate::benchmark::SessionSummary {
                id: id.to_string(),
                status: SessionStatus::Completed,
                started_at: "2026-08-11T00:00:00Z".into(),
                finished_at: Some("2026-08-11T00:01:00Z".into()),
                gpu_name: "Fake GPU".into(),
                gpu_instance_id: r"PCI\VEN_FAKE".into(),
                cpu_fingerprint: "fixture-fingerprint".to_string(),
                best_lp: Some(3),
                reliability: Default::default(),
                severe_lps: vec![],
                sample_count: 5,
                total_bytes: 0,
                config: BenchmarkConfig::default(),
                error: None,
                ..Default::default()
            },
            results: vec![LpResult {
                lp: 3,
                avg_fps: Some(240.0),
                completed: true,
                ..Default::default()
            }],
            samples: vec![],
            ..Default::default()
        }
    }

    #[test]
    fn invalid_session_ids_rejected() {
        let root = temp_root("invalid");
        let mut bads = vec![
            String::from("../config.json"),
            String::from(r"C:\config.json"),
            String::from(r"..\..\x"),
            String::new(),
            String::from("foo"),
            String::from("urn:uuid:.."),
        ];
        bads.push("a".repeat(50));
        for bad in bads {
            assert!(
                session_dir_at(&root, &bad).is_err(),
                "should reject {bad:?}"
            );
        }
    }

    #[test]
    fn save_get_roundtrip() {
        let root = temp_root("roundtrip");
        let id = Uuid::new_v4().to_string();
        save_session_at(&root, &sample_detail(&id)).unwrap();
        let loaded = get_at(&root, &id).unwrap();
        assert_eq!(loaded.summary.id, id);
        assert_eq!(loaded.summary.status, SessionStatus::Completed);
        assert_eq!(loaded.summary.best_lp, Some(3));
        assert_eq!(loaded.results[0].lp, 3);
        assert_eq!(
            loaded.summary.cpu_fingerprint,
            sample_detail(&id).summary.cpu_fingerprint
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn list_sorted_and_skips_garbage() {
        let root = temp_root("list");
        let id1 = Uuid::new_v4().to_string();
        let id2 = Uuid::new_v4().to_string();
        let mut d1 = sample_detail(&id1);
        d1.summary.started_at = "2026-08-11T01:00:00Z".into();
        let mut d2 = sample_detail(&id2);
        d2.summary.started_at = "2026-08-11T00:00:00Z".into();
        save_session_at(&root, &d1).unwrap();
        save_session_at(&root, &d2).unwrap();
        std::fs::create_dir_all(root.join("not-a-uuid")).unwrap();

        let list = list_at(&root).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, id1); // 較新在前
        assert_eq!(list[1].id, id2);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn list_sorts_rfc3339_by_instant_not_text() {
        let root = temp_root("list_offsets");
        let older_id = Uuid::new_v4().to_string();
        let newer_id = Uuid::new_v4().to_string();
        let mut older = sample_detail(&older_id);
        // 字典序較大，但實際為前一天 23:00Z。
        older.summary.started_at = "2026-08-11T01:00:00+02:00".into();
        let mut newer = sample_detail(&newer_id);
        newer.summary.started_at = "2026-08-11T00:30:00Z".into();
        save_session_at(&root, &older).unwrap();
        save_session_at(&root, &newer).unwrap();

        let list = list_at(&root).unwrap();
        assert_eq!(list[0].id, newer_id);
        assert_eq!(list[1].id, older_id);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn delete_removes_and_missing_errs() {
        let root = temp_root("delete");
        let id = Uuid::new_v4().to_string();
        save_session_at(&root, &sample_detail(&id)).unwrap();
        assert!(get_at(&root, &id).is_ok());
        delete_at(&root, &id).unwrap();
        assert!(!root.join(&id).exists());
        assert!(delete_at(&root, &id).is_err()); // 已刪除 → not found
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn total_bytes_sums_all_files() {
        let root = temp_root("bytes");
        let id = Uuid::new_v4().to_string();
        save_session_at(&root, &sample_detail(&id)).unwrap();
        let dir = root.join(&id);
        std::fs::write(dir.join("metrics.csv"), "a,b\n1,2\n").unwrap();
        assert!(total_bytes_at(&root) > 0);
        let before = total_bytes_at(&root);
        std::fs::write(dir.join("extra.bin"), vec![0u8; 100]).unwrap();
        assert_eq!(total_bytes_at(&root), before + 100);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn csv_helpers_validate_id() {
        assert!(csv_metrics_path("../x").is_err());
        assert!(csv_results_path("../x").is_err());
    }

    /// 從 JSON 中移除 summary.error 欄位（模擬舊版 session.json）
    fn remove_error_field(json: &str) -> String {
        let mut v: serde_json::Value = serde_json::from_str(json).unwrap();
        if let Some(summary) = v.get_mut("summary") {
            summary.as_object_mut().unwrap().remove("error");
        }
        serde_json::to_string_pretty(&v).unwrap()
    }

    /// 舊版 session.json 沒有 error 欄位 → serde default 必須照常載入（error=None）
    #[test]
    fn session_without_error_field_loads_with_error_none() {
        let root = temp_root("noerr");
        let id = Uuid::new_v4().to_string();
        let mut detail = sample_detail(&id);
        detail.summary.status = SessionStatus::Failed;
        let json = serde_json::to_string_pretty(&detail).unwrap();
        assert!(json.contains("\"error\""));
        let cleaned = remove_error_field(&json);
        let v: serde_json::Value = serde_json::from_str(&cleaned).unwrap();
        assert!(
            v["summary"].get("error").is_none(),
            "summary 不該再有 error 欄位"
        );

        let dir = root.join(&id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("session.json"), cleaned).unwrap();

        let loaded = get_at(&root, &id).unwrap();
        assert_eq!(loaded.summary.status, SessionStatus::Failed);
        assert_eq!(
            loaded.summary.error, None,
            "舊檔案缺 error 欄位 → 預設 None"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 舊版 session.json 沒有 reliability 欄位 → serde default 照常載入（Unassessed）
    #[test]
    fn session_without_reliability_field_loads_unassessed() {
        let root = temp_root("norel");
        let id = Uuid::new_v4().to_string();
        let mut detail = sample_detail(&id);
        detail.summary.reliability = crate::benchmark::ReliabilitySummary {
            status: crate::benchmark::ReliabilityStatus::Passed,
            ..Default::default()
        };
        let json = serde_json::to_string_pretty(&detail).unwrap();
        assert!(json.contains("\"reliability\""));
        // 移除 reliability 欄位模擬舊 session.json
        let mut v: serde_json::Value = serde_json::from_str(&json).unwrap();
        v["summary"].as_object_mut().unwrap().remove("reliability");
        let cleaned = serde_json::to_string_pretty(&v).unwrap();
        assert!(v["summary"].get("reliability").is_none());

        let dir = root.join(&id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("session.json"), cleaned).unwrap();

        let loaded = get_at(&root, &id).unwrap();
        assert_eq!(
            loaded.summary.reliability.status,
            crate::benchmark::ReliabilityStatus::Unassessed
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 失敗原因（穩定錯誤代碼）必須能存能讀，供 reload 後 UI 顯示
    #[test]
    fn failure_error_roundtrips() {
        let root = temp_root("errrt");
        let id = Uuid::new_v4().to_string();
        let mut detail = sample_detail(&id);
        detail.summary.status = SessionStatus::Failed;
        detail.summary.error = Some(crate::error::codes::BENCHMARK_CAPTURE_MISSING.to_string());
        save_session_at(&root, &detail).unwrap();

        let loaded = get_at(&root, &id).unwrap();
        assert_eq!(loaded.summary.status, SessionStatus::Failed);
        assert_eq!(
            loaded.summary.error.as_deref(),
            Some(crate::error::codes::BENCHMARK_CAPTURE_MISSING)
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
