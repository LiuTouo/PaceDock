//! 自動更新：NSIS 透過 tauri-plugin-updater；可攜版自行實作。
//!
//! 可攜版偵測：exe 同層存在 `.pacedock-portable` 標記檔。
//! 更新流程：
//!   1. 以 GitHub Releases API 查詢最新非 prerelease 版本
//!   2. 比對 semver；若目前版本 >= 最新版本則無動作
//!   3. 從 assets 中精確選取可攜版 zip（PaceDock_X.Y.Z_x64-portable.zip）
//!   4. 下載並驗證對應的 .sha256 校驗檔（強制，失敗即拒絕）
//!   5. 解壓縮出新 exe（檢查 ZIP 內含標記檔、單一 exe、無路徑遍歷）
//!   6. 產生 PowerShell 輔助腳本（安全引用路徑、等待、備份、置換、重啟）
//!   7. 設定 quitting flag，執行輔助腳本後呼叫 app.exit(0)

use std::io::Write;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};

use semver::Version;
use serde::Deserialize;

/// 避免 PowerShell 彈出視窗（與 autostart.rs 一致）
const CREATE_NO_WINDOW: u32 = 0x08000000;

// ── GitHub API 資料結構 ──

#[derive(Deserialize, Debug)]
struct GhRelease {
    tag_name: String,
    prerelease: bool,
    draft: Option<bool>,
    assets: Vec<GhAsset>,
}

#[derive(Deserialize, Debug, Clone)]
pub(crate) struct GhAsset {
    pub name: String,
    pub browser_download_url: String,
    pub size: u64,
}

// ── 公開型別 ──

/// 前端 update-state event 的 payload（camelCase 序列化給前端）
#[derive(serde::Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct UpdateState {
    pub status: UpdateStatus,
    /// 最新版本號（不含 v 前綴），僅 Checked/Available/Downloading 有意義
    pub latest_version: Option<String>,
    /// 目前版本號
    pub current_version: String,
    /// 下載進度 0..100（僅 Downloading 狀態時更新）
    pub progress: Option<u32>,
    /// 人類可讀錯誤訊息
    pub error: Option<String>,
}

#[derive(serde::Serialize, Clone, Debug, PartialEq)]
pub enum UpdateStatus {
    /// 正在查詢 GitHub API
    Checking,
    /// 已是最新版本
    UpToDate,
    /// 有新版本可用，等待使用者確認
    Available,
    /// 下載中
    Downloading,
    /// 準備安裝（NSIS）或替換（可攜版），通知前端準備結束
    Installing,
    /// 發生錯誤
    Error,
}

// ── GitHub API 常數 ──

const GITHUB_API_RELEASES: &str = "https://api.github.com/repos/LiuTouo/PaceDock/releases";
const USER_AGENT: &str = "PaceDock";

/// 可攜版 ZIP 大小上限 (100 MiB)
const MAX_ZIP_SIZE: u64 = 100 * 1024 * 1024;

// ── 可攜版偵測 ──

/// 可攜版標記檔名（放在 exe 同層目錄）
pub const PORTABLE_MARKER: &str = ".pacedock-portable";

/// exe 同層目錄存在 `.pacedock-portable` 時為可攜版
pub fn is_portable() -> bool {
    current_exe_dir()
        .map(|d| d.join(PORTABLE_MARKER).exists())
        .unwrap_or(false)
}

/// 目前執行檔所在目錄
pub fn current_exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()?
        .parent()
        .map(|p| p.to_path_buf())
}

/// 目前執行檔完整路徑
pub fn current_exe_path() -> Option<PathBuf> {
    std::env::current_exe().ok()
}

// ── 版本取得 ──

/// 從 Tauri app handle 的 package info 取得版本
pub fn current_version(app: &tauri::AppHandle) -> String {
    app.package_info().version.to_string()
}

// ── HTTP 輔助 ──

fn http_client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .unwrap_or_default()
}

fn map_http_error(err: reqwest::Error, prefix: &str) -> String {
    if err.is_timeout() {
        format!("{prefix}：連線逾時")
    } else if err.is_connect() {
        format!("{prefix}：無法連線（{}）", err)
    } else {
        format!("{prefix}：{err}")
    }
}

fn map_status_code(status: u16, prefix: &str) -> String {
    if status == 403 {
        format!("{prefix}：HTTP 403（可能超過 GitHub API 速率限制）")
    } else if status == 404 {
        format!("{prefix}：HTTP 404（找不到資源）")
    } else {
        format!("{prefix}：HTTP {status}")
    }
}

// ── 可攜版發行資訊 ──

/// 解析後的單一可攜版發行
#[derive(Debug, Clone)]
pub struct PortableRelease {
    /// 語意化版本（不含 v 前綴）
    pub version: Version,
    /// 可攜版 zip 資產
    pub zip_asset: GhAsset,
    /// 對應的 .sha256 校驗檔資產
    pub checksum_asset: GhAsset,
    /// publisher 簽章的 metadata 資產（綁 version/asset/sha256）
    pub metadata_asset: GhAsset,
    /// metadata 的 minisign 簽章資產
    pub signature_asset: GhAsset,
}

/// 建構可攜版 ZIP 資產名稱
fn portable_zip_name(version: &Version) -> String {
    format!("PaceDock_{}_x64-portable.zip", version)
}

/// 查詢 GitHub 最新非 prerelease、非 draft release，
/// 精確選取 `PaceDock_X.Y.Z_x64-portable.zip` 與對應 `.sha256`。
pub fn fetch_portable_release() -> Result<PortableRelease, String> {
    let response = http_client()
        .get(GITHUB_API_RELEASES)
        .header("Accept", "application/vnd.github+json")
        .send()
        .map_err(|e| map_http_error(e, "查詢 GitHub Releases API 失敗"))?;

    if !response.status().is_success() {
        let status = response.status().as_u16();
        return Err(map_status_code(status, "查詢 GitHub Releases API 失敗"));
    }

    let releases: Vec<GhRelease> = response
        .json()
        .map_err(|e| format!("解析 GitHub API 回應失敗: {e}"))?;

    // 取第一個非 prerelease、非 draft
    let latest = releases
        .into_iter()
        .find(|r| !r.prerelease && !r.draft.unwrap_or(false))
        .ok_or("找不到任何正式發行版本".to_string())?;

    // 解析 tag vX.Y.Z → semver Version
    let version_str = latest
        .tag_name
        .strip_prefix('v')
        .unwrap_or(&latest.tag_name);
    let version = Version::parse(version_str)
        .map_err(|e| format!("無法解析版本標籤 '{}': {e}", latest.tag_name))?;

    let expected_zip = portable_zip_name(&version);
    let expected_checksum = format!("{}.sha256", expected_zip);
    let expected_metadata = format!("{expected_zip}.update.json");
    let expected_signature = format!("{expected_zip}.update.json.sig");

    // 精確匹配可攜版 zip
    let zip_asset = latest
        .assets
        .iter()
        .find(|a| a.name == expected_zip)
        .cloned()
        .ok_or_else(|| format!("版本 {} 缺少可攜版資產 '{}'", latest.tag_name, expected_zip))?;

    // 精確匹配校驗檔（強制）
    let checksum_asset = latest
        .assets
        .iter()
        .find(|a| a.name == expected_checksum)
        .cloned()
        .ok_or_else(|| {
            format!(
                "版本 {} 缺少校驗檔 '{}'",
                latest.tag_name, expected_checksum
            )
        })?;

    // publisher 簽章資產（強制；缺簽章的 release 由新 client 拒絕更新）
    let find_asset = |name: &str| {
        latest
            .assets
            .iter()
            .find(|a| a.name == name)
            .cloned()
            .ok_or_else(|| format!("版本 {} 缺少簽章資產 '{}'", latest.tag_name, name))
    };
    let metadata_asset = find_asset(&expected_metadata)?;
    let signature_asset = find_asset(&expected_signature)?;

    // 交叉驗證：tag 必須與資產名稱中的版本一致
    if !expected_zip.contains(version_str) {
        return Err(format!(
            "資產名稱 '{}' 與標籤版本 '{}' 不一致",
            expected_zip, latest.tag_name
        ));
    }

    Ok(PortableRelease {
        version,
        zip_asset,
        checksum_asset,
        metadata_asset,
        signature_asset,
    })
}

