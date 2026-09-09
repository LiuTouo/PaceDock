# FrameAnchor

<p align="center"><img src="src-tauri/icons/icon.png" width="128" alt="FrameAnchor 圖示"></p>

Windows GPU 實體核心調校工具。**繁體中文** · [English](README.en.md)

FrameAnchor 以合成負載比較 GPU 中斷親和性的實體核心候選，提供結果、歷史、手動套用與還原。啟動直接進入 GPU 頁，用完即可退出。

## 實測中斷核心

GPU 頁的「實測中斷 CPU 核心」可取樣 3 秒，列出實際處理 ISR 的 LP、對應實體核心及次數。取樣時請執行 GPU 負載；結果是最近一次取樣快照，與登錄檔中的親和性設定分開顯示。

量測透過 ETW ISR 位址與驅動映像辨識來源。多張 GPU 共用驅動時無法區分裝置；`dxgkrnl.sys` 屬於全系統共用顯示層，另列結果。沒有事件、驅動位址不可得、資料遺失或多處理器群組均會明確提示，不推算核心。量測需要管理員權限，期間不能執行基準測試或變更 GPU 策略；臨時 ETL 於量測結束後清除。

## 測試流程

1. 自動校準 FPS cap（預設 Vulkan；進階設定可改為固定限幀）。
2. 隨機排列 N 顆候選；各暖機 3 秒、取樣 10 秒。
3. 篩選前兩名各暖機 5 秒、取樣 20 秒，先後順序與篩選時的相對順序相反。
4. 還原測試前的完整 GPU 策略。

正常流程共 `N + min(N, 2)` 次候選擷取；校準、重試與最後還原另計。預估時間來自後端同一份排程，包含校準、啟動與重啟成本，實際等待或重試可能延長時間。暖機／取樣時間、workload、解析度及限幀可在進階設定調整。

候選包含整顆實體核心的全部 LP（含 SMT sibling），也包含核心 0。混合架構只提供 P-core；目前無法正確表示的處理器群組拓撲不提供候選。介面統一顯示例如「實體核心 1（LP 2、3）」。

## 結果與手動套用

篩選與複測分開保存，兩階段均使用 competitive score，最終排序只取複測資料。結果標示排名一致、差異接近、排名反轉或資料不足；單一候選標示無比較對象。

複測分數相對差距不超過 0.5% 時標示接近。這只是顯示用啟發式，不是 FPS 改善幅度或統計顯著性。快速測試只排名本次合成負載的候選，不能證明勝過 Windows 原始策略。

兩個有效複測候選均可手動選擇，確認後套用。接近／反轉時不預選勝者，也不追加確認測試。取消、失敗與資料不足結果不可套用。獨立手動核心設定位於進階區，標示「未經本次測試」。

套用時前端只傳核心 ID；後端驗證 session HMAC、GPU、CPU 指紋及完整核心 LP 集合，再建立遮罩，寫入後重啟並回讀。遇到錯誤會嘗試回復，未完成的回復日誌不會刪除。還原原始策略不受新版候選限制。

## 升級與資料

- 舊單 LP 歷史保留檢視，不可套用、不會自動擴大為整顆核心，也不會自動重新簽署。
- 舊 GPU 還原紀錄繼續有效。多 bit 策略會顯示實際完整 LP 集合。
- 舊遊戲 CPU 規則保留在設定檔中，但永不執行；儲存一般設定也會保留它們。若仍在執行的遊戲曾被舊版修改，請重新啟動遊戲。
- 系統匣、遊戲規則頁、Dashboard、自啟與最小化啟動已移除。升級時只清理可辨識為本工具建立的舊自啟排程；失敗時顯示原因並可重試。
- 一般設定保留語言、主題、更新與資料目錄功能。資料位於 `%APPDATA%\FrameAnchor`。
- 閒置時關閉視窗直接退出；GPU 測試、套用或還原期間會阻止退出，必須等待操作與清理完成。

GPU 策略操作需要管理員權限，並會重啟顯示裝置，畫面可能短暫閃黑。擷取保留 ETW／CSV 完整性、視窗檢查與取消保護。

## 安裝

### 從 Releases 安裝

