//! 設定持久化（PLAN §7.8）：%APPDATA%\PaceDock\config.json，原子寫入 + 壞檔備份。

use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::{
    MoveFileExW, ReplaceFileW, MOVEFILE_WRITE_THROUGH, REPLACE_FILE_FLAGS,
};

use crate::model::Config;

pub fn config_dir() -> PathBuf {
    let base = std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|p| p.to_path_buf()))
                .unwrap_or_else(|| PathBuf::from("."))
        });
    base.join("PaceDock")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

pub fn load() -> Result<Config, String> {
    load_with_retries(&config_path(), &[100, 250, 400])
}

#[cfg(test)]
pub fn load_from(path: &Path) -> Result<Config, String> {
    load_with_retries(path, &[])
}

fn load_with_retries(path: &Path, retry_delays_ms: &[u64]) -> Result<Config, String> {
    let mut retry = retry_delays_ms.iter();
    let text = loop {
        match std::fs::read_to_string(path) {
            Ok(text) => break text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(e) => match retry.next() {
                Some(delay) => {
                    log::warn!("config 讀取失敗，{delay} ms 後重試: {e}");
                    std::thread::sleep(std::time::Duration::from_millis(*delay));
                }
                None => {
                    return Err(format!("CONFIG_FAILED: read {}: {e}", path.display()));
                }
            },
        }
    };
    match serde_json::from_str::<Config>(&text) {
        Ok(cfg) => Ok(cfg),
        Err(e) => {
            // 壞檔：備份為 config.corrupt.json，用預設值，不覆蓋使用者原檔（PLAN §7.8）
            log::error!("config 解析失敗，備份原檔: {e}");
            let backup = path.with_file_name("config.corrupt.json");
            std::fs::copy(path, &backup).map_err(|backup_error| {
                format!(
                    "CONFIG_FAILED: parse {}; backup {}: {backup_error}",
                    path.display(),
                    backup.display()
                )
            })?;
            Ok(Config::default())
        }
    }
}

pub fn save(cfg: &Config) -> Result<(), String> {
    save_to(&config_path(), cfg)
}

/// 原子寫入：先寫 tmp 再 rename（PLAN §5.1）
pub fn save_to(path: &Path, cfg: &Config) -> Result<(), String> {
    let text = serde_json::to_string_pretty(cfg).map_err(|e| format!("serialize: {e}"))?;
    atomic_write(path, &text)
}