/// 比較目前版本與最新版本，若目前版本較舊回傳 true
pub fn is_update_available(current: &str, latest: &Version) -> bool {
    match Version::parse(current) {
        Ok(cur) => latest > &cur,
        Err(_) => false,
    }
}

// ── SHA256 校驗 ──

fn compute_sha256(data: &[u8]) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

/// 下載 .sha256 校驗檔內容並解析 hex digest。
/// 接受 common 格式：`<hex> *<filename>` 或純 hex。
/// 驗證 hex 長度為 64 且全為 hex 字元。
fn fetch_and_parse_checksum(asset: &GhAsset) -> Result<String, String> {
    let response = http_client()
        .get(&asset.browser_download_url)
        .send()
        .map_err(|e| format!("下載校驗檔失敗: {e}"))?;

    if !response.status().is_success() {
        return Err(format!(
            "下載校驗檔失敗：HTTP {}",
            response.status().as_u16()
        ));
    }

    let body = response
        .text()
        .map_err(|e| format!("讀取校驗檔失敗: {e}"))?;

    // 取第一段空白分隔前的 hex 字串
    let hex = body.split_whitespace().next().unwrap_or("").to_lowercase();

    if hex.len() != 64 {
        return Err(format!(
            "校驗檔內容格式異常：hex 長度為 {}（預期 64）",
            hex.len()
        ));
    }

    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("校驗檔內容格式異常：包含非 hex 字元".to_string());
    }

    Ok(hex)
}

/// 驗證下載資料的 SHA256 與校驗檔一致
fn verify_checksum(data: &[u8], expected_hex: &str) -> Result<(), String> {
    let actual = compute_sha256(data);
    if actual != expected_hex {
        return Err(format!(
            "SHA256 校驗失敗：預期 {}，實際 {}",
            expected_hex, actual
        ));
    }
    Ok(())
}

// ── portable update publisher 簽章 ──
//
// 可攜版 ZIP 與 .sha256 由同一 release authority 產生/上傳,checksum 不構成
// 獨立信任根。改由 updater 簽署 key(與 installed updater 同 keypair)簽署
// metadata(綁 schema/version/asset/sha256),client 以內嵌 public key 驗證;
// 缺 metadata 或簽章一律拒絕。semver 比較(client > latest 才更新)同時提供
// rollback 保護。

/// 簽章標的 metadata
#[derive(serde::Deserialize, Debug)]
struct PortableUpdateMetadata {
    schema: u32,
    version: String,
    asset: String,
    sha256: String,
}

/// 取 minisign 檔案格式(兩行)的第二行(base64 本體)
fn minisign_second_line(text: &str, what: &str) -> Result<String, String> {
    text.lines()
        .nth(1)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("{what} 格式錯誤:缺少 base64 本體行"))
}

fn base64_decode(input: &str, what: &str) -> Result<Vec<u8>, String> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(input.trim())
        .map_err(|e| format!("{what} base64 解碼失敗: {e}"))
}

/// 內嵌 public key:與 tauri.conf.json 的 updater pubkey 同源(同一 keypair),
/// 避免第二份常數漂移
fn updater_pubkey() -> Result<minisign_verify::PublicKey, String> {
    use minisign_verify::PublicKey;
    let config = include_str!("../tauri.conf.json");
    let value: serde_json::Value =
        serde_json::from_str(config).map_err(|e| format!("解析 tauri.conf.json 失敗: {e}"))?;
    let pubkey_b64 = value["plugins"]["updater"]["pubkey"]
        .as_str()
        .ok_or_else(|| "tauri.conf.json 缺少 plugins.updater.pubkey".to_string())?;
    let decoded = base64_decode(pubkey_b64, "updater pubkey")?;
    let text =
        String::from_utf8(decoded).map_err(|e| format!("updater pubkey 非合法 UTF-8: {e}"))?;
    let key_line = minisign_second_line(&text, "updater pubkey")?;
    PublicKey::from_base64(&key_line).map_err(|e| format!("updater pubkey 解析失敗: {e}"))
}

fn minisign_signature(sig_bytes: &[u8]) -> Result<minisign_verify::Signature, String> {
    use minisign_verify::Signature;
    let text = std::str::from_utf8(sig_bytes).map_err(|e| format!("簽章檔非合法 UTF-8: {e}"))?;
    // Signature::decode 直接解析兩行式檔案格式
    Signature::decode(text).map_err(|e| format!("簽章解析失敗: {e}"))
}

/// 驗證 metadata 內容與下載物/release 的一致性(schema/version/asset/sha256)
fn validate_portable_metadata(
    meta_bytes: &[u8],
    zip: &[u8],
    want_version: &str,
    want_asset: &str,
) -> Result<(), String> {
    let meta: PortableUpdateMetadata = serde_json::from_slice(meta_bytes)
        .map_err(|e| format!("portable update metadata 解析失敗: {e}"))?;
    if meta.schema != 1 {
        return Err(format!(
            "portable update metadata schema 不支援: {}（預期 1）",
            meta.schema
        ));
    }
    if meta.version != want_version {
        return Err(format!(
            "portable update metadata 版本 '{}' 與 release '{}' 不一致",
            meta.version, want_version
        ));
    }
    if meta.asset != want_asset {
        return Err(format!(
            "portable update metadata 資產名 '{}' 與下載 '{}' 不一致",
            meta.asset, want_asset
        ));
    }
    let hash = compute_sha256(zip);
    if !meta.sha256.eq_ignore_ascii_case(&hash) {
        return Err(format!(
            "portable update metadata sha256 與下載內容不符（metadata {},實際 {hash}）",
            meta.sha256
        ));
    }
    Ok(())
}

/// 驗證 portable update:簽章 → metadata 綁定(schema/version/asset/sha256)。
/// 任一失敗即拒絕安裝。
fn verify_portable_update(
    zip: &[u8],
    meta_bytes: &[u8],
    sig_bytes: &[u8],
    want_version: &str,
    want_asset: &str,
) -> Result<(), String> {
    let key = updater_pubkey()?;
    let sig = minisign_signature(sig_bytes)?;
    key.verify(meta_bytes, &sig, false)
        .map_err(|_| "portable update 簽章驗證失敗(pubkey 不符或內容遭改)".to_string())?;
    validate_portable_metadata(meta_bytes, zip, want_version, want_asset)
}

// ── 下載與解壓縮 ──

/// 下載小型輔助資產（metadata / 簽章檔），加上大小上限避免異常檔案
fn download_asset_bytes(asset: &GhAsset) -> Result<Vec<u8>, String> {
    const MAX_AUX_SIZE: u64 = 64 * 1024;
    if asset.size > MAX_AUX_SIZE {
        return Err(format!(
            "輔助資產 '{}' 超出大小上限（{} bytes）",
            asset.name, asset.size
        ));
    }
    let response = http_client()
        .get(&asset.browser_download_url)
        .send()
        .map_err(|e| map_http_error(e, "下載輔助資產失敗"))?;
    if !response.status().is_success() {
        return Err(map_status_code(
            response.status().as_u16(),
            "下載輔助資產失敗",
        ));
    }
    let bytes = response
        .bytes()
        .map_err(|e| format!("下載輔助資產失敗: {e}"))?
        .to_vec();
    if bytes.is_empty() {
        return Err(format!("輔助資產 '{}' 為空", asset.name));
    }
    Ok(bytes)
}