從 [GitHub Releases](https://github.com/LiuTouo/FrameAnchor/releases) 下載最新版本。提供兩種發布形式：

- **NSIS 安裝程式**（`FrameAnchor_X.Y.Z_x64-setup.exe`）：標準安裝模式。支援自動更新（透過 Tauri updater plugin）。
- **可攜版**（`FrameAnchor_X.Y.Z_x64-portable.zip`）：解壓縮至任意目錄即可執行。啟動時與手動操作均支援線上檢查更新，可自動下載新版、詢問後替換執行檔並重啟。

每個發布資產均附帶 SHA256 校驗檔（`.sha256`）。

### 從原始碼建置

#### 前置需求

- Windows 11
- [Node.js](https://nodejs.org/) 20 或更新版本
- [Rust](https://www.rust-lang.org/tools/install) 1.80 或更新版本，使用 MSVC toolchain
- Visual Studio Build Tools，包含「使用 C++ 的桌面開發」工作負載
- Microsoft Edge WebView2 Runtime（Windows 11 預設已安裝）

#### 建置步驟

```bash
npm ci
npm run build:app
```

`npm run build:app` 是**本機完整桌面應用程式建置的正式指令**，以 `tauri build --no-sign` 執行，產出未簽署的 release 執行檔與 NSIS 安裝程式。已簽署的正式發布版本仍透過 `npm run tauri build` 或 GitHub release 工作流程（使用簽署 secret）產生。

NSIS 安裝程式會輸出至：

```text
src-tauri/target/release/bundle/nsis/
```

## 開發與建置

安裝相依套件：

```bash
npm ci
```

常用指令：

```bash
npm run dev
# 僅啟動 Vite 前端：http://localhost:1420
# 不包含 Rust 後端與 Tauri IPC

npm run tauri dev
# 啟動完整 Tauri 應用程式

npm run check
# 執行 svelte-check 與 TypeScript 檢查

npm run security:scan
# 以 DeepSec 掃描 Git 追蹤的第一方原始碼（見下方說明）

npm run build
# 建置前端至 dist/

npm run build:app
# 本機完整桌面應用程式建置（未簽署的 release 執行檔與 NSIS 安裝程式），
# 以 tauri build --no-sign 執行，不需 updater 簽署 secret

npm run tauri build
# 完整建置（含 updater 簽署）；需 TAURI_SIGNING_PRIVATE_KEY，
# 主要用於 GitHub release 工作流程

npm run gen-icons
# 重新產生 src-tauri/icons/*

npm run build:benchmark-assets
# 編譯 D3D9 workload sidecar（Rust + Direct3D 9）並複製到資源目錄

npm run verify:benchmark-assets
# 驗證內建基準測試資源（PresentMon／liblava 的 SHA-256 與 D3D9 sidecar 存在）

npm run fetch:benchmark-assets
# 重新下載 PresentMon 與 liblava workload 並更新 SHA256SUMS
```

`npm run tauri build` 會自動依序執行前端建置、D3D9 sidecar 建置與資源驗證，因此打包結果一定包含內建工具與授權聲明。

Rust 檢查與測試：

```bash
cargo check --manifest-path src-tauri/Cargo.toml
cargo test --manifest-path src-tauri/Cargo.toml
```

安全性掃描（DeepSec）：

`npm run security:scan` 會呼叫 `scripts/deepsec-scan.ps1`，以 `git ls-files` 建立只含 Git 追蹤第一方檔案的臨時 staging 目錄，再對該目錄執行 `deepsec shield scan`（預設 `--no-remote-l3`），最後自動清理。**請勿直接對 repository root 執行 `deepsec shield scan`**：DeepSec 不讀取 `.gitignore`，會連 `src-tauri/target`（生成文件）與 `.claude/worktrees`（工作副本）一起掃描，產生大量非第一方原始碼的誤報。

完整應用程式與程序操作依賴 Windows API。涉及 live process、affinity、priority、CPU Sets、系統匣、Task Scheduler 或 WebView2 的變更，仍需在 Windows 上使用可拋棄的測試程序進行手動驗證。

## 發布流程

維護者透過推送語意化版本標籤觸發 GitHub Actions 自動建置與發布。

### 前置設定：updater 簽署金鑰

自動更新需要一組 Ed25519 簽署金鑰。若尚未設定，請在本地執行：

```bash
npm run tauri signer generate -- -w src-tauri
```

此命令會在 `src-tauri` 目錄產生私鑰與公鑰。將私鑰內容設為 GitHub repository secret `TAURI_SIGNING_PRIVATE_KEY`；若私鑰有密碼保護，另設 `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`。將公鑰寫入 `src-tauri/tauri.conf.json` 中 `plugins.updater.pubkey` 欄位（取代 placeholder `REPLACE_ME_WITH_YOUR_PUBLIC_KEY_BASE64`）。

**私鑰絕對不可提交至版本控制。** CI 會在發行前驗證公鑰已替換且 secret 已設定，未設定時建置會失敗並顯示明確錯誤。

### 步驟

1. **同步版本號**

   確保以下四個檔案中的版本號一致（例如 `0.2.0`）：

   - `package.json` — `"version": "0.2.0"`
   - `package-lock.json` — 根層級 `"version": "0.2.0"`（`npm install` 會自動同步）
   - `src-tauri/Cargo.toml` — `[package]` 下的 `version = "0.2.0"`
   - `src-tauri/tauri.conf.json` — 頂層 `"version": "0.2.0"`

2. **提交並建立標籤**

   ```bash
   git add package.json package-lock.json src-tauri/Cargo.toml src-tauri/tauri.conf.json
   git commit -m "chore: bump version to 0.2.0"
   git tag -a v0.2.0 -m "v0.2.0"
   ```

   標籤必須嚴格符合 `vX.Y.Z` 格式。CI 會在建置前驗證標籤與所有檔案版本一致，不一致時會失敗並顯示明確錯誤。

3. **推送觸發建置**

   ```bash
   git push origin master
   git push origin v0.2.0
   ```

   推送標籤後，GitHub Actions 會自動執行版本驗證、簽署金鑰驗證、前端型別檢查、Rust 測試，然後建置以下資產：

   - NSIS 安裝程式（`FrameAnchor_X.Y.Z_x64-setup.exe`）與 `.sha256`
   - 可攜版 ZIP（`FrameAnchor_X.Y.Z_x64-portable.zip`）與 `.sha256`
   - updater 用 `latest.json` 與簽署檔案

4. **下載發布版本**

   建置完成後，前往 [GitHub Releases](https://github.com/LiuTouo/FrameAnchor/releases) 下載所需資產。

### 注意事項

- Windows 二進位檔**未經數位簽章**，下載及執行時 Windows Defender SmartScreen 可能顯示警告。這是預期行為，不影響程式功能。
- 可攜版與安裝版可從 設定頁面手動檢查更新；可攜版啟動時也會自動檢查。
- GitHub Actions 工作流程定義於 `.github/workflows/release.yml`。

## 技術架構

Tauri v2、Svelte 5、TypeScript 與 Rust。後端依拓撲產生候選，使用 PresentMon 擷取 frametime；GPU 操作由單一排他管理者協調，保留策略快照、HMAC、取消與 crash recovery。新版方法版本為 3，完整核心證據保存於 `summary.quick`，舊 `lp`／`bestLp` 永遠維持 LP 索引語意。

## 授權

本專案採用 [GNU General Public License v3.0](LICENSE)。

### 第三方元件聲明

GPU 基準測試功能內建並重新發布以下第三方元件（各自授權如下）：

- **PresentMon 2.5.1**（Intel 出品）— [MIT License](src-tauri/resources/benchmark/LICENSE-PresentMon.txt)。frame-time 收集工具；執行基準測試前會以固定 SHA-256 校驗。
- **liblava Vulkan workload**（`lava-triangle.exe`，由 valleyofdoom/AutoGpuAffinity 發布，使用 liblava 框架）— [MIT License](src-tauri/resources/benchmark/LICENSE-liblava.txt)。Vulkan 測試負載；執行前同樣會校驗 SHA-256。
- **Direct3D 9 workload**（`d3d9-workload.exe`）— 由本專案以 Rust 直接使用 Win32 Direct3D 9 API 撰寫的 sidecar（見 `src-tauri/d3d9-workload/`），GPL-3.0 與本專案一致。

授權全文與 SHA-256 清單存放於 `src-tauri/resources/benchmark/`。