/// tmp 檔名序號：避免固定暫存名在並行寫入時碰撞。
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// 原子寫入文字檔。先寫同目錄 tmp 並 `sync_all` 落地，再用 Windows 原子取代原語 commit：
/// - 目標已存在 → `ReplaceFileW` 原子取代（不先刪，無資料遺失空窗）。
/// - 目標不存在 → `MoveFileExW`（WRITE_THROUGH）改名。
///
/// 資料（`sync_all`）先於 metadata（rename/replace）落地，commit 前既有目標絕不被刪。
/// config、基準測試 session.json、還原日誌共用同一套防壞檔寫法。
pub fn atomic_write(path: &Path, text: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create dir: {e}"))?;
    }
    let tmp = tmp_path(path);
    if let Err(e) = write_synced(&tmp, text) {
        // 寫入或 sync 失敗 → 清除 tmp，既有目標未動、內容保留。
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = commit(&tmp, path) {
        // commit 失敗 → 清除 tmp，既有目標未動、內容保留。
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// 用 `File` handle 寫入完整 UTF-8 位元組並 `sync_all` 落地（`FlushFileBuffers`），
/// 確保取代 commit 前，替換資料已持久化到磁碟。
fn write_synced(tmp: &Path, text: &str) -> Result<(), String> {
    use std::io::Write;
    let mut f = std::fs::File::create(tmp).map_err(|e| format!("create tmp: {e}"))?;
    f.write_all(text.as_bytes())
        .map_err(|e| format!("write tmp: {e}"))?;
    f.sync_all().map_err(|e| format!("sync tmp: {e}"))?;
    Ok(())
}

/// 同目錄、唯一命名的暫存檔路徑（與目標同 volume，rename/replace 才安全）。
fn tmp_path(path: &Path) -> PathBuf {
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let mut name = std::ffi::OsString::from(".");
    name.push(path.file_name().unwrap_or_default());
    name.push(format!(".{}.{}.{}.tmp", std::process::id(), nanos, seq));
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.join(name),
        _ => PathBuf::from(name),
    }
}

/// Path → 以 NUL 結尾的 UTF-16（Win32 寬字串）。
fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// 把寫好的 tmp commit 成最終路徑：既有目標走原子取代，新檔走改名。
/// `ReplaceFileW` 使用預設 flags（0）：`REPLACEFILE_WRITE_THROUGH` 依微軟文件標記
/// 「Not supported」，不採用；資料耐久性由 [`write_synced`] 的 `sync_all` 負責。
fn commit(tmp: &Path, path: &Path) -> Result<(), String> {
    let target = wide(path);
    let replacement = wide(tmp);
    unsafe {
        if path.exists() {
            ReplaceFileW(
                PCWSTR(target.as_ptr()),
                PCWSTR(replacement.as_ptr()),
                PCWSTR::null(),
                REPLACE_FILE_FLAGS(0),
                None,
                None,
            )
            .map_err(|e| format!("replace: {e}"))
        } else {
            // WRITE_THROUGH 是 MoveFileExW 支援的 flag，讓 rename 的 metadata 也落地。
            MoveFileExW(
                PCWSTR(replacement.as_ptr()),
                PCWSTR(target.as_ptr()),
                MOVEFILE_WRITE_THROUGH,
            )
            .map_err(|e| format!("rename: {e}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Rule;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("pacedock_test_{}_{}", std::process::id(), name))
    }

    #[test]
    fn roundtrip_preserves_data() {
        let path = temp_path("roundtrip.json");
        let mut cfg = Config::default();
        cfg.settings.poll_interval_ms = 2000;
        cfg.rules
            .push(Rule::new(r"C:\Games\game.exe".into(), "Game".into()));

        save_to(&path, &cfg).unwrap();
        let loaded = load_from(&path).unwrap();
        assert_eq!(loaded.settings.poll_interval_ms, 2000);
        assert_eq!(loaded.rules.len(), 1);
        assert_eq!(loaded.rules[0].exe_path, r"C:\Games\game.exe");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn corrupt_file_is_backed_up_and_defaulted() {
        let path = temp_path("corrupt.json");
        std::fs::write(&path, "{ not valid json !!!").unwrap();
        let cfg = load_from(&path).unwrap();
        assert_eq!(cfg.version, 1);
        let backup = path.with_file_name("config.corrupt.json");
        assert!(backup.exists());
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&backup);
    }

    #[test]
    fn missing_file_gives_default() {
        let path = temp_path("missing.json");
        let _ = std::fs::remove_file(&path);
        let cfg = load_from(&path).unwrap();
        assert_eq!(cfg.settings.language, "zh-TW");
    }

    #[test]
    fn non_not_found_read_error_is_not_defaulted() {
        let path = temp_path("read_error_dir");
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        let error = load_from(&path).unwrap_err();
        assert!(error.starts_with("CONFIG_FAILED: read "));
        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn missing_fields_use_defaults() {
        let path = temp_path("partial.json");
        std::fs::write(&path, r#"{ "version": 1 }"#).unwrap();
        let cfg = load_from(&path).unwrap();
        assert_eq!(cfg.settings.poll_interval_ms, 1000);
        assert!(cfg.rules.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    /// 舊 config 含 affinity.primaryCore：load_from 成功、規則保留，
    /// save_to 後輸出的 JSON 不含 primaryCore。
    #[test]
    fn old_primary_core_field_is_ignored_and_stripped() {
        let path = temp_path("old_primary.json");
        let json = r#"{
            "version": 1,
            "rules": [
                {
                    "id": "test-id",
                    "name": "TestGame",
                    "exePath": "C:\\Games\\game.exe",
                    "matchBy": "FullPath",
                    "enabled": true,
                    "affinity": { "mode": "Custom", "cores": [0, 1, 2], "primaryCore": 1 },
                    "priority": "High",
                    "advanced": { "ioPriority": null, "memoryPriority": null }
                }
            ]
        }"#;
        std::fs::write(&path, json).unwrap();
        let cfg = load_from(&path).unwrap();
        assert_eq!(cfg.rules.len(), 1);
        assert_eq!(cfg.rules[0].name, "TestGame");
        assert_eq!(
            cfg.rules[0].affinity.mode,
            crate::model::AffinityMode::Custom
        );
        assert_eq!(cfg.rules[0].affinity.cores, vec![0, 1, 2]);

        // 存回後檔案不含 primaryCore
        save_to(&path, &cfg).unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(!saved.contains("primaryCore"));
        assert!(saved.contains("\"cores\""));

        let _ = std::fs::remove_file(&path);
    }

    /// 舊 config 不含 theme 欄位 → 預設 Dark
    #[test]
    fn old_config_without_theme_defaults_to_dark() {
        let path = temp_path("no_theme.json");
        let json = r#"{
            "version": 1,
            "settings": {
                "language": "en",
                "pollIntervalMs": 2000
            }
        }"#;
        std::fs::write(&path, json).unwrap();
        let cfg = load_from(&path).unwrap();
        assert_eq!(cfg.settings.theme, crate::model::Theme::Dark);
        assert_eq!(cfg.settings.poll_interval_ms, 2000);
        let _ = std::fs::remove_file(&path);
    }

    /// 新檔：直接建立，內容逐位元組保留。
    #[test]
    fn atomic_write_creates_new_file() {
        let path = temp_path("atomic_new.json");
        let _ = std::fs::remove_file(&path);
        atomic_write(&path, "{\"new\":true}").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"new\":true}");
        let _ = std::fs::remove_file(&path);
    }

    /// 取代既有檔：舊內容換成新內容，且不殘留 tmp。
    #[test]
    fn atomic_write_replaces_existing_file() {
        let path = temp_path("atomic_replace.json");
        std::fs::write(&path, "old").unwrap();
        atomic_write(&path, "new").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        let prefix = format!(
            ".pacedock_test_{}_atomic_replace.json.",
            std::process::id()
        );
        let leftover = std::fs::read_dir(std::env::temp_dir())
            .unwrap()
            .flatten()
            .any(|e| {
                let n = e.file_name();
                let n = n.to_string_lossy();
                n.starts_with(&prefix) && n.ends_with(".tmp")
            });
        assert!(!leftover, "commit 後不該殘留 tmp");
        let _ = std::fs::remove_file(&path);
    }

    /// 取代失敗（唯讀目標）→ Err，且既有內容原封不動、無殘留 tmp。
    #[test]
    #[allow(clippy::permissions_set_readonly_false)] // Windows-only：clear 唯讀以清理測試檔
    fn atomic_write_failure_preserves_existing_file() {
        let path = temp_path("atomic_fail.json");
        std::fs::write(&path, "old").unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&path, perms).unwrap();

        assert!(atomic_write(&path, "new").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old");

        // 復原唯讀以便清理
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_readonly(false);
        std::fs::set_permissions(&path, perms).unwrap();
        let _ = std::fs::remove_file(&path);
    }
    #[test]
    fn saving_general_settings_preserves_disabled_legacy_game_rules() {
        let raw = r#"{"version":1,"settings":{"language":"zh-TW","startWithWindows":true,"startMinimized":true,"closeToTray":true},"rules":[{"id":"legacy","name":"Game","exePath":"C:/Game.exe","enabled":true,"matchBy":"FullPath","affinity":{"mode":"Custom","cores":[0,1,4]},"priority":"High","advanced":{"ioPriority":"High","memoryPriority":"Normal"}}]}"#;
        let mut config: Config = serde_json::from_str(raw).unwrap();
        let rules = serde_json::to_value(&config.rules).unwrap();
        config.settings.language = "en".into();
        let path = std::env::temp_dir().join(format!(
            "pacedock_preserve_rules_{}.json",
            uuid::Uuid::new_v4()
        ));
        save_to(&path, &config).unwrap();
        let loaded = load_from(&path).unwrap();
        assert_eq!(serde_json::to_value(&loaded.rules).unwrap(), rules);
        assert_eq!(loaded.settings.language, "en");
        std::fs::remove_file(path).unwrap();
    }
}