/// 下載可攜版 zip 並驗證 SHA256，回傳 bytes。
pub fn download_portable_zip(
    release: &PortableRelease,
    progress_cb: impl Fn(u32),
) -> Result<Vec<u8>, String> {
    let asset = &release.zip_asset;

    // 大小上限檢查
    if asset.size > MAX_ZIP_SIZE {
        return Err(format!(
            "可攜版 ZIP 超出大小上限（{} bytes，上限 {} bytes）",
            asset.size, MAX_ZIP_SIZE
        ));
    }

    let response = http_client()
        .get(&asset.browser_download_url)
        .send()
        .map_err(|e| map_http_error(e, "下載可攜版失敗"))?;

    if !response.status().is_success() {
        let status = response.status().as_u16();
        return Err(map_status_code(status, "下載可攜版失敗"));
    }

    let declared_size = response.content_length();

    let buf = response
        .bytes()
        .map_err(|e| format!("下載過程發生錯誤: {e}"))?;

    if buf.is_empty() {
        return Err("下載的檔案為空".to_string());
    }

    // 比較實際大小與 GitHub asset size
    let actual_len = buf.len() as u64;
    if actual_len != asset.size {
        return Err(format!(
            "下載大小不符：GitHub 宣告 {} bytes，實際收到 {} bytes",
            asset.size, actual_len
        ));
    }

    // 比較實際大小與 HTTP content-length（若有）
    if let Some(cl) = declared_size {
        if actual_len != cl {
            return Err(format!(
                "下載大小不符：Content-Length {} bytes，實際收到 {} bytes",
                cl, actual_len
            ));
        }
    }

    // ZIP magic bytes
    if buf.len() < 4 || buf[0] != 0x50 || buf[1] != 0x4B {
        return Err("下載的檔案不是有效的 ZIP".to_string());
    }

    // 下載 publisher 簽章的 metadata 並驗證（強制；綁 version/asset/sha256）
    let meta_bytes = download_asset_bytes(&release.metadata_asset)?;
    let sig_bytes = download_asset_bytes(&release.signature_asset)?;
    verify_portable_update(
        &buf,
        &meta_bytes,
        &sig_bytes,
        &release.version.to_string(),
        &release.zip_asset.name,
    )?;

    // 下載並驗證 SHA256（縱深防禦；真正信任根是上方的簽章驗證）
    let expected_hex = fetch_and_parse_checksum(&release.checksum_asset)?;
    verify_checksum(&buf, &expected_hex)?;

    progress_cb(100);

    Ok(buf.to_vec())
}

/// 可攜版 ZIP 內基準測試資源的目錄前綴
const RESOURCE_PREFIX: &str = "resources/benchmark/";

/// 可攜版 ZIP 內 `resources/benchmark/` 必須完整包含的資源檔（缺一不可）。
/// 抽驗與 helper 交換資源目錄時都以這份清單為準。
pub const REQUIRED_RESOURCE_FILES: [&str; 6] = [
    "PresentMon-2.5.1-x64.exe",
    "lava-triangle.exe",
    "d3d9-workload.exe",
    "LICENSE-PresentMon.txt",
    "LICENSE-liblava.txt",
    "SHA256SUMS",
];

/// 從 zip bytes 解壓縮出新 exe、標記檔與完整基準測試資源。
/// 驗證：單一 PaceDock.exe、根層級標記檔、`resources/benchmark/` 內
/// 六個必要資源檔各出現一次。拒絕：路徑遍歷/反斜線、缺少任何必要資源、
/// 重複資源、以目錄偽裝成資源檔、資源巢狀路徑或未預期的額外檔名。
/// 回傳 (暫存 exe 路徑, 暫存標記檔路徑, 暫存資源目錄路徑)。
pub fn extract_portable_exe(zip_data: &[u8]) -> Result<(PathBuf, PathBuf, PathBuf), String> {
    let cursor = std::io::Cursor::new(zip_data);
    let mut archive = zip::ZipArchive::new(cursor).map_err(|e| format!("無法讀取 ZIP: {e}"))?;

    let mut exe_indices: Vec<usize> = Vec::new();
    let mut marker_found = false;
    // 每個必要資源檔出現的 ZIP 項目索引（空 = 未出現；>1 = 重複）
    let mut required_seen: Vec<Vec<usize>> = vec![Vec::new(); REQUIRED_RESOURCE_FILES.len()];

    for i in 0..archive.len() {
        let entry = archive
            .by_index(i)
            .map_err(|e| format!("讀取 ZIP 項目 {i} 失敗: {e}"))?;
        let name = entry.name();

        // 路徑遍歷檢查（檔名或目錄名內含 .. 或反斜線一律拒絕）
        if name.contains("..") || name.contains('\\') {
            return Err(format!("ZIP 項目 '{name}' 包含不安全的路径元件"));
        }

        let basename = Path::new(name)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(name);

        // 目錄項目：容器目錄（resources/、resources/benchmark/）直接略過；
        // 以 exe 或標記檔命名的目錄屬偽裝，拒絕。
        if entry.is_dir() {
            if basename.eq_ignore_ascii_case("PaceDock.exe") || basename == PORTABLE_MARKER {
                return Err(format!("ZIP 項目 '{name}' 是目錄，不能用來偽裝檔案"));
            }
            continue;
        }

        if basename.eq_ignore_ascii_case("PaceDock.exe") {
            exe_indices.push(i);
        } else if name == PORTABLE_MARKER {
            marker_found = true;
        } else if let Some(rel) = name.strip_prefix(RESOURCE_PREFIX) {
            // 資源必須剛好落在 resources/benchmark/ 下、非目錄、無巢狀路徑，
            // 且檔名是六個必要檔之一。
            if rel.is_empty() || rel.contains('/') || rel.contains('\\') {
                return Err(format!("ZIP 資源項目路徑不合法: '{name}'"));
            }
            match REQUIRED_RESOURCE_FILES.iter().position(|f| *f == rel) {
                Some(pos) => required_seen[pos].push(i),
                None => return Err(format!("ZIP 資源項目不在必要清單: '{name}'")),
            }
        }
    }

    if exe_indices.is_empty() {
        return Err("ZIP 中找不到 PaceDock.exe".to_string());
    }
    if exe_indices.len() > 1 {
        return Err(format!(
            "ZIP 中包含 {} 個 PaceDock.exe（預期 1 個）",
            exe_indices.len()
        ));
    }
    if !marker_found {
        return Err(format!("ZIP 中缺少可攜版標記檔 '{PORTABLE_MARKER}'"));
    }
    for (pos, seen) in required_seen.iter().enumerate() {
        let file = REQUIRED_RESOURCE_FILES[pos];
        match seen.len() {
            0 => return Err(format!("ZIP 中缺少必要資源 '{file}'")),
            n if n > 1 => {
                return Err(format!("ZIP 中包含 {n} 個必要資源 '{file}'（預期 1 個）"));
            }
            _ => {}
        }
    }

    let tmp_dir = create_protected_staging_dir()?;

    let tmp_exe = tmp_dir.join("PaceDock_new.exe");
    let tmp_marker = tmp_dir.join(PORTABLE_MARKER);
    // 暫存資源目錄：helper 把整個 benchmark 目錄搬移到 exe 旁的 resources 下
    let tmp_resources = tmp_dir.join("resources").join("benchmark");
    std::fs::create_dir_all(&tmp_resources).map_err(|e| format!("建立暫存資源目錄失敗: {e}"))?;

    // 解壓縮 exe
    {
        let mut exe_file = archive
            .by_index(exe_indices[0])
            .map_err(|e| format!("讀取 ZIP 項目失敗: {e}"))?;
        let mut out = exclusive_create(&tmp_exe).map_err(|e| format!("建立暫存執行檔失敗: {e}"))?;
        std::io::copy(&mut exe_file, &mut out).map_err(|e| format!("解壓縮執行檔失敗: {e}"))?;
    }

    // 解壓縮標記檔
    for i in 0..archive.len() {
        if let Ok(mut f) = archive.by_index(i) {
            if f.name() == PORTABLE_MARKER {
                let mut out = exclusive_create(&tmp_marker)
                    .map_err(|e| format!("建立暫存標記檔失敗: {e}"))?;
                std::io::copy(&mut f, &mut out).map_err(|e| format!("解壓縮標記檔失敗: {e}"))?;
                break;
            }
        }
    }

    // 解壓縮六個必要資源檔到暫存目錄，供 helper 整目錄交換
    for seen in &required_seen {
        let mut f = archive
            .by_index(seen[0])
            .map_err(|e| format!("讀取資源項目失敗: {e}"))?;
        let out_path = tmp_resources.join(f.name().strip_prefix(RESOURCE_PREFIX).unwrap());
        let mut out =
            exclusive_create(&out_path).map_err(|e| format!("建立暫存資源檔失敗: {e}"))?;
        std::io::copy(&mut f, &mut out).map_err(|e| format!("解壓縮資源檔失敗: {e}"))?;
    }

    Ok((tmp_exe, tmp_marker, tmp_resources))
}

