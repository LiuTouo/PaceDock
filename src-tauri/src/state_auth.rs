//! 特權狀態檔的完整性認證。
//!
//! `%APPDATA%\PaceDock` 下的狀態（還原日誌、還原記錄、session 結果）可被
//! 同帳戶 medium-integrity 程序任意竄改，卻會驅動提升權限端的 HKLM 寫入與
//! 裝置重啟。此處以 HMAC-SHA256 認證檔案內容：HMAC key 存放在僅
//! Administrators/SYSTEM 可讀的目錄（`%PROGRAMDATA%\PaceDock`），攻擊者
//! 無法為竄改後的內容重算 MAC；驗證失敗一律 fail closed。
//!
//! 格式：內容檔旁另存 `<file>.mac`（hex）。兩者同時竄改無效（無 key）。

use std::path::{Path, PathBuf};

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

const KEY_FILE: &str = "state.key";
/// key 長度（bytes）
const KEY_LEN: usize = 32;
/// 認證檔案的大小上限（防 oversized input）
pub const MAX_AUTHENTICATED_SIZE: u64 = 1024 * 1024;

/// HMAC key 的存放目錄（僅 Administrators/SYSTEM 可讀）
fn key_dir() -> PathBuf {
    // FOLDERID_ProgramData：%PROGRAMDATA%（預設 DACL 給 Users 讀權，
    // 因此目錄本身必須以 admin-only DACL 建立）
    let program_data = known_program_data().unwrap_or_else(|| {
        PathBuf::from(std::env::var_os("ProgramData").unwrap_or_default())
    });
    program_data.join("PaceDock")
}

fn known_program_data() -> Option<PathBuf> {
    use windows::Win32::UI::Shell::{FOLDERID_ProgramData, SHGetKnownFolderPath};
    let path = unsafe {
        SHGetKnownFolderPath(&FOLDERID_ProgramData, windows::Win32::UI::Shell::KNOWN_FOLDER_FLAG(0), None)
            .ok()?
    };
    let s = unsafe { path.to_string() }.ok()?;
    Some(PathBuf::from(s))
}

/// 讀取既有 key 檔（長度必須正確）
fn read_key(path: &Path) -> Result<[u8; KEY_LEN], String> {
    let bytes = std::fs::read(path).map_err(|e| format!("讀取狀態認證 key 失敗: {e}"))?;
    if bytes.len() != KEY_LEN {
        return Err(format!(
            "狀態認證 key 長度異常（{} bytes，預期 {KEY_LEN}）",
            bytes.len()
        ));
    }
    let mut key = [0u8; KEY_LEN];
    key.copy_from_slice(&bytes);
    Ok(key)
}

/// 每 process 只載入/生成一次：平行測試或多執行緒下，若各自生成再用
/// rename 落地，Windows 的 rename 會覆蓋既有 key，造成已寫入 MAC 用舊 key、
/// 之後驗證失敗。process 內快取後，初次生成只在單一執行緒發生一次。
static KEY_CACHE: std::sync::OnceLock<Result<[u8; KEY_LEN], String>> = std::sync::OnceLock::new();

/// 載入（必要時生成）HMAC key。生成與讀取都限定在 admin-only 目錄。
fn load_or_create_key() -> Result<[u8; KEY_LEN], String> {
    KEY_CACHE.get_or_init(init_key).clone()
}

fn init_key() -> Result<[u8; KEY_LEN], String> {
    let dir = key_dir();
    if let Err(e) = crate::syspath::create_admin_only_dir(&dir) {
        // 平行建立者可能剛把目錄建出來（fail-closed 的 ALREADY_EXISTS）— 可續用
        if !dir.exists() {
            return Err(e);
        }
    }
    let path = dir.join(KEY_FILE);
    if path.exists() {
        return read_key(&path);
    }

    // 生成：兩組 UUID v4（各 122 bits 隨機）串接
    let mut key = [0u8; KEY_LEN];
    key[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    key[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());

    // exclusive create：撞名（他者已落地）→ 以既有 key 為準（不覆蓋）
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut f) => {
            use std::io::Write;
            f.write_all(&key)
                .map_err(|e| format!("寫入狀態認證 key 失敗: {e}"))?;
            Ok(key)
        }
        Err(_) => read_key(&path),
    }
}

fn mac_hex(data: &[u8]) -> Result<String, String> {
    let key = load_or_create_key()?;
    let mut mac = HmacSha256::new_from_slice(&key).map_err(|e| format!("HMAC 初始化失敗: {e}"))?;
    mac.update(data);
    let tag = mac.finalize().into_bytes();
    Ok(tag.iter().map(|b| format!("{b:02x}")).collect())
}

fn mac_sidecar_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".mac");
    path.with_file_name(name)
}

/// 以認證格式寫入：內容 + 旁檔 MAC（兩者皆原子寫入）
pub fn auth_write(path: &Path, text: &str) -> Result<(), String> {
    let tag = mac_hex(text.as_bytes())?;
    crate::config::atomic_write(path, text)?;
    crate::config::atomic_write(&mac_sidecar_path(path), &tag)
}

/// 讀取認證檔案：大小上限 + MAC 驗證。檔案、旁檔缺失或不符一律 Err
/// （呼叫端決定 fail-closed 行為）。
pub fn auth_read(path: &Path) -> Result<String, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("讀取狀態檔失敗: {e}"))?;
    if meta.len() > MAX_AUTHENTICATED_SIZE {
        return Err(format!(
            "狀態檔超出大小上限（{} bytes，上限 {MAX_AUTHENTICATED_SIZE}）",
            meta.len()
        ));
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("讀取狀態檔失敗: {e}"))?;
    let tag = std::fs::read_to_string(mac_sidecar_path(path))
        .map_err(|_| "狀態檔缺少 MAC 旁檔（可能遭移除或為舊版未認證資料）".to_string())?;
    let expected = mac_hex(text.as_bytes())?;
    if tag.trim().ne(&expected) {
        return Err("狀態檔 MAC 驗證失敗（內容可能遭竄改）".to_string());
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pacedock_state_auth_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(format!("{name}_{:?}", std::thread::current().id()))
    }

    #[test]
    fn auth_write_read_roundtrip() {
        let path = temp_path("roundtrip.json");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(mac_sidecar_path(&path));
        auth_write(&path, "{\"a\":1}").unwrap();
        assert_eq!(auth_read(&path).unwrap(), "{\"a\":1}");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(mac_sidecar_path(&path));
    }

    #[test]
    fn tampered_content_rejected() {
        let path = temp_path("tamper.json");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(mac_sidecar_path(&path));
        auth_write(&path, "original").unwrap();
        std::fs::write(&path, "tampered").unwrap();
        assert!(auth_read(&path).is_err(), "竄改內容必須被拒");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(mac_sidecar_path(&path));
    }

    #[test]
    fn missing_mac_sidecar_rejected() {
        let path = temp_path("nomac.json");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(mac_sidecar_path(&path));
        auth_write(&path, "data").unwrap();
        std::fs::remove_file(mac_sidecar_path(&path)).unwrap();
        assert!(auth_read(&path).is_err(), "缺 MAC 旁檔必須被拒");
        let _ = std::fs::remove_file(&path);
    }
}
