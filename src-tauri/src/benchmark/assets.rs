//! 基準測試資源驗證(trust root 內嵌主程式):
//! PresentMon 與 liblava Vulkan workload 的期望 SHA-256 由 build.rs 從
//! `resources/benchmark/SHA256SUMS` 生成並內嵌(`BUILTIN_DIGESTS`);
//! d3d9-workload.exe 使用 build 時計算的 per-build digest(`D3D9_WORKLOAD_DIGEST`,
//! 開發流程未編譯時為 None,退回存在檢查)。
//! 資源樹中的 `SHA256SUMS` 與待驗證檔共置且可寫,不構成 trust root;
//! runtime 一律以內嵌 digest 驗證,manifest 僅供 `npm run verify:benchmark-assets`
//! 等開發流程使用。

use std::path::{Path, PathBuf};

include!(concat!(env!("OUT_DIR"), "/benchmark_digests.rs"));

/// 內建資源目錄底下的相對檔名
pub const PRESENTMON_FILE: &str = "PresentMon-2.5.1-x64.exe";
pub const VULKAN_WORKLOAD_FILE: &str = "lava-triangle.exe";
pub const D3D9_WORKLOAD_FILE: &str = "d3d9-workload.exe";

/// 執行一個基準測試所需的全部外部工具路徑
#[derive(Clone, Debug)]
pub struct BenchmarkAssets {
    pub presentmon: PathBuf,
    pub vulkan_workload: PathBuf,
    pub d3d9_workload: PathBuf,
}

/// 依資源目錄組出資產路徑（檔案可尚不存在；`verify` 負責檢查）
pub fn load(dir: &Path) -> BenchmarkAssets {
    BenchmarkAssets {
        presentmon: dir.join(PRESENTMON_FILE),
        vulkan_workload: dir.join(VULKAN_WORKLOAD_FILE),
        d3d9_workload: dir.join(D3D9_WORKLOAD_FILE),
    }
}

/// 資源驗證錯誤：帶穩定代碼（前端查 i18n）+ 詳細訊息
#[derive(Debug)]
pub enum AssetError {
    /// 內嵌 digest 清單中的檔案缺失
    Missing(String),
    /// 檔案 hash 與內嵌 digest 不符
    HashMismatch(String),
    /// d3d9 workload 缺失
    MissingD3D9(String),
}

impl std::fmt::Display for AssetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AssetError::Missing(file) => write!(f, "資源缺失: {file}"),
            AssetError::HashMismatch(file) => write!(f, "資源 hash 不符: {file}"),
            AssetError::MissingD3D9(file) => {
                write!(f, "資源缺失: {file}（請先 npm run build:benchmark-assets）")
            }
        }
    }
}

impl AssetError {
    pub fn code(&self) -> &'static str {
        match self {
            AssetError::HashMismatch(_) => crate::error::codes::BENCHMARK_ASSETS_HASH_MISMATCH,
            _ => crate::error::codes::BENCHMARK_ASSETS_MISSING,
        }
    }
}

/// 驗證所有資源：每個內嵌 digest 對應的檔案必須存在且 sha256 相符。
/// d3d9 workload 有 per-build digest 時一併比對,否則僅要求存在(開發建置)。
pub fn verify(assets: &BenchmarkAssets) -> Result<(), AssetError> {
    let dir = assets
        .presentmon
        .parent()
        .unwrap_or_else(|| Path::new("."));
    for (file, want_hash) in BUILTIN_DIGESTS {
        check_hash(&dir.join(file), want_hash)?;
    }
    match D3D9_WORKLOAD_DIGEST {
        Some(want_hash) => check_hash(&assets.d3d9_workload, want_hash)?,
        None => {
            if !assets.d3d9_workload.exists() {
                return Err(AssetError::MissingD3D9(
                    assets.d3d9_workload.display().to_string(),
                ));
            }
            log::warn!(
                "d3d9-workload.exe 無 build 時 digest(開發建置),僅檢查存在: {}",
                assets.d3d9_workload.display()
            );
        }
    }
    Ok(())
}

fn check_hash(path: &Path, want_hash: &str) -> Result<(), AssetError> {
    let got_hash =
        sha256_of(path).ok_or_else(|| AssetError::Missing(path.display().to_string()))?;
    if got_hash != want_hash {
        return Err(AssetError::HashMismatch(format!(
            "{}（應為 {want_hash}，實際 {got_hash}）",
            path.display()
        )));
    }
    Ok(())
}

fn sha256_of(path: &Path) -> Option<String> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path).ok()?;
    let mut h = Sha256::new();
    h.update(&bytes);
    Some(format!("{:x}", h.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pacedock_assets_{}_{}",
            std::process::id(),
            name
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, bytes: &[u8]) {
        std::fs::write(path, bytes).unwrap();
    }

    /// repo 內真實 vendored 資源必須通過內嵌 digest 驗證
    /// （d3d9-workload.exe 不在 git 內，CI checkout 沒有它 → 該環境跳過）
    #[test]
    fn verify_passes_on_vendored_resources() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources/benchmark");
        if !dir.join(D3D9_WORKLOAD_FILE).exists() {
            eprintln!("vendored d3d9 workload 不存在（未 build），跳過此測試");
            return;
        }
        verify(&load(&dir)).unwrap();
    }

    #[test]
    fn verify_fails_clearly_on_missing_asset() {
        let dir = temp_dir("missing");
        // 不建任何資源檔
        let err = verify(&load(&dir)).unwrap_err();
        assert!(err.to_string().contains("資源缺失"), "err={err}");
        assert_eq!(err.code(), crate::error::codes::BENCHMARK_ASSETS_MISSING);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_fails_on_hash_mismatch() {
        let dir = temp_dir("mismatch");
        write(&dir.join(PRESENTMON_FILE), b"tampered-bytes");
        let err = verify(&load(&dir)).unwrap_err();
        assert!(err.to_string().contains("hash 不符"), "err={err}");
        assert_eq!(
            err.code(),
            crate::error::codes::BENCHMARK_ASSETS_HASH_MISMATCH
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_requires_d3d9_workload_present() {
        let dir = temp_dir("nod3d9");
        // 真實 vendored 目錄只缺 d3d9 → 第一個失敗點應指名 d3d9-workload
        let vendored = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources/benchmark");
        write(&dir.join(PRESENTMON_FILE), &std::fs::read(vendored.join(PRESENTMON_FILE)).unwrap());
        write(&dir.join(VULKAN_WORKLOAD_FILE), &std::fs::read(vendored.join(VULKAN_WORKLOAD_FILE)).unwrap());
        // 不建 d3d9-workload.exe

        let err = verify(&load(&dir)).unwrap_err();
        assert!(err.to_string().contains("d3d9-workload"), "err={err}");
        assert_eq!(err.code(), crate::error::codes::BENCHMARK_ASSETS_MISSING);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