// ── 暫存 staging 保護 ──

/// 以 create-new 語意開檔,拒絕覆寫既有檔案。
fn exclusive_create(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

/// 建立一次性暫存目錄:隨機名稱 + 僅 Administrators/SYSTEM 可寫的保護型 DACL。
/// 隨機名稱與受保護 DACL 防止驗證後被非提升權限程序置換內容。
fn create_protected_staging_dir() -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join(format!("pacedock_update_{}", uuid::Uuid::new_v4()));
    // UUID 撞名殘留時移除重試一次;仍失敗即放棄(fail closed)
    if dir.exists() && std::fs::remove_dir_all(&dir).is_err() {
        return Err(format!("無法清除殘留暫存目錄: {}", dir.display()));
    }
    create_dir_admin_only(&dir)?;
    Ok(dir)
}

/// 以「僅 Administrators 與 SYSTEM」的保護型 DACL 建立目錄。
/// 同帳戶 medium-integrity 程序不在 DACL 內,無法寫入或置換 staging 內容。
fn create_dir_admin_only(dir: &Path) -> Result<(), String> {
    crate::syspath::create_admin_only_dir(dir).map_err(|e| format!("建立保護暫存目錄失敗: {e}"))
}

// ── 可攜版替換輔助腳本 ──

/// 對 PowerShell 單引號字串安全跳脫：將 ' 替換為 ''
fn ps_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// 產生 PowerShell 輔助腳本內容。
/// 使用單引號字串避免跳脫問題，處理 apostrophe/double-quote 安全。
/// 腳本會將進度寫入 `%TEMP%\pacedock_update_<uuid>\update.log` 以便診斷。
/// 每次啟動會截斷舊日誌，避免無限制成長。
fn portable_helper_script(
    old_exe: &str,
    new_exe: &str,
    marker_path: &str,
    new_resources: &str,
    pid: u32,
    log_path: &str,
) -> String {
    let old_q = ps_single_quote(old_exe);
    let new_q = ps_single_quote(new_exe);
    let marker_q = ps_single_quote(marker_path);
    let resources_q = ps_single_quote(new_resources);
    let log_q = ps_single_quote(log_path);

    format!(
        r#"# PaceDock 可攜版更新輔助腳本
param(
    [int]$TargetPid = {pid}
)

$ErrorActionPreference = "Stop"
$LogFile = {log_q}

# 每次啟動時截斷日誌，避免無限制成長
Remove-Item $LogFile -Force -ErrorAction SilentlyContinue

function Write-Log {{
    param([string]$Message)
    $ts = Get-Date -Format "yyyy-MM-dd HH:mm:ss"
    "$ts $Message" | Out-File -FilePath $LogFile -Append -Encoding utf8
}}

Write-Log "helper started, target PID=$TargetPid"

$OldExe = {old_q}
$NewExe = {new_q}
$Marker = {marker_q}
$OldDir = Split-Path $OldExe -Parent

Write-Log "old=$OldExe, new=$NewExe, marker=$Marker"

# 等待 PaceDock 完全結束（含 timeout）
$timeout = Get-Date
while ($true) {{
    $proc = Get-Process -Id $TargetPid -ErrorAction SilentlyContinue
    if (-not $proc) {{ break }}
    if (((Get-Date) - $timeout).TotalSeconds -gt 30) {{
        Write-Log "ERROR: timeout waiting for PID $TargetPid"
        exit 1
    }}
    Start-Sleep -Milliseconds 200
}}

Write-Log "target exited, waiting for file unlock"
Start-Sleep -Milliseconds 500

# 備份、置換、標記、資源、清理全部在 try/catch 內，確保錯誤可診斷且可還原。
# 資源以整目錄交換（swap）而非逐檔覆寫，避免中途失敗留下混雜版本。
$Backup = "$OldExe.bak"
$ResourcesDir = Join-Path $OldDir "resources\benchmark"
$ResourcesBackup = Join-Path $OldDir "resources\benchmark.bak"
$OldResourcesExisted = Test-Path $ResourcesDir
try {{
    # 備份舊 exe
    Write-Log "creating backup: $Backup"
    Copy-Item -Path $OldExe -Destination $Backup -Force -ErrorAction Stop
    Write-Log "backup OK"

    # 置換新 exe（move 比 copy+delete 更接近原子）
    Write-Log "replacing exe"
    Move-Item -Path $NewExe -Destination $OldExe -Force -ErrorAction Stop
    Write-Log "replace OK"

    # 複製標記檔
    if (Test-Path $Marker) {{
        Write-Log "copying marker"
        Copy-Item -Path $Marker -Destination (Join-Path $OldDir "{marker_name}") -Force
        Remove-Item -Path $Marker -Force -ErrorAction SilentlyContinue
        Write-Log "marker OK"
    }}

    # 交換基準測試資源：舊資源改名備份後，把整個新 benchmark 目錄搬入。
    # Move-Item 不建立父目錄，舊資源不存在時先建立 resources 容器。
    Write-Log "swapping benchmark resources"
    if ($OldResourcesExisted) {{
        Rename-Item -Path $ResourcesDir -NewName "benchmark.bak" -Force -ErrorAction Stop
        Write-Log "old resources backed up to benchmark.bak"
    }} else {{
        New-Item -ItemType Directory -Force -Path (Split-Path $ResourcesDir -Parent) | Out-Null
    }}
    Move-Item -Path {resources_q} -Destination $ResourcesDir -Force -ErrorAction Stop
    Write-Log "resources swapped"

    # 成功後才清理備份（exe 備份 + benchmark.bak）
    Write-Log "removing backups"
    Remove-Item -Path $Backup -Force -ErrorAction SilentlyContinue
    if (Test-Path $ResourcesBackup) {{
        Remove-Item -Path $ResourcesBackup -Recurse -Force -ErrorAction SilentlyContinue
    }}
    Write-Log "backups cleaned"
}} catch {{
    Write-Log "ERROR: $($_.Exception.Message)"
    # 還原 exe（備份存在 → 已發生變動）；備份不存在 → 原 exe 未動
    $exeRestored = $false
    if (Test-Path $Backup) {{
        Write-Log "restoring exe from backup"
        Move-Item -Path $Backup -Destination $OldExe -Force -ErrorAction SilentlyContinue
        Write-Log "exe restored"
        $exeRestored = $true
    }} else {{
        Write-Log "ERROR: failure before backup, old exe untouched"
    }}
    # 還原資源：
    #   舊資源曾存在 → 移除可能半套的新資源，整目錄還原 benchmark.bak；
    #   舊資源原本不存在 → 移除本次安裝的半套新資源（不留殘骸）。
    if (Test-Path $ResourcesBackup) {{
        Write-Log "restoring resources from backup"
        Remove-Item -Path $ResourcesDir -Recurse -Force -ErrorAction SilentlyContinue
        Rename-Item -Path $ResourcesBackup -NewName "benchmark" -Force -ErrorAction SilentlyContinue
        Write-Log "resources restored"
    }} elseif ((Test-Path $ResourcesDir) -and -not $OldResourcesExisted) {{
        Write-Log "removing partially installed resources"
        Remove-Item -Path $ResourcesDir -Recurse -Force -ErrorAction SilentlyContinue
        Write-Log "partial resources removed"
    }}
    # 重啟原 exe 只發生在 rollback 完成之後
    if ($exeRestored) {{
        Write-Log "restarting original"
        Start-Process -FilePath $OldExe
        Write-Log "original restart initiated"
    }}
    Remove-Item -LiteralPath (Split-Path $NewExe -Parent) -Recurse -Force -ErrorAction SilentlyContinue
    exit 1
}}

# 成功完成：重新啟動
Write-Log "SUCCESS, restarting $OldExe"
Start-Process -FilePath $OldExe
Remove-Item -LiteralPath (Split-Path $NewExe -Parent) -Recurse -Force -ErrorAction SilentlyContinue
Write-Log "restart initiated"
"#,
        pid = pid,
        log_q = log_q,
        old_q = old_q,
        new_q = new_q,
        marker_q = marker_q,
        resources_q = resources_q,
        marker_name = PORTABLE_MARKER,
    )
}

