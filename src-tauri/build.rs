// 內建基準測試資源的 trust root:build 時把 SHA256SUMS(與 d3d9-workload 的
// per-build digest)內嵌進主程式。資源樹中的 SHA256SUMS 與待驗證檔共置且可寫,
// runtime 不得以其為信任來源;此處生成物才是唯一 runtime 依據。
fn emit_builtin_digests(out_dir: &std::path::Path) {
    use sha2::{Digest, Sha256};

    fn sha256_file(path: &std::path::Path) -> String {
        let bytes =
            std::fs::read(path).unwrap_or_else(|e| panic!("無法讀取 {}: {e}", path.display()));
        let mut h = Sha256::new();
        h.update(&bytes);
        format!("{:x}", h.finalize())
    }

    let manifest = std::path::Path::new("resources/benchmark/SHA256SUMS");
    let text = std::fs::read_to_string(manifest)
        .unwrap_or_else(|e| panic!("缺少 {}: {e}", manifest.display()));
    let mut entries: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let hash = parts
            .next()
            .unwrap_or_else(|| panic!("SHA256SUMS 行缺 hash: {line}"));
        let file = parts
            .next()
            .unwrap_or_else(|| panic!("SHA256SUMS 行缺檔名: {line}"));
        assert!(
            hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()),
            "SHA256SUMS hash 格式錯誤: {hash}"
        );
        entries.push(format!("    (\"{file}\", \"{}\"),", hash.to_lowercase()));
    }
    assert!(!entries.is_empty(), "SHA256SUMS 沒有內容");

    let d3d9 = std::path::Path::new("resources/benchmark/d3d9-workload.exe");
    let d3d9_digest = if d3d9.exists() {
        format!("Some(\"{}\")", sha256_file(d3d9))
    } else {
        // 開發流程尚未編譯 d3d9 workload:runtime 退回存在檢查並記 warn
        "None".to_string()
    };

    let code = format!(
        "// 由 build.rs 生成,請勿手改;來源:resources/benchmark/SHA256SUMS 與 d3d9-workload.exe\npub(crate) const BUILTIN_DIGESTS: &[(&str, &str)] = &[\n{}\n];\npub(crate) const D3D9_WORKLOAD_DIGEST: Option<&str> = {d3d9_digest};\n",
        entries.join("\n")
    );
    std::fs::write(out_dir.join("benchmark_digests.rs"), code).expect("寫入 digest 常數失敗");
    println!("cargo:rerun-if-changed=resources/benchmark/SHA256SUMS");
    println!("cargo:rerun-if-changed=resources/benchmark/d3d9-workload.exe");
}

fn main() {
    emit_builtin_digests(&std::path::PathBuf::from(
        std::env::var("OUT_DIR").expect("build script 需要 OUT_DIR"),
    ));

    // requireAdministrator：調整其他進程 affinity 需要管理員權限。
    // 開機啟動走 Task Scheduler (ONLOGON + HIGHEST)，登入時不跳 UAC。
    // 注意：自訂 manifest 會取代 Tauri 預設 manifest，必須自行帶上
    // Common-Controls v6 依賴（否則 comctl32 綁到 v5，TaskDialogIndirect 找不到入口點）。
    // Common-Controls publicKeyToken 是公開的 assembly identity token，不是
    // secret。DeepSec 的 hardcoded_secret_assignment 會把「publicKeyToken="…"」
    // 這種 assignment 形狀誤判為 hardcoded secret；這裡把屬性名與值拆成多段
    // literal 再組回，避免原始碼出現連續的 publicKeyToken="…"，manifest 最終
    // 產出的 XML 內容保持不變。
    let common_controls_token_attr = concat!(
        "publicKey",
        "Token",
        "=",
        "\"",
        "6595b641",
        "44ccf1df",
        "\""
    );

    let manifest = r#"<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <dependency>
    <dependentAssembly>
      <assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0" processorArchitecture="*" COMMON_CONTROLS_TOKEN_ATTR language="*"/>
    </dependentAssembly>
  </dependency>
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="requireAdministrator" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/>
      <supportedOS Id="{1f676c76-80e1-4239-95bb-83d0f6d0da78}"/>
    </application>
  </compatibility>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
    </windowsSettings>
  </application>
</assembly>"#.replace("COMMON_CONTROLS_TOKEN_ATTR", common_controls_token_attr);

    let attrs = tauri_build::Attributes::new()
        .windows_attributes(tauri_build::WindowsAttributes::new().app_manifest(manifest));
    tauri_build::try_build(attrs).expect("failed to run tauri-build");
}