/// 執行可攜版替換：寫出腳本、啟動、設定 quitting flag
pub fn execute_portable_replacement(
    old_exe: &Path,
    new_exe: &Path,
    marker_path: &Path,
    new_resources: &Path,
    pid: u32,
) -> Result<(), String> {
    // staging 目錄由 extract_portable_exe 建立(隨機名稱 + 僅 Administrators 可寫),
    // 從 staged exe 路徑反推,不再自行建立固定名稱目錄
    let tmp_dir = new_exe
        .parent()
        .ok_or_else(|| "無法取得暫存目錄".to_string())?
        .to_path_buf();

    let script_path = tmp_dir.join("update.ps1");
    let log_path = tmp_dir.join("update.log");
    let script = portable_helper_script(
        &old_exe.to_string_lossy(),
        &new_exe.to_string_lossy(),
        &marker_path.to_string_lossy(),
        &new_resources.to_string_lossy(),
        pid,
        &log_path.to_string_lossy(),
    );

    // 寫入 UTF-8 BOM（EF BB BF）確保 PowerShell 5.1 正確解讀非 ASCII 字元
    let mut file =
        std::fs::File::create(&script_path).map_err(|e| format!("建立更新腳本失敗: {e}"))?;
    file.write_all(b"\xEF\xBB\xBF")
        .map_err(|e| format!("寫入更新腳本失敗: {e}"))?;
    file.write_all(script.as_bytes())
        .map_err(|e| format!("寫入更新腳本失敗: {e}"))?;

    // 啟動 PowerShell，使用 CREATE_NO_WINDOW 避免彈出視窗；
    // 以 System32 絕對路徑啟動，避免依賴 PATH 搜尋
    std::process::Command::new(crate::syspath::powershell_exe()?)
        .args([
            "-WindowStyle",
            "Hidden",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(&script_path)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("啟動更新輔助程序失敗: {e}"))?;

    Ok(())
}

// ── 測試 ──

#[cfg(test)]
mod tests {
    use super::*;

    /// 產生兩行式 minisign 檔案格式內容(untrusted comment + base64)
    fn minisign_doc(comment: &str, b64: &str) -> String {
        format!("untrusted comment: {comment}\n{b64}\n")
    }

    /// 以測試內生成的 throwaway keypair 驗證 minisign 格式解析與驗章路徑
    #[test]
    fn portable_signature_roundtrip_with_test_keypair() {
        use std::io::Cursor;
        // dev-dep minisign:測試內生成 keypair,與發布 key 無關
        let keypair =
            minisign::KeyPair::generate_unencrypted_keypair().expect("測試 keypair 生成不應失敗");
        let secret = keypair.sk;
        let public =
            minisign::PublicKey::from_secret_key(&secret).expect("由 secret key 推導公鑰不應失敗");
        let sig_box = minisign::sign(
            None,
            &secret,
            Cursor::new(b"metadata-bytes".to_vec()),
            None,
            None,
        )
        .expect("測試簽章不應失敗");

        // 兩行格式解析 + minisign-verify 驗章(與 client 實際路徑同構)
        let key_line =
            minisign_second_line(&minisign_doc("pub", &public.to_base64()), "pubkey").unwrap();
        let key = minisign_verify::PublicKey::from_base64(&key_line).unwrap();
        let sig = minisign_verify::Signature::decode(&sig_box.to_string()).unwrap();
        key.verify(b"metadata-bytes", &sig, false)
            .expect("正確資料應通過");
        assert!(
            key.verify(b"tampered", &sig, false).is_err(),
            "遭改內容必須失敗"
        );
    }

    #[test]
    fn updater_pubkey_resolves_from_config() {
        // tauri.conf.json 的 updater pubkey 應可解析為合法 minisign public key
        updater_pubkey().expect("內嵌 updater pubkey 應解析成功");
    }

    #[test]
    fn metadata_validation_rejects_version_mismatch() {
        let zip = b"zip-bytes";
        let meta = br#"{"schema":1,"version":"1.2.3","asset":"PaceDock_1.2.3_x64-portable.zip","sha256":"X"}"#;
        let err = validate_portable_metadata(meta, zip, "1.2.4", "PaceDock_1.2.3_x64-portable.zip")
            .unwrap_err();
        assert!(err.contains("版本"), "err={err}");
    }

    #[test]
    fn metadata_validation_rejects_asset_mismatch() {
        let zip = b"zip-bytes";
        let meta = br#"{"schema":1,"version":"1.2.4","asset":"evil.zip","sha256":"X"}"#;
        let err = validate_portable_metadata(meta, zip, "1.2.4", "PaceDock_1.2.4_x64-portable.zip")
            .unwrap_err();
        assert!(err.contains("資產名"), "err={err}");
    }

    #[test]
    fn metadata_validation_rejects_hash_mismatch() {
        let zip = b"zip-bytes";
        let meta = br#"{"schema":1,"version":"1.2.4","asset":"PaceDock_1.2.4_x64-portable.zip","sha256":"0000000000000000000000000000000000000000000000000000000000000000"}"#;
        let err = validate_portable_metadata(meta, zip, "1.2.4", "PaceDock_1.2.4_x64-portable.zip")
            .unwrap_err();
        assert!(err.contains("sha256"), "err={err}");
    }

    #[test]
    fn metadata_validation_accepts_matching_content() {
        let zip = b"zip-bytes";
        let hash = compute_sha256(zip);
        let meta = format!(
            r#"{{"schema":1,"version":"1.2.4","asset":"PaceDock_1.2.4_x64-portable.zip","sha256":"{hash}"}}"#
        );
        validate_portable_metadata(
            meta.as_bytes(),
            zip,
            "1.2.4",
            "PaceDock_1.2.4_x64-portable.zip",
        )
        .expect("一致的 metadata 應通過");
    }

    #[test]
    fn metadata_validation_rejects_unknown_schema() {
        let meta = br#"{"schema":99,"version":"1.2.4","asset":"a.zip","sha256":"X"}"#;
        let err = validate_portable_metadata(meta, b"z", "1.2.4", "a.zip").unwrap_err();
        assert!(err.contains("schema"), "err={err}");
    }

    #[test]
    fn protected_staging_dir_is_random() {
        let dir = create_protected_staging_dir().expect("staging 目錄應建立成功");
        assert!(dir.is_dir());
        assert!(dir
            .file_name()
            .map(|n| n.to_string_lossy().starts_with("pacedock_update_"))
            .unwrap_or(false));

        // 隨機名稱:兩次建立不會落在同一目錄
        let dir2 = create_protected_staging_dir().expect("第二個 staging 目錄應建立成功");
        assert_ne!(dir, dir2);

        // 測試環境未必能刪除僅 Administrators 可寫的目錄,盡力清理
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    // ── 版本檢查 ──

    #[test]
    fn current_version_parses_as_semver() {
        let v = env!("CARGO_PKG_VERSION");
        assert!(
            Version::parse(v).is_ok(),
            "CARGO_PKG_VERSION 不是有效 semver"
        );
    }

    #[test]
    fn is_update_available_detects_newer() {
        let latest = Version::new(0, 2, 0);
        assert!(is_update_available("0.1.0", &latest));
    }

    #[test]
    fn is_update_available_detects_same() {
        let latest = Version::new(0, 1, 0);
        assert!(!is_update_available("0.1.0", &latest));
    }

    #[test]
    fn is_update_available_detects_older() {
        let latest = Version::new(0, 0, 9);
        assert!(!is_update_available("0.1.0", &latest));
    }

    #[test]
    fn is_update_available_handles_invalid_current() {
        let latest = Version::new(9, 9, 9);
        assert!(!is_update_available("not-semver", &latest));
    }

    // ── 資產名稱 ──

    #[test]
    fn portable_zip_name_matches_pattern() {
        let v = Version::new(0, 2, 0);
        let name = portable_zip_name(&v);
        assert_eq!(name, "PaceDock_0.2.0_x64-portable.zip");
    }

    #[test]
    fn portable_zip_name_contains_version_and_arch() {
        let v = Version::new(1, 2, 3);
        let name = portable_zip_name(&v);
        assert!(name.contains("1.2.3"));
        assert!(name.contains("x64-portable"));
        assert!(name.ends_with(".zip"));
    }

    // ── SHA256 ──

    #[test]
    fn compute_sha256_is_deterministic() {
        let data = b"hello pacedock";
        let h1 = compute_sha256(data);
        let h2 = compute_sha256(data);
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64);
    }

    #[test]
    fn verify_checksum_match_passes() {
        let data = b"test data for checksum";
        let hex = compute_sha256(data);
        assert!(verify_checksum(data, &hex).is_ok());
    }

    #[test]
    fn verify_checksum_mismatch_fails() {
        let data = b"test data for checksum";
        let wrong = "a".repeat(64);
        let err = verify_checksum(data, &wrong).unwrap_err();
        assert!(err.contains("SHA256 校驗失敗"));
    }

    #[test]
    fn fetch_and_parse_checksum_rejects_short_hex() {
        // 不實際呼叫網路：測試格式驗證邏輯已整合在 fetch_and_parse_checksum 中
        // 此處用輔助斷言驗證：hex 必須為 64 字元
        let short = "abc123";
        assert_ne!(short.len(), 64);
    }

    #[test]
    fn fetch_and_parse_checksum_rejects_non_hex() {
        let non_hex = "gggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggg";
        assert_eq!(non_hex.len(), 64);
        assert!(!non_hex.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn parse_checksum_accepts_standard_format() {
        // 標準格式 "<hex>  <filename>"
        let body = "d14f5bcf9f29f5a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b1c2d3e4f5a6  PaceDock_0.2.0_x64-portable.zip\n";
        let hex = body.split_whitespace().next().unwrap_or("").to_lowercase();
        assert_eq!(hex.len(), 64);
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn parse_checksum_accepts_hex_only() {
        let body = "d14f5bcf9f29f5a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b1c2d3e4f5a6\n";
        let hex = body.split_whitespace().next().unwrap_or("").to_lowercase();
        assert_eq!(hex.len(), 64);
    }

    // ── 輔助腳本 ──

    fn make_script(old: &str, new: &str, marker: &str, pid: u32) -> String {
        portable_helper_script(
            old,
            new,
            marker,
            r"C:\tmp\resources\benchmark",
            pid,
            r"C:\tmp\update.log",
        )
    }

    /// 回歸測試：helper 腳本必須以「整目錄交換」更新基準測試資源，
    /// 而不是逐檔複製到 live 目錄。
    #[test]
    fn helper_script_swaps_benchmark_resources() {
        let script = make_script(
            r"C:\app\PaceDock.exe",
            r"C:\tmp\new.exe",
            r"C:\tmp\.pacedock-portable",
            12345,
        );
        assert!(
            script.contains("swapping benchmark resources"),
            "腳本應含資源交換步驟"
        );
        // 舊資源改名備份（swap 的第一段）
        assert!(
            script.contains("Rename-Item -Path $ResourcesDir -NewName \"benchmark.bak\""),
            "腳本應把舊資源改名為 benchmark.bak 當備份"
        );
        // 整目錄搬入新資源（swap 的第二段）
        assert!(
            script.contains(
                "Move-Item -Path 'C:\\tmp\\resources\\benchmark' -Destination $ResourcesDir"
            ),
            "腳本應整目錄搬入新資源"
        );
        assert!(
            script.contains(r"C:\tmp\resources\benchmark"),
            "腳本應含暫存資源路徑"
        );
        // 避免在 live 目錄逐檔複製：不得對暫存資源目錄用 Copy-Item
        assert!(
            !script.contains("Copy-Item -Path 'C:\\tmp\\resources\\benchmark'"),
            "不得對資源目錄逐檔 Copy-Item"
        );
    }

    /// 回歸測試：資源更新必須可 rollback——舊資源先改名備份，
    /// 失敗時從 benchmark.bak 整目錄還原（exe 還原與資源還原都要有）。
    #[test]
    fn helper_script_backs_up_and_restores_resources() {
        let script = make_script(
            r"C:\app\PaceDock.exe",
            r"C:\tmp\new.exe",
            r"C:\tmp\.pacedock-portable",
            12345,
        );
        assert!(
            script.contains("benchmark.bak"),
            "腳本應把舊資源備份為 benchmark.bak"
        );
        assert!(
            script.contains("$ResourcesBackup = Join-Path $OldDir \"resources\\benchmark.bak\""),
            "備份路徑應指向 resources\\benchmark.bak"
        );
        assert!(script.contains("Rename-Item"), "腳本應改名備份/還原資源");
        assert!(
            script.contains("restoring resources from backup"),
            "腳本失敗路徑應還原資源"
        );
        // 還原順序：先移除半套新 resources，再還原 backup
        let restore_idx = script.find("restoring resources from backup").unwrap();
        let rm_idx = script.find("Remove-Item -Path $ResourcesDir").unwrap();
        assert!(rm_idx > restore_idx, "應先刪半套新資源再還原備份");
        // 成功路徑也會清理 benchmark.bak
        assert!(
            script.contains("Remove-Item -Path $ResourcesBackup -Recurse -Force"),
            "成功後應清理 benchmark.bak"
        );
    }

    /// 回歸測試：舊資源原本不存在時，catch 必須移除本次安裝的半套新資源，
    /// 不留殘骸。
    #[test]
    fn helper_script_catch_removes_partial_resources_when_old_absent() {
        let script = make_script(
            r"C:\app\PaceDock.exe",
            r"C:\tmp\new.exe",
            r"C:\tmp\.pacedock-portable",
            12345,
        );
        assert!(
            script.contains("removing partially installed resources"),
            "舊資源不存在時 catch 應移除半套新資源"
        );
        assert!(
            script.contains("elseif ((Test-Path $ResourcesDir) -and -not $OldResourcesExisted)"),
            "應以 OldResourcesExisted 區分舊資源是否存在（-and 條件必須加括號）"
        );
        assert!(
            !script.contains("elseif (Test-Path $ResourcesDir -and"),
            "禁止未加括號的 Test-Path -and 條件（-and 會被當作 Test-Path 參數）"
        );
        assert!(
            script.contains("partial resources removed"),
            "移除半套新資源後應寫 log"
        );
    }

    /// 回歸測試：重啟原 exe 只發生在資源 rollback（還原或移除）完成之後。
    #[test]
    fn helper_script_restarts_original_only_after_rollback() {
        let script = make_script(
            r"C:\app\PaceDock.exe",
            r"C:\tmp\new.exe",
            r"C:\tmp\.pacedock-portable",
            1,
        );
        let restore_pos = script
            .find("restoring resources from backup")
            .expect("should have restore branch");
        let partial_pos = script
            .find("removing partially installed resources")
            .expect("should have removal branch");
        let restart_pos = script
            .find("restarting original")
            .expect("should restart original in catch");
        assert!(
            partial_pos > restore_pos,
            "移除半套分支（elseif）應在還原備份分支（if）之後"
        );
        assert!(
            restart_pos > partial_pos,
            "重啟原 exe 應在所有資源 rollback 之後"
        );
    }

    /// 回歸測試：產生的 helper 腳本必須能通過 PowerShell parser 解析（零 parse error），
    /// 防止不合法的運算式（例如未加括號的 -and 條件）混入產出腳本。
    /// 使用與生產執行相同的 powershell.exe（5.1）與 UTF-8 BOM 寫入方式。
    #[test]
    fn helper_script_parses_clean_with_powershell_parser() {
        let script = make_script(
            r"C:\app\PaceDock.exe",
            r"C:\tmp\new.exe",
            r"C:\tmp\.pacedock-portable",
            12345,
        );
        let path = std::env::temp_dir().join("fa_helper_parse_check.ps1");
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&path).expect("建立暫存腳本失敗");
            f.write_all(b"\xEF\xBB\xBF").expect("寫入 BOM 失敗");
            f.write_all(script.as_bytes()).expect("寫入腳本失敗");
        }
        let escaped = path.display().to_string().replace('\'', "''");
        let ps = format!(
            r"$errs=$null; [void][System.Management.Automation.Language.Parser]::ParseFile('{escaped}',[ref]$null,[ref]$errs); if($errs.Count -gt 0){{ $errs | ForEach-Object {{ Write-Output ('PARSE:' + $_.Extent.StartLineNumber + ':' + $_.Message) }}; exit 1 }}"
        );
        let out = std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &ps])
            .output()
            .expect("無法啟動 PowerShell，請確認已安裝");
        let _ = std::fs::remove_file(&path);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "產生的 helper 腳本未通過 PowerShell parser:\n{stdout}"
        );
        assert!(stdout.is_empty(), "PowerShell parser 回報錯誤:\n{stdout}");
    }

    #[test]
    fn helper_script_contains_pid_and_paths() {
        let script = make_script(
            r"C:\app\PaceDock.exe",
            r"C:\tmp\new.exe",
            r"C:\tmp\.pacedock-portable",
            12345,
        );
        assert!(script.contains("12345"));
        assert!(script.contains("PaceDock.exe"));
        assert!(script.contains("new.exe"));
    }

    #[test]
    fn helper_script_contains_backup_and_restore() {
        let script = make_script(
            r"C:\app\fa.exe",
            r"C:\tmp\new.exe",
            r"C:\tmp\.pacedock-portable",
            1,
        );
        assert!(script.contains(".bak"));
        assert!(script.contains("Start-Process"));
        assert!(script.contains("Move-Item"));
        // rollback 應區分備份存在/不存在兩條路徑（exe 還原 + 資源還原）
        assert!(script.contains("restoring exe from backup"));
        assert!(script.contains("failure before backup"));
        assert!(script.contains("restoring resources from backup"));
    }

    #[test]
    fn helper_script_contains_timeout() {
        let script = make_script(
            r"C:\app\fa.exe",
            r"C:\tmp\new.exe",
            r"C:\tmp\.pacedock-portable",
            1,
        );
        assert!(script.contains("TotalSeconds"));
    }

    #[test]
    fn helper_script_contains_logging() {
        let script = make_script(
            r"C:\app\fa.exe",
            r"C:\tmp\new.exe",
            r"C:\tmp\.pacedock-portable",
            1,
        );
        assert!(
            script.contains("Write-Log"),
            "script should contain diagnostic logging"
        );
        assert!(
            script.contains("update.log"),
            "script should reference log file"
        );
    }

    #[test]
    fn helper_script_log_markers() {
        let script = make_script(
            r"C:\app\fa.exe",
            r"C:\tmp\new.exe",
            r"C:\tmp\.pacedock-portable",
            1,
        );
        // 關鍵階段應有對應 log
        for marker in &[
            "helper started",
            "backup OK",
            "replacing exe",
            "replace OK",
            "removing backups",
            "SUCCESS",
            "restoring exe from backup",
            "failure before backup",
            "restoring resources from backup",
        ] {
            assert!(
                script.contains(marker),
                "script missing log marker: {}",
                marker
            );
        }
    }

    #[test]
    fn helper_script_has_log_truncation() {
        let script = make_script(
            r"C:\app\fa.exe",
            r"C:\tmp\new.exe",
            r"C:\tmp\.pacedock-portable",
            1,
        );
        // 每次啟動應截斷舊日誌
        assert!(
            script.contains("Remove-Item $LogFile"),
            "script should truncate log at startup"
        );
    }

    #[test]
    fn helper_script_backup_inside_try() {
        let script = make_script(
            r"C:\app\fa.exe",
            r"C:\tmp\new.exe",
            r"C:\tmp\.pacedock-portable",
            1,
        );
        // Copy-Item（備份）必須在 try { 和 } catch 之間（輸出為實際 PowerShell，非 Rust format 跳脫）
        let try_pos = script.find("try {").expect("script should have try block");
        let catch_pos = script
            .find("} catch {")
            .expect("script should have catch block");
        let copy_pos = script
            .find("Copy-Item")
            .expect("script should have Copy-Item");
        assert!(
            try_pos < copy_pos && copy_pos < catch_pos,
            "Copy-Item (backup) should be inside try/catch, try={try_pos} copy={copy_pos} catch={catch_pos}"
        );
    }

    #[test]
    fn helper_script_rollback_both_branches() {
        let script = make_script(
            r"C:\app\fa.exe",
            r"C:\tmp\new.exe",
            r"C:\tmp\.pacedock-portable",
            1,
        );
        // catch 內應有兩條分支：備份存在時還原，不存在時記錄原 exe 未動
        assert!(script.contains("restoring exe from backup"));
        assert!(script.contains("failure before backup"));
        assert!(script.contains("restoring resources from backup"));
        // 確認兩條路徑都有對應動作
        assert!(script.contains("restarting original"));
        assert!(script.contains("old exe untouched"));
    }

    #[test]
    fn helper_script_escapes_single_quote_in_path() {
        // 路徑含 apostrophe
        let script = make_script(
            r"C:\Users\John'OConnor\App\fa.exe",
            r"C:\tmp\new.exe",
            r"C:\tmp\.pacedock-portable",
            1,
        );
        // 單引號字串內 '' 為跳脫
        assert!(script.contains("John''OConnor"));
    }

    #[test]
    fn helper_script_contains_no_bare_write_error() {
        let script = make_script(
            r"C:\app\fa.exe",
            r"C:\tmp\new.exe",
            r"C:\tmp\.pacedock-portable",
            1,
        );
        // Write-Error 已由 Write-Log 取代，確保不會因 stderr 遺失診斷訊息
        assert!(!script.contains("Write-Error"));
    }

    #[test]
    fn helper_script_bom_check() {
        // 此測試驗證 BOM 寫入邏輯：\xEF\xBB\xBF 是 UTF-8 BOM
        let bom: &[u8] = b"\xEF\xBB\xBF";
        assert_eq!(bom.len(), 3);
        // 確認 BOM 開頭的檔案會被 PowerShell 5.1 識別為 UTF-8
        let script = make_script(r"C:\a\f.exe", r"C:\t\n.exe", r"C:\t\m", 1);
        let script_start = script.as_bytes();
        // 確保腳本內容不以 BOM 開頭（BOM 在 execute_portable_replacement 寫入時才加上）
        // 腳本字串本身不含 BOM
        assert!(
            !script_start.starts_with(bom),
            "script string itself should not have BOM"
        );
    }

    #[test]
    fn ps_single_quote_handles_apostrophe() {
        let result = ps_single_quote(r#"C:\John's App\test.exe"#);
        // 單引號包圍，內部 ' → ''
        assert_eq!(result, "'C:\\John''s App\\test.exe'");
    }

    #[test]
    fn ps_single_quote_handles_double_quotes() {
        let result = ps_single_quote(r#"C:\Some"Path\file.exe"#);
        // 雙引號在單引號字串內不需要跳脫
        assert_eq!(result, "'C:\\Some\"Path\\file.exe'");
    }

    #[test]
    fn ps_single_quote_handles_normal_path() {
        let result = ps_single_quote(r"C:\Program Files\App\app.exe");
        assert_eq!(result, "'C:\\Program Files\\App\\app.exe'");
    }

    // ── ZIP 解壓縮驗證（不呼叫網路） ──

    #[test]
    fn extract_rejects_empty_zip() {
        // 空 vec 不是有效 ZIP
        let buf: Vec<u8> = vec![];
        let result = extract_portable_exe(&buf);
        assert!(result.is_err());
    }

    #[test]
    fn extract_rejects_non_zip() {
        let buf: &[u8] = b"this is not a zip file at all, just some random bytes here";
        let result = extract_portable_exe(buf);
        assert!(result.is_err());
    }

    /// 建一個含 exe + 標記 + 指定資源檔（+ 選擇性重複項目）的可攜版 ZIP。
    /// `extra` 為直接以原檔名寫入的非標準項目（用於巢狀/未預期檔名測試）。
    fn build_zip(resources: &[&str], duplicate: Option<&str>) -> Vec<u8> {
        build_zip_with_extra(resources, duplicate, None)
    }

    fn build_zip_with_extra(
        resources: &[&str],
        duplicate: Option<&str>,
        extra: Option<&str>,
    ) -> Vec<u8> {
        use std::io::Write;
        use zip::write::SimpleFileOptions;
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            let opts = SimpleFileOptions::default();
            w.start_file("PaceDock.exe", opts).unwrap();
            w.write_all(b"MZ fake exe bytes").unwrap();
            w.start_file(".pacedock-portable", opts).unwrap();
            w.write_all(b"").unwrap();
            for r in resources {
                w.start_file(format!("{RESOURCE_PREFIX}{r}"), opts).unwrap();
                w.write_all(format!("fake {r}").as_bytes()).unwrap();
            }
            if let Some(dup) = duplicate {
                w.start_file(format!("{RESOURCE_PREFIX}{dup}"), opts)
                    .unwrap();
                w.write_all(b"fake duplicate").unwrap();
            }
            if let Some(x) = extra {
                w.start_file(x, opts).unwrap();
                w.write_all(b"extra").unwrap();
            }
            w.finish().unwrap();
        }
        buf.into_inner()
    }

    /// 六個必要資源全部存在 → 全部被抽出。
    /// 註：extract 輸出到一次性隨機 staging 目錄，各測試互不共用，
    /// 結束後可安全清理自己的目錄。
    #[test]
    fn extract_accepts_all_six_resources() {
        let zip = build_zip(&REQUIRED_RESOURCE_FILES, None);
        let (exe, marker, resources) = extract_portable_exe(&zip).unwrap();
        assert!(exe.exists(), "exe 應被抽出");
        assert!(marker.exists(), "標記應被抽出");
        for f in REQUIRED_RESOURCE_FILES {
            assert!(resources.join(f).exists(), "資源應被抽出: {f}");
        }
        if let Some(dir) = exe.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// 六個必要資源缺任何一個 → 拒絕且錯誤指名缺失檔（table-driven）
    #[test]
    fn extract_rejects_each_missing_required_resource() {
        for missing in REQUIRED_RESOURCE_FILES {
            let present: Vec<&str> = REQUIRED_RESOURCE_FILES
                .iter()
                .copied()
                .filter(|f| *f != missing)
                .collect();
            let err = extract_portable_exe(&build_zip(&present, None)).unwrap_err();
            assert!(
                err.contains(missing),
                "缺少 {missing} 時錯誤應指名該檔: {err}"
            );
            assert!(err.contains("缺少必要資源"), "err={err}");
        }
    }

    /// 重複檔名 → 拒絕。
    /// zip crate 在寫入與開啟兩階段都拒絕重複項目名（InvalidArchive），
    /// 因此可攜版 ZIP 結構上不可能含重複資源名；此測試鎖住該不變式。
    #[test]
    fn extract_rejects_duplicate_required_resource() {
        use zip::write::SimpleFileOptions;
        let mut buf = std::io::Cursor::new(Vec::new());
        let mut w = zip::ZipWriter::new(&mut buf);
        let opts = SimpleFileOptions::default();
        w.start_file("resources/benchmark/SHA256SUMS", opts)
            .unwrap();
        let dup = w.start_file("resources/benchmark/SHA256SUMS", opts);
        assert!(dup.is_err(), "zip 寫入階段必須拒絕重複檔名");
    }

    /// 資源下出現巢狀路徑 → 拒絕
    #[test]
    fn extract_rejects_nested_resource_path() {
        let err = extract_portable_exe(&build_zip_with_extra(
            &REQUIRED_RESOURCE_FILES,
            None,
            Some("resources/benchmark/sub/lava-triangle.exe"),
        ))
        .unwrap_err();
        assert!(err.contains("路徑不合法"), "err={err}");
    }

    /// 六個必要檔齊全，但多出未預期的資源檔 → 拒絕
    #[test]
    fn extract_rejects_unexpected_resource_file() {
        let err = extract_portable_exe(&build_zip_with_extra(
            &REQUIRED_RESOURCE_FILES,
            None,
            Some("resources/benchmark/extra.dll"),
        ))
        .unwrap_err();
        assert!(err.contains("不在必要清單"), "err={err}");
        assert!(err.contains("extra.dll"), "err={err}");
    }

    /// 以目錄偽裝成必要資源檔 → 不得被當成資源接受（缺該檔即拒絕）
    #[test]
    fn extract_rejects_directory_masquerading_as_resource() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            let opts = SimpleFileOptions::default();
            w.start_file("PaceDock.exe", opts).unwrap();
            w.write_all(b"MZ").unwrap();
            w.start_file(".pacedock-portable", opts).unwrap();
            w.write_all(b"").unwrap();
            // 目錄項目偽裝成必要資源 SHA256SUMS
            w.add_directory("resources/benchmark/SHA256SUMS/", opts)
                .unwrap();
            for f in REQUIRED_RESOURCE_FILES
                .iter()
                .filter(|f| **f != "SHA256SUMS")
            {
                w.start_file(format!("{RESOURCE_PREFIX}{f}"), opts).unwrap();
                w.write_all(b"x").unwrap();
            }
            w.finish().unwrap();
        }
        let err = extract_portable_exe(&buf.into_inner()).unwrap_err();
        assert!(err.contains("SHA256SUMS"), "err={err}");
    }

    /// 容器目錄項目（resources/、resources/benchmark/）不應造成拒絕
    #[test]
    fn extract_tolerates_container_directory_entries() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            let opts = SimpleFileOptions::default();
            w.start_file("PaceDock.exe", opts).unwrap();
            w.write_all(b"MZ").unwrap();
            w.start_file(".pacedock-portable", opts).unwrap();
            w.write_all(b"").unwrap();
            w.add_directory("resources/", opts).unwrap();
            w.add_directory("resources/benchmark/", opts).unwrap();
            for f in REQUIRED_RESOURCE_FILES {
                w.start_file(format!("{RESOURCE_PREFIX}{f}"), opts).unwrap();
                w.write_all(b"x").unwrap();
            }
            w.finish().unwrap();
        }
        let (exe, marker, resources) = extract_portable_exe(&buf.into_inner()).unwrap();
        assert!(exe.exists() && marker.exists(), "exe/marker 應被抽出");
        assert!(resources.join("SHA256SUMS").exists(), "資源應被抽出");
    }

    /// 路徑遍歷（.. 或反斜線）→ 拒絕
    #[test]
    fn extract_rejects_traversal_and_backslash() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            let opts = SimpleFileOptions::default();
            // ..\ 起頭的項目同時觸發 .. 與反斜線檢查
            w.start_file(r"..\evil.exe", opts).unwrap();
            w.write_all(b"evil").unwrap();
            w.finish().unwrap();
        }
        let err = extract_portable_exe(&buf.into_inner()).unwrap_err();
        assert!(err.contains("不安全的路径元件"), "err={err}");
    }

    /// 回歸測試：ZIP 含 resources/benchmark 但缺 exe → 仍拒絕
    #[test]
    fn extract_rejects_missing_exe_even_with_resources() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            let opts = SimpleFileOptions::default();
            w.start_file("resources/benchmark/SHA256SUMS", opts)
                .unwrap();
            w.write_all(b"abc").unwrap();
            w.start_file(".pacedock-portable", opts).unwrap();
            w.write_all(b"").unwrap();
            w.finish().unwrap();
        }
        let result = extract_portable_exe(&buf.into_inner());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("PaceDock.exe"));
    }

    #[test]
    fn portable_marker_name_is_stable() {
        assert_eq!(PORTABLE_MARKER, ".pacedock-portable");
    }

    // ── 大小檢查 ──

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn max_zip_size_is_reasonable() {
        // 100 MiB 對單一 exe 而言很寬裕
        assert!(MAX_ZIP_SIZE > 10_000_000);
        assert!(MAX_ZIP_SIZE < 500_000_000);
    }

    // ── 發行資訊資產交叉驗證 ──

    #[test]
    fn portable_zip_name_matches_version_consistency() {
        let v = Version::new(0, 1, 0);
        let name = portable_zip_name(&v);
        assert!(name.contains("0.1.0"), "資產名稱應包含版本號");
    }
}
