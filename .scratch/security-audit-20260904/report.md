# Security Review: PaceDock

## Scope

Single-pass repository-wide static security audit of all 100 Git-tracked paths at revision 1df767b65b458e6b96938dbd3c6f7228a4df41c2. All application-owned Rust, Svelte/TypeScript, workflow, script and configuration sources were reviewed; non-source and opaque artifacts are explicitly excluded below while their integration/provenance was assessed.

- Scan mode: repository
- Target kind: git_revision
- Target ID: target_sha256_e59acf8ddf685e0e1dec0ea2fac58620774e56c3b9a95c7ab95327c7c7d1b058
- Revision: 1df767b65b458e6b96938dbd3c6f7228a4df41c2
- Inventory strategy: repository
- Included paths: .
- Excluded paths: none
- Runtime or test status: Read-only offline source review; no application code, exploit payload or external service was executed.
- Artifacts reviewed: 31 Rust source files including the D3D9 sidecar, 23 frontend Svelte/TypeScript/CSS/HTML sources, 11 workflow and maintenance scripts, application manifests, capability/configuration files and three dependency lockfiles, README/PLAN deployment and security claims, bundled benchmark executable hashes, Authenticode metadata and SHA256SUMS

Limitations and exclusions:
- GitNexus index was one commit stale; reindex failed because the local FTS extension was unavailable, so GitNexus was used only for bounded navigation and every conclusion was verified from current source.
- TAC status could not be verified because the Codex Security Access connector was not logged in.
- No authoritative dependency vulnerability knowledge base was supplied and network access was intentionally not used; current dependency CVEs were not evaluated.
- Actual Windows DACL/MIC, final NSIS installation path, GitHub tag/environment protections and Tauri dependency internals are deployment/dependency facts not established by repository source.
- Opaque bundled PE internals were not reverse engineered.
- Excluded src-tauri/gen/schemas/\*\*: Generated Tauri dependency schemas are not the application-owned enforcement implementation; the actual capability manifest and consuming configuration were reviewed.
- Excluded src-tauri/icons/\*\* and src/assets/fonts/\*.woff2: Static visual/font assets have no executable application behavior; their loaders and generated/embedded usage were reviewed.
- Excluded LICENSE, src-tauri/resources/benchmark/LICENSE-\*.txt, src/assets/fonts/\*-LICENSE.txt: License texts are non-executable and do not define security controls.
- Excluded src-tauri/resources/benchmark/\*.exe: Opaque third-party PE internals were not reverse engineered. Hashes, Authenticode status, manifest trust and every application execution path were reviewed.

### Scan Summary

| Field | Value |
| --- | --- |
| Scan outcome | completed |
| Reportable findings | 52 |
| Severity mix | high: 15, medium: 18, low: 19 |
| Confidence mix | high: 52 |
| Coverage | complete |
| Validation mode | Independent baseline audit, independent architecture mapping, focused investigations and one parent source validation per deduplicated finding. |

Canonical artifacts: `scan-manifest.json`, `findings.json`, and `coverage.json`. This report is a deterministic projection of those files.

## Threat Model

PaceDock 0.2.6 是 Windows 專用、單使用者的 Tauri 桌面工具。Svelte main WebView 透過 Tauri IPC 控制 requireAdministrator Rust host；正常啟動包含手動/UAC 與 ONLOGON+HIGHEST --minimized，部署包含 NSIS currentUser 與 portable。敏感操作包括修改其他程序 scheduling、寫 HKLM GPU interrupt policy、重啟顯示裝置、以高權限啟動 benchmark sidecars、自我更新及 release 簽署/發布。

### Assets

- Windows 管理員 token、SeDebugPrivilege 與排程工作完整性（src-tauri/build.rs:21-49；src-tauri/src/process.rs:170-203；src-tauri/src/autostart.rs:12-27）。
- 其他程序的 affinity/priority 與 handle authority（src-tauri/src/process.rs:129-207；src-tauri/src/priority.rs:31-85）。
- HKLM GPU interrupt-affinity values 與顯示裝置可用性（src-tauri/src/gpu.rs:253-375）。
- 高權限 IPC、benchmark child-process execution 與 bundled executable integrity（src-tauri/src/main.rs:145-179；src-tauri/src/benchmark/process_win.rs:140-164）。
- config/session/recovery/restore state under config_dir（src-tauri/src/config.rs:14-31；src-tauri/src/benchmark/storage.rs:15-57；src-tauri/src/benchmark/recovery.rs:28-52）。
- installed/portable update integrity、release token 與 updater signing key（src-tauri/tauri.conf.json:45-55；src-tauri/src/update.rs:155-228；.github/workflows/release.yml:80-134）。
- Benchmark capture freshness and ranking evidence in `%APPDATA%\PaceDock\benchmarks` must remain bound to the successful PresentMon process and immutable through selection (src-tauri/src/benchmark/runner.rs:1888-2030,2338-2426).

### Trust Boundaries

- local main WebView 到 elevated Rust invoke_handler；renderer payload可到達規則、自啟、更新、benchmark及GPU mutation（src-tauri/src/main.rs:145-179；src/lib/ipc.ts:19-65）。
- user-profile APPDATA files 到 elevated startup/watcher/GPU manager；source 有 serde與durability controls，但沒有 ACL、owner、reparse或MAC controls（src-tauri/src/config.rs:14-108；src-tauri/src/benchmark/recovery.rs:83-110）。
- rules 到其他 live processes；full path/basename match 經 blacklist/PID reuse/readback controls後取得 privileged handles（src-tauri/src/watcher.rs:455-502,703-805；src-tauri/src/process.rs:715-755）。
- GPU manager 到 HKLM與display device；interactive apply 有 LP/present-adapter/BasicDisplay/single-flight/journal/readback/rollback controls（src-tauri/src/benchmark/manager.rs:149-265,626-687）。
- recovery journal 到 startup GPU mutation；PolicyApplied/DeviceRestarted 直接寫 snapshot 並 restart，沒有 interactive present-adapter/BasicDisplay checks（src-tauri/src/benchmark/manager.rs:341-365；src-tauri/src/gpu.rs:574-592）。
- benchmark config/resources 到 elevated child processes；normal assets beside executable，PresentMon/lava hashes由同目錄 manifest決定，D3D9只檢查存在，debug overrides可更換 executable（src-tauri/src/benchmark/assets.rs:71-96；src-tauri/src/benchmark/manager.rs:1498-1523）。
- GitHub release metadata/assets 到 portable replacement；custom path驗證版本/名稱/大小/checksum/ZIP structure後在固定 TEMP staging並以PowerShell替換（src-tauri/src/update.rs:160-228,299-365,381-707）。
- release workflow/action code 到 contents:write token與updater signing key（.github/workflows/release.yml:8-9,80-134）。
- third-party benchmark releases 到 vendored executables；refresh script下載後才產生 SHA256SUMS（scripts/fetch-benchmark-assets.mjs:44-108）。
- Caller-selected PE files cross into developer diagnostic parsers and terminal output; malformed offsets/strings must remain bounded and displayed names must be inert (scripts/pe-exports.mjs:5-45；scripts/pe-imports.mjs:5-62).

### Attacker Capabilities

- same-user medium-integrity local process 可建立程序、控制自身 executable path/name，且在常見 Windows profile 部署中可寫該帳戶 APPDATA/TEMP；不預設已持有 administrator token。
- renderer compromise actor 可組任意已註冊 IPC payload，但目前 source 未建立外部 content 或 XSS 入口，因此此能力只作條件式 scenario。
- network actor 不預設能突破 TLS或控制 GitHub；release publisher、Action ref與third-party asset control分別作低機率 supply-chain actor。
- PR author 在一般 CI 只有 contents:read，不預設能存取 release secrets（.github/workflows/ci.yml:3-8）。
- An author of a PE manually inspected by a developer can control export/import RVAs and name bytes, but does not thereby control the product runtime or an elevated process.

### Security Objectives

- 高完整性 execution、scheduled-task targets、temporary helpers、sidecars與update artifacts不可由 medium-integrity actor置換或由不可信搜尋路徑解析。
- elevated process只對使用者明確規則命中的非黑名單程序操作，並持續防止 PID reuse與錯誤 readback。
- GPU mutation只可針對目前存在 adapter/有效LP，需BasicDisplay、single-flight、pre-mutation snapshot、readback、rollback與可信 recovery state。
- installed/portable updates必須綁定可信 publisher identity、版本、source revision與完整 payload；signing key及publish token只交給 immutable reviewed code。
- benchmark executable provenance必須由不隨 executable 同時可寫的 trust root驗證；debug overrides不可存在於production IPC或必須受 allowlist。
- dangerous update/GPU/benchmark/history actions需要清楚目標/確認；renderer不是獨立授權控制時，backend需重新驗證所有安全屬性。
- Every benchmark capture used for ranking must be fresh, produced by the successful expected process, and consumed as the same immutable evidence; developer binary parsers must bound work and escape terminal controls.

### Assumptions

- 無 user context、既有 threat model、knowledge base或 SECURITY.md；範圍為目前 repository 全部內容。
- 產品是 Windows desktop app，不假設 network service、多租戶或remote unauthenticated caller。
- APPDATA正常時 effective config root 為 %APPDATA%\\PaceDock；缺失時 code fallback 至 current-exe-dir或相對路徑（src-tauri/src/config.rs:14-27）。
- NSIS installMode=currentUser但 repository未固定絕對安裝路徑；portable可在任意目錄。
- installed updater使用配置 public key；portable custom updater只使用同release checksum，兩條 authenticity controls不同。
- normal UI對 benchmark executable override傳 null；production IPC/schema仍接受該欄位，是否可利用需renderer compromise前提。
- GitHub protection、actual Windows ACL/MIC、Tauri/Wry internal IPC/navigation controls及Action ref解析不在 repository source，相關結論需明示部署前提。
- GitNexus FTS index更新失敗，既有索引只作導航；finding evidence以目前source驗證。
- All production Rust, Svelte/TypeScript and repository automation/configuration sources were reviewed. Generated Tauri schemas, visual/font assets, licenses and opaque third-party executable internals are explicit exclusions; executable hashes/signatures and provenance paths were still inspected.
- Current third-party dependency CVEs were not queried because the Standard workflow kept source review offline and no authoritative knowledge base was supplied.
- Two watcher questions remain unresolved but not reportable: authorization checks are not bound to the same process handle opened for mutation, and FileName matching can proceed when an executable path is unreadable. No reliable attacker-to-impact path was established from source.

## Findings

| Finding | Severity | Confidence | Detailed write-up |
| --- | --- | --- | --- |
| [固定使用者 TEMP staging 允許在驗證後置換更新腳本與 executable](#finding-1) | high | high | inline below |
| [固定使用者 TEMP staging 允許置換更新腳本與 executable](#finding-2) | high | high | inline below |
| [最高權限排程工作指向可由未提升程序置換的 executable](#finding-3) | high | high | inline below |
| [固定使用者 TEMP staging 允許在驗證後置換更新腳本與 executable](#finding-4) | high | high | inline below |
| [未限定系統工具路徑可在提升後啟動攻擊者 binary](#finding-5) | high | high | inline below |
| [未限定系統工具路徑可啟動攻擊者binary](#finding-6) | high | high | inline below |
| [固定TEMP staging允許置換update script與PE](#finding-7) | high | high | inline below |
| [最高權限排程工作指向可由未提升程序置換的 executable](#finding-8) | high | high | inline below |
| [未限定系統工具路徑可在提升後啟動攻擊者 binary](#finding-9) | high | high | inline below |
| [固定TEMP staging允許置換update script與PE](#finding-10) | high | high | inline below |
| [最高權限排程工作指向可置換 executable](#finding-11) | high | high | inline below |
| [未限定系統工具路徑可啟動攻擊者binary](#finding-12) | high | high | inline below |
| [最高權限排程工作指向可由未提升程序置換的 executable](#finding-13) | high | high | inline below |
| [最高權限排程工作指向可置換 executable](#finding-14) | high | high | inline below |
| [未限定系統工具路徑可在提升後啟動攻擊者 binary](#finding-15) | high | high | inline below |
| [可偽造的 recovery/restore JSON 驅動 elevated HKLM GPU mutation](#finding-16) | medium | high | inline below |
| [production BenchmarkConfig 可選取未驗證 executable](#finding-17) | medium | high | inline below |
| [benchmark 接受或重讀未綁定本次 capture identity 的 CSV](#finding-18) | medium | high | inline below |
| [可寫manifest與D3D9 exists-only無法保護elevated sidecars](#finding-19) | medium | high | inline below |
| [可修改的同目錄 manifest 與未驗證 D3D9 無法保護 elevated benchmark sidecars](#finding-20) | medium | high | inline below |
| [可寫manifest與D3D9 exists-only無法保護elevated sidecars](#finding-21) | medium | high | inline below |
| [production BenchmarkConfig 可選取未驗證 executable 供 elevated runner 執行](#finding-22) | medium | high | inline below |
| [可修改的同目錄 manifest 與未驗證 D3D9 無法保護 elevated benchmark sidecars](#finding-23) | medium | high | inline below |
| [可偽造 recovery/restore JSON 驅動 elevated HKLM GPU mutation](#finding-24) | medium | high | inline below |
| [production BenchmarkConfig 可選取未驗證 executable 供 elevated runner 執行](#finding-25) | medium | high | inline below |
| [可偽造的 APPDATA state 驅動 elevated GPU mutation](#finding-26) | medium | high | inline below |
| [benchmark 接受或重讀未綁定本次 capture identity 的 CSV](#finding-27) | medium | high | inline below |
| [production BenchmarkConfig可選未驗證executable](#finding-28) | medium | high | inline below |
| [production BenchmarkConfig可選未驗證executable](#finding-29) | medium | high | inline below |
| [偽造recovery/restore JSON驅動HKLM GPU mutation](#finding-30) | medium | high | inline below |
| [benchmark 接受或重讀未綁定本次 capture identity 的 CSV](#finding-31) | medium | high | inline below |
| [可偽造的 APPDATA state 驅動 elevated GPU policy mutation](#finding-32) | medium | high | inline below |
| [可修改同目錄 manifest 與未驗證 D3D9 無法保護 elevated sidecars](#finding-33) | medium | high | inline below |
| [可變 GitHub Action ref 接觸 signing key 與 release token](#finding-34) | low | high | inline below |
| [PE 診斷 parser 可因未終止名稱進入無限迴圈](#finding-35) | low | high | inline below |
| [PE 診斷工具未中和名稱中的 terminal control sequences](#finding-36) | low | high | inline below |
| [portable update 缺少獨立 publisher signature](#finding-37) | low | high | inline below |
| [第三方binary下載後才自生digest](#finding-38) | low | high | inline below |
| [可變 GitHub Action ref 直接接觸 updater signing key 與 release token](#finding-39) | low | high | inline below |
| [portable update缺獨立publisher signature](#finding-40) | low | high | inline below |
| [第三方 benchmark executable 下載後才從自身產生 digest](#finding-41) | low | high | inline below |
| [第三方binary下載後才自生digest](#finding-42) | low | high | inline below |
| [可變 GitHub Action ref 直接接觸 updater signing key 與 release token](#finding-43) | low | high | inline below |
| [PE 診斷 parser 可因未終止名稱進入無限迴圈](#finding-44) | low | high | inline below |
| [第三方 benchmark executable 下載後才從自身產生信任 digest](#finding-45) | low | high | inline below |
| [portable update 缺少獨立 publisher signature](#finding-46) | low | high | inline below |
| [mutable Action ref接觸signing key](#finding-47) | low | high | inline below |
| [PE 診斷工具未中和名稱中的 terminal control sequences](#finding-48) | low | high | inline below |
| [portable update 缺少獨立 publisher signature](#finding-49) | low | high | inline below |
| [mutable Action ref接觸signing key](#finding-50) | low | high | inline below |
| [第三方 benchmark executable 下載後才從自身產生信任 digest](#finding-51) | low | high | inline below |
| [portable update缺獨立publisher signature](#finding-52) | low | high | inline below |

### Confidence Scale

| Label | Meaning |
| --- | --- |
| high | Direct evidence supports the finding with no material unresolved blocker. |
| medium | Evidence supports a plausible issue, but material runtime or reachability proof remains. |
| low | Evidence is incomplete and the item is retained only for explicit follow-up. |

<a id="finding-1"></a>

### [1] 固定使用者 TEMP staging 允許在驗證後置換更新腳本與 executable

| Field | Value |
| --- | --- |
| Severity | high |
| Confidence | high |
| Confidence rationale | 固定路徑、普通 create、path-based reopen/move/start 與缺少 ACL/reparse/file-ID binding 均由 source 建立；不需假設惡意 release。 |
| Category | insecure-temporary-file |
| CWE | CWE-367, CWE-377 |
| Affected lines | src-tauri/src/update.rs:460-500, src-tauri/src/update.rs:673-706, src-tauri/src/update.rs:557-586, src-tauri/src/update.rs:650-652, src-tauri/src/commands.rs:307-335, src-tauri/build.rs:27-31 |

#### Summary

portable updater 驗證記憶體中的 ZIP 後，把 executable/resources 與 `update.ps1` 寫到固定 `%TEMP%\pacedock_update`；elevated helper 之後重新依路徑開啟腳本並移動/啟動 staged executable。低完整性同帳戶程序可置換這些物件，使任意 script/PE 以管理員權限執行。

#### Root Cause

完整性驗證只綁定下載 bytes，未跨越 extraction-to-consumption boundary；staged script與PE的 path identity/ACL 不受保護。

**Fixed writable staging paths** — `src-tauri/src/update.rs:460-475`

固定 user TEMP 路徑與一般 File::create 未建立不可變 file identity。

```Rust
let tmp_dir = std::env::temp_dir().join("pacedock_update");
std::fs::create_dir_all(&tmp_dir)?;
let tmp_exe = tmp_dir.join("PaceDock_new.exe");
let mut out = std::fs::File::create(&tmp_exe)?;
```

**Fixed script is reopened by path** — `src-tauri/src/update.rs:673-706`

寫入與 interpreter 開啟不是同一 handle，且父目錄/檔案未受 high-only ACL 保護。

```Rust
let script_path = tmp_dir.join("update.ps1");
let mut file = std::fs::File::create(&script_path)?;
file.write_all(script.as_bytes())?;
Command::new("powershell").arg(&script_path).spawn()?;
```

#### Validation

獨立確認 script reopen 與 staged PE move/start 兩個 sink共享相同固定、可寫 staging root，因此合併為一個 finding並保留兩條 attack path。

Validation method: Manual static source trace against the current repository state

- **Status:** validated
- **Disposition:** reportable

Evidence:
- src-tauri/src/update.rs:460-500
- src-tauri/src/update.rs:673-706
- src-tauri/src/update.rs:557-586
- src-tauri/src/update.rs:650-652

Counterevidence and remaining uncertainty:
- ZIP 有大小、magic、SHA-256、必要 entries 與 traversal checks。
- `ps_single_quote` 正確處理 PowerShell 字串。
- 這些控制不涵蓋解壓後本機置換；利用仍需使用者執行 portable update。

#### Dataflow

attacker 監看固定 user TEMP path，於 write/verify 後替換 `update.ps1` 或 `PaceDock_new.exe`；elevated process/PowerShell重新依路徑消費；任意 script/PE獲得 administrator token。

**Fixed writable staging paths** — `src-tauri/src/update.rs:460-475`

固定 user TEMP 路徑與一般 File::create 未建立不可變 file identity。

```Rust
let tmp_dir = std::env::temp_dir().join("pacedock_update");
std::fs::create_dir_all(&tmp_dir)?;
let tmp_exe = tmp_dir.join("PaceDock_new.exe");
let mut out = std::fs::File::create(&tmp_exe)?;
```

**Fixed script is reopened by path** — `src-tauri/src/update.rs:673-706`

寫入與 interpreter 開啟不是同一 handle，且父目錄/檔案未受 high-only ACL 保護。

```Rust
let script_path = tmp_dir.join("update.ps1");
let mut file = std::fs::File::create(&script_path)?;
file.write_all(script.as_bytes())?;
Command::new("powershell").arg(&script_path).spawn()?;
```

#### Reachability

entry point 是 `perform_portable_update`；script race 在 spawn 時到 sink，staged PE race 可延續到原程序退出後的 Move-Item/Start-Process。

Preconditions:
- 同帳戶 medium-integrity attacker 可寫其 TEMP。
- 使用者觸發 portable update。

Existing controls:
- 下載資料先經 SHA-256 與 ZIP allowlist。
- 路徑值以 PowerShell 單引號正確 escape。

#### Severity

**High** — 攻擊成功直接取得 administrator code execution；對已駐留且能監看使用者 TEMP 的 attacker，在使用者觸發 portable update 後路徑固定且可重試。

同帳戶 medium-integrity attacker 可寫其 TEMP。；使用者觸發 portable update。

Impact assessment:
- **Level:** high
- **Rationale:** 任意 PowerShell或native executable以administrator權限執行並可持久化。

Likelihood assessment:
- **Level:** high
- **Rationale:** 在 local resident attacker與portable-update trigger前提下，固定名稱、長等待及缺少identity binding讓置換可實際重試。

#### Remediation

不要從共享 user TEMP 依名稱執行腳本。使用受保護、每次不可預測的 high-only staging directory，exclusive/create-new 且拒絕 reparse point；用持續持有的 handle 或 file ID 把驗證、置換與執行綁到同一物件，並在交換前驗證 executable signature。較佳方案是受保護且已簽署的 native updater/helper。

Tests:
- 讓 medium-integrity helper 持續置換 fixed staging paths，確認新版本拒絕或仍執行 trusted file ID。
- 測試 precreated directory junction、file symlink/hardlink、rename race與script/PE replacement。

Preventive controls:
- Secure temporary directory ownership/DACL/MIC。
- No-reparse exclusive file creation。
- Verify-to-use file identity binding。

<a id="finding-2"></a>

### [2] 固定使用者 TEMP staging 允許置換更新腳本與 executable

| Field | Value |
| --- | --- |
| Severity | high |
| Confidence | high |
| Confidence rationale | Validated by an independent worker and parent static source trace. |
| Category | insecure-temporary-file |
| CWE | CWE-367, CWE-377 |
| Affected lines | src-tauri/src/update.rs:460-500, src-tauri/src/update.rs:673-706, src-tauri/src/update.rs:557-586, src-tauri/src/update.rs:650-652 |

#### Summary

portable update把update.ps1與staged PE寫入固定user TEMP，再以pathname reopen/move/start；same-user attacker可在驗證後置換而取得administrator execution。

#### Root Cause

portable update把update.ps1與staged PE寫入固定user TEMP，再以pathname reopen/move/start；same-user attacker可在驗證後置換而取得administrator execution。

#### Validation

Validated against current repository source with counterevidence and prerequisites retained.

Validation method: Manual static source trace

- **Status:** validated
- **Disposition:** reportable

#### Dataflow

portable update把update.ps1與staged PE寫入固定user TEMP，再以pathname reopen/move/start；same-user attacker可在驗證後置換而取得administrator execution。

#### Reachability

Source-backed conditional path described in summary.

#### Severity

**High** — Severity was calibrated from the validated attacker, prerequisites, impact and likelihood.

Additional runtime or deployment evidence could raise or lower this severity.

Impact assessment:
- **Level:** high
- **Rationale:** portable update把update.ps1與staged PE寫入固定user TEMP，再以pathname reopen/move/start；same-user attacker可在驗證後置換而取得administrator execution。

Likelihood assessment:
- **Level:** high
- **Rationale:** Calibrated from explicit prerequisites.

#### Remediation

使用high-only隨機staging、exclusive no-reparse creation、file-ID/handle binding與protected signed helper。

<a id="finding-3"></a>

### [3] 最高權限排程工作指向可由未提升程序置換的 executable

| Field | Value |
| --- | --- |
| Severity | high |
| Confidence | high |
| Confidence rationale | 排程參數、提升層級、target 選擇與缺少 ACL/owner/reparse/signature 驗證均可直接由 source 建立；實際安裝絕對路徑仍是明示前提。 |
| Category | privilege-escalation |
| CWE | CWE-732 |
| Affected lines | src-tauri/src/autostart.rs:14-21, src-tauri/tauri.conf.json:39-42, src-tauri/build.rs:27-31, README.md:68-70, README.md:92-97 |

#### Summary

啟用開機自啟後，ONLOGON/HIGHEST 工作只保存目前 executable 的路徑；在 currentUser 或使用者可寫的 portable 部署中，同帳戶 medium-integrity 程序可於程式退出後置換該檔，讓下次登入無 UAC 執行攻擊者程式。

#### Root Cause

信任邊界只綁定 mutable pathname，未把排程權限綁定到受保護且可驗證的 executable identity。

**HIGHEST task uses current executable** — `src-tauri/src/autostart.rs:14-21`

排程直接信任 current_exe 路徑，沒有檔案或父目錄完整性驗證。

```Rust
let exe = std::env::current_exe()?;
let tr = format!("\"{}\" --minimized", exe.display());
Command::new("schtasks").args(["/Create", "/TN", TASK_NAME, "/SC", "ONLOGON", "/RL", "HIGHEST", "/TR", &tr, "/F"])
```

#### Validation

source 確認 current_exe 被直接寫入 /RL HIGHEST ONLOGON task；路徑 quoting 只解決參數剖析，不阻止後續檔案置換。

Validation method: Manual static source trace against the current repository state

- **Status:** validated
- **Disposition:** reportable

Evidence:
- src-tauri/src/autostart.rs:14-21
- src-tauri/tauri.conf.json:39-42
- src-tauri/build.rs:27-31

Counterevidence and remaining uncertainty:
- autostart 預設關閉。
- 標準非管理員帳戶的 HIGHEST 不會憑空產生 administrator token。
- 若 target 位於 Program Files 類受保護樹且所有父路徑不可重導，攻擊前提不成立。

#### Dataflow

同帳戶 medium-integrity attacker 置換 current_exe 路徑的 PE；Task Scheduler 在 ONLOGON 解析同一路徑，使用 HIGHEST token 建立攻擊者程序。

**HIGHEST task uses current executable** — `src-tauri/src/autostart.rs:14-21`

排程直接信任 current_exe 路徑，沒有檔案或父目錄完整性驗證。

```Rust
let exe = std::env::current_exe()?;
let tr = format!("\"{}\" --minimized", exe.display());
Command::new("schtasks").args(["/Create", "/TN", TASK_NAME, "/SC", "ONLOGON", "/RL", "HIGHEST", "/TR", &tr, "/F"])
```

#### Reachability

入口是可寫的 task target path；sink 是 Task Scheduler 的 HIGHEST process creation。排程保存的是字串路徑，未保存原檔 file identity。

Preconditions:
- 受害者為具 split administrator token 的帳戶。
- 受害者曾啟用 PaceDock autostart。
- current_exe 或任一可重導父路徑可由 medium-integrity attacker 修改。

Existing controls:
- task action 的 executable path 有雙引號包覆。
- 建立 task 當下 PaceDock 已提升。

#### Severity

**High** — 此路徑可持續取得管理員程式碼執行；對使用 split administrator token、已啟用 autostart 且 target path 可寫的部署，利用只需檔案置換與下一次登入。

受害者為具 split administrator token 的帳戶。；受害者曾啟用 PaceDock autostart。；current_exe 或任一可重導父路徑可由 medium-integrity attacker 修改。

Impact assessment:
- **Level:** high
- **Rationale:** 取得持續、無 UAC 的 administrator native code execution。

Likelihood assessment:
- **Level:** high
- **Rationale:** 在明示的 split-admin、autostart enabled、writable-target 條件下，置換與登入觸發直接且可重複。

#### Remediation

不得為 medium-integrity 可寫位置建立 HIGHEST task。將主程式或最小 signed launcher 安裝到 machine-wide、Administrators/SYSTEM 才可修改的目錄；建立及每次更新後驗證所有父目錄與檔案的 DACL、owner、reparse 狀態及簽章。若做不到，將 task 降為 LIMITED 或拒絕啟用。

Tests:
- 在可寫測試目錄放置 signed benign fixture、建立 HIGHEST task 後置換檔案，確認新版本會拒絕建立或執行。
- 以 protected Program Files 與 portable writable directory 各測一次，驗證 ACL/reparse/signature policy。

Preventive controls:
- Privileged executable 必須位於 medium-integrity 不可寫位置。
- 排程建立前後綁定並驗證 executable identity。

<a id="finding-4"></a>

### [4] 固定使用者 TEMP staging 允許在驗證後置換更新腳本與 executable

| Field | Value |
| --- | --- |
| Severity | high |
| Confidence | high |
| Confidence rationale | 固定路徑、普通 create、path-based reopen/move/start 與缺少 ACL/reparse/file-ID binding 均由 source 建立；不需假設惡意 release。 |
| Category | insecure-temporary-file |
| CWE | CWE-367, CWE-377 |
| Affected lines | src-tauri/src/update.rs:460-500, src-tauri/src/update.rs:673-706, src-tauri/src/update.rs:557-586, src-tauri/src/update.rs:650-652, src-tauri/src/commands.rs:307-335, src-tauri/build.rs:27-31 |

#### Summary

portable updater 驗證記憶體中的 ZIP 後，把 executable/resources 與 `update.ps1` 寫到固定 `%TEMP%\pacedock_update`；elevated helper 之後重新依路徑開啟腳本並移動/啟動 staged executable。低完整性同帳戶程序可置換這些物件，使任意 script/PE 以管理員權限執行。

#### Root Cause

完整性驗證只綁定下載 bytes，未跨越 extraction-to-consumption boundary；staged script與PE的 path identity/ACL 不受保護。

**Fixed writable staging paths** — `src-tauri/src/update.rs:460-475`

固定 user TEMP 路徑與一般 File::create 未建立不可變 file identity。

```Rust
let tmp_dir = std::env::temp_dir().join("pacedock_update");
std::fs::create_dir_all(&tmp_dir)?;
let tmp_exe = tmp_dir.join("PaceDock_new.exe");
let mut out = std::fs::File::create(&tmp_exe)?;
```

**Fixed script is reopened by path** — `src-tauri/src/update.rs:673-706`

寫入與 interpreter 開啟不是同一 handle，且父目錄/檔案未受 high-only ACL 保護。

```Rust
let script_path = tmp_dir.join("update.ps1");
let mut file = std::fs::File::create(&script_path)?;
file.write_all(script.as_bytes())?;
Command::new("powershell").arg(&script_path).spawn()?;
```

#### Validation

獨立確認 script reopen 與 staged PE move/start 兩個 sink共享相同固定、可寫 staging root，因此合併為一個 finding並保留兩條 attack path。

Validation method: Manual static source trace against the current repository state

- **Status:** validated
- **Disposition:** reportable

Evidence:
- src-tauri/src/update.rs:460-500
- src-tauri/src/update.rs:673-706
- src-tauri/src/update.rs:557-586
- src-tauri/src/update.rs:650-652

Counterevidence and remaining uncertainty:
- ZIP 有大小、magic、SHA-256、必要 entries 與 traversal checks。
- `ps_single_quote` 正確處理 PowerShell 字串。
- 這些控制不涵蓋解壓後本機置換；利用仍需使用者執行 portable update。

#### Dataflow

attacker 監看固定 user TEMP path，於 write/verify 後替換 `update.ps1` 或 `PaceDock_new.exe`；elevated process/PowerShell重新依路徑消費；任意 script/PE獲得 administrator token。

**Fixed writable staging paths** — `src-tauri/src/update.rs:460-475`

固定 user TEMP 路徑與一般 File::create 未建立不可變 file identity。

```Rust
let tmp_dir = std::env::temp_dir().join("pacedock_update");
std::fs::create_dir_all(&tmp_dir)?;
let tmp_exe = tmp_dir.join("PaceDock_new.exe");
let mut out = std::fs::File::create(&tmp_exe)?;
```

**Fixed script is reopened by path** — `src-tauri/src/update.rs:673-706`

寫入與 interpreter 開啟不是同一 handle，且父目錄/檔案未受 high-only ACL 保護。

```Rust
let script_path = tmp_dir.join("update.ps1");
let mut file = std::fs::File::create(&script_path)?;
file.write_all(script.as_bytes())?;
Command::new("powershell").arg(&script_path).spawn()?;
```

#### Reachability

entry point 是 `perform_portable_update`；script race 在 spawn 時到 sink，staged PE race 可延續到原程序退出後的 Move-Item/Start-Process。

Preconditions:
- 同帳戶 medium-integrity attacker 可寫其 TEMP。
- 使用者觸發 portable update。

Existing controls:
- 下載資料先經 SHA-256 與 ZIP allowlist。
- 路徑值以 PowerShell 單引號正確 escape。

#### Severity

**High** — 攻擊成功直接取得 administrator code execution；對已駐留且能監看使用者 TEMP 的 attacker，在使用者觸發 portable update 後路徑固定且可重試。

Additional runtime or deployment evidence could raise or lower this severity.

Impact assessment:
- **Level:** high
- **Rationale:** 任意 PowerShell或native executable以administrator權限執行並可持久化。

Likelihood assessment:
- **Level:** high
- **Rationale:** 在 local resident attacker與portable-update trigger前提下，固定名稱、長等待及缺少identity binding讓置換可實際重試。

#### Remediation

不要從共享 user TEMP 依名稱執行腳本。使用受保護、每次不可預測的 high-only staging directory，exclusive/create-new 且拒絕 reparse point；用持續持有的 handle 或 file ID 把驗證、置換與執行綁到同一物件，並在交換前驗證 executable signature。較佳方案是受保護且已簽署的 native updater/helper。

Tests:
- 讓 medium-integrity helper 持續置換 fixed staging paths，確認新版本拒絕或仍執行 trusted file ID。
- 測試 precreated directory junction、file symlink/hardlink、rename race與script/PE replacement。

Preventive controls:
- Secure temporary directory ownership/DACL/MIC。
- No-reparse exclusive file creation。
- Verify-to-use file identity binding。

<a id="finding-5"></a>

### [5] 未限定系統工具路徑可在提升後啟動攻擊者 binary

| Field | Value |
| --- | --- |
| Severity | high |
| Confidence | high |
| Confidence rationale | Validated by an independent worker and parent static source trace. |
| Category | untrusted-search-path |
| CWE | CWE-426 |
| Affected lines | src-tauri/src/autostart.rs:54-59, src-tauri/src/tray.rs:79-90, src-tauri/src/update.rs:695-706, src-tauri/src/commands.rs:154-162 |

#### Summary

elevated host以裸名稱啟動schtasks/powershell/explorer；tray startup自動到達schtasks，較早可寫搜尋目錄可種植同名PE。

#### Root Cause

elevated host以裸名稱啟動schtasks/powershell/explorer；tray startup自動到達schtasks，較早可寫搜尋目錄可種植同名PE。

#### Validation

Validated against current repository source with counterevidence and prerequisites retained.

Validation method: Manual static source trace

- **Status:** validated
- **Disposition:** reportable

#### Dataflow

elevated host以裸名稱啟動schtasks/powershell/explorer；tray startup自動到達schtasks，較早可寫搜尋目錄可種植同名PE。

#### Reachability

Source-backed conditional path described in summary.

#### Severity

**High** — Severity was calibrated from the validated attacker, prerequisites, impact and likelihood.

Additional runtime or deployment evidence could raise or lower this severity.

Impact assessment:
- **Level:** high
- **Rationale:** elevated host以裸名稱啟動schtasks/powershell/explorer；tray startup自動到達schtasks，較早可寫搜尋目錄可種植同名PE。

Likelihood assessment:
- **Level:** high
- **Rationale:** Calibrated from explicit prerequisites.

#### Remediation

使用System32可信絕對路徑與受保護child CWD，或直接Windows API。

<a id="finding-6"></a>

### [6] 未限定系統工具路徑可啟動攻擊者binary

| Field | Value |
| --- | --- |
| Severity | high |
| Confidence | high |
| Confidence rationale | Independent worker plus parent source trace. |
| Category | privilege-escalation |
| CWE | CWE-426 |
| Affected lines | src-tauri/src/autostart.rs:54 |

#### Summary

elevated host以裸名稱解析schtasks/powershell/explorer，tray startup自動到達schtasks。

#### Validation

Validated against current source.

Validation method: Manual static source trace

- **Status:** validated
- **Disposition:** reportable

#### Dataflow

elevated host以裸名稱解析schtasks/powershell/explorer，tray startup自動到達schtasks。

#### Reachability

elevated host以裸名稱解析schtasks/powershell/explorer，tray startup自動到達schtasks。

#### Severity

**High** — Previously validated and calibrated.

Additional runtime or deployment evidence could raise or lower this severity.

#### Remediation

使用System32可信絕對路徑或Windows API。

<a id="finding-7"></a>

### [7] 固定TEMP staging允許置換update script與PE

| Field | Value |
| --- | --- |
| Severity | high |
| Confidence | high |
| Confidence rationale | Independently reviewed and parent-validated against current source. |
| Category | insecure-temporary-file |
| CWE | CWE-367, CWE-377 |
| Affected lines | src-tauri/src/update.rs:460-500, src-tauri/src/update.rs:673-706 |

#### Summary

portable updater在固定user TEMP寫入後重新依path執行/移動/啟動，未綁定驗證後file identity。

#### Root Cause

portable updater在固定user TEMP寫入後重新依path執行/移動/啟動，未綁定驗證後file identity。

#### Validation

Validated against current source.

Validation method: Manual static source trace

- **Status:** validated
- **Disposition:** reportable

Evidence:
- src-tauri/src/update.rs:460
- src-tauri/src/update.rs:673

#### Dataflow

portable updater在固定user TEMP寫入後重新依path執行/移動/啟動，未綁定驗證後file identity。

#### Reachability

portable updater在固定user TEMP寫入後重新依path執行/移動/啟動，未綁定驗證後file identity。

#### Severity

**High** — Severity calibrated from established impact and likelihood.

Additional runtime or deployment evidence could raise or lower this severity.

Impact assessment:
- **Level:** high
- **Rationale:** portable updater在固定user TEMP寫入後重新依path執行/移動/啟動，未綁定驗證後file identity。

Likelihood assessment:
- **Level:** high
- **Rationale:** Conditional on the stated attacker prerequisites.

#### Remediation

使用high-only隨機no-reparse staging與handle/file-ID binding。

<a id="finding-8"></a>

### [8] 最高權限排程工作指向可由未提升程序置換的 executable

| Field | Value |
| --- | --- |
| Severity | high |
| Confidence | high |
| Confidence rationale | Validated by an independent worker and parent static source trace. |
| Category | privilege-escalation |
| CWE | CWE-732 |
| Affected lines | src-tauri/src/autostart.rs:14-21, src-tauri/tauri.conf.json:39-42, src-tauri/build.rs:27-31 |

#### Summary

ONLOGON/HIGHEST 工作保存可寫 current_exe 路徑；split-admin same-user attacker 可在程式退出後置換並於登入獲得無 UAC administrator execution。

#### Root Cause

ONLOGON/HIGHEST 工作保存可寫 current_exe 路徑；split-admin same-user attacker 可在程式退出後置換並於登入獲得無 UAC administrator execution。

#### Validation

Validated against current repository source with counterevidence and prerequisites retained.

Validation method: Manual static source trace

- **Status:** validated
- **Disposition:** reportable

#### Dataflow

ONLOGON/HIGHEST 工作保存可寫 current_exe 路徑；split-admin same-user attacker 可在程式退出後置換並於登入獲得無 UAC administrator execution。

#### Reachability

Source-backed conditional path described in summary.

#### Severity

**High** — Severity was calibrated from the validated attacker, prerequisites, impact and likelihood.

Additional runtime or deployment evidence could raise or lower this severity.

Impact assessment:
- **Level:** high
- **Rationale:** ONLOGON/HIGHEST 工作保存可寫 current_exe 路徑；split-admin same-user attacker 可在程式退出後置換並於登入獲得無 UAC administrator execution。

Likelihood assessment:
- **Level:** high
- **Rationale:** Calibrated from explicit prerequisites.

#### Remediation

只允許受保護、已驗證的 machine-wide executable/stub 作為 HIGHEST task target；驗證完整路徑 DACL/owner/reparse/signature。

<a id="finding-9"></a>

### [9] 未限定系統工具路徑可在提升後啟動攻擊者 binary

| Field | Value |
| --- | --- |
| Severity | high |
| Confidence | high |
| Confidence rationale | 三個 executable selector、啟動時 caller 與提升 manifest 均直接可見；部署可寫性與 Windows 搜尋目錄是明示的環境前提。 |
| Category | untrusted-search-path |
| CWE | CWE-426 |
| Affected lines | src-tauri/src/autostart.rs:54-59, src-tauri/src/tray.rs:79-90, src-tauri/src/update.rs:695-706, src-tauri/src/commands.rs:154-162, src-tauri/build.rs:27-31 |

#### Summary

提升權限的 host 以裸名稱啟動 schtasks、powershell 與 explorer；tray 初始化會無條件查詢 schtasks。若 application/current/search directory 可寫，攻擊者放置的同名 executable 可先於真正系統工具被解析並繼承 administrator token。

#### Root Cause

privileged process 把安全敏感 executable selection 交給環境搜尋，而非受保護絕對 identity。

**Startup launches bare schtasks** — `src-tauri/src/autostart.rs:54-59`

未指定 System32 絕對路徑，且此函式由 tray 初始化自動呼叫。

```Rust
Command::new("schtasks")
    .args(["/Query", "/TN", TASK_NAME])
    .creation_flags(CREATE_NO_WINDOW)
    .output()
```

**Updater launches bare PowerShell** — `src-tauri/src/update.rs:695-706`

portable updater 的 helper interpreter 同樣依名稱解析。

```Rust
std::process::Command::new("powershell")
    .args(["-WindowStyle", "Hidden", "-ExecutionPolicy", "Bypass", "-File"])
    .arg(&script_path)
    .spawn()
```

#### Validation

逐一追蹤三個 `Command::new` sink；參數採 argv，因此排除 shell-metacharacter injection，但裸 executable selector 仍未受保護。

Validation method: Manual static source trace against the current repository state

- **Status:** validated
- **Disposition:** reportable

Evidence:
- src-tauri/src/autostart.rs:54-59
- src-tauri/src/tray.rs:79-90
- src-tauri/src/update.rs:695-706
- src-tauri/src/commands.rs:154-162

Counterevidence and remaining uncertainty:
- 若有效 application/current/PATH directories 全由 high-integrity ACL 保護，binary planting 不成立。
- 真正 System32 工具存在不會自行證明它一定被解析。
- `.args()` 避免的是參數注入，不解決 executable resolution。

#### Dataflow

attacker 在 privileged child lookup 的較早可寫目錄種植 `schtasks.exe`（或其他同名工具）；main startup 建立 tray 並查詢 autostart；Rust/Windows process creation 解析 attacker binary 並用 inherited administrator token 啟動。

**Startup launches bare schtasks** — `src-tauri/src/autostart.rs:54-59`

未指定 System32 絕對路徑，且此函式由 tray 初始化自動呼叫。

```Rust
Command::new("schtasks")
    .args(["/Query", "/TN", TASK_NAME])
    .creation_flags(CREATE_NO_WINDOW)
    .output()
```

**Updater launches bare PowerShell** — `src-tauri/src/update.rs:695-706`

portable updater 的 helper interpreter 同樣依名稱解析。

```Rust
std::process::Command::new("powershell")
    .args(["-WindowStyle", "Hidden", "-ExecutionPolicy", "Bypass", "-File"])
    .arg(&script_path)
    .spawn()
```

#### Reachability

`build_tray` 到 `build_menu` 再到 `autostart::is_enabled` 是無互動 startup path；另外兩個 sink 在 portable update 與 open-data-folder 操作可達。

Preconditions:
- 至少一個早於受保護 System32 的有效 executable 搜尋目錄可由 attacker 寫入。
- 受害者啟動 PaceDock 並完成 UAC，或由 HIGHEST task 啟動。

Existing controls:
- child arguments 不經 cmd.exe。
- currentUser/portable deployment facts support but do not prove every concrete machine ACL。

#### Severity

**High** — `schtasks` sink 在每次 tray 建立時自動到達；對 currentUser/portable 可寫搜尋目錄中的 resident local attacker，影響為直接 administrator code execution。

Additional runtime or deployment evidence could raise or lower this severity.

Impact assessment:
- **Level:** high
- **Rationale:** 取得 administrator native code execution。

Likelihood assessment:
- **Level:** high
- **Rationale:** 在常見 user-writable portable/currentUser 或 attacker-controlled current directory 前提下，每次啟動自動觸發 schtasks lookup。

#### Remediation

以 GetSystemDirectoryW 等可信 API 組出 `%SystemRoot%\System32\schtasks.exe`、`explorer.exe` 與 Windows PowerShell 的絕對路徑，固定 child current directory 到受保護位置；可行時改用 Windows API，不依賴 PATH/PATHEXT/application/current directory。

Tests:
- 在測試 PATH/current/application directory 放置同名 fixture，確認新版本仍只執行解析後的 System32 file ID。
- 測試 WOW64/System32 路徑解析與不存在時 fail closed。

Preventive controls:
- 所有 privileged child selectors 使用 validated absolute paths。
- 不要繼承不可信 child CWD/PATH。

<a id="finding-10"></a>

### [10] 固定TEMP staging允許置換update script與PE

| Field | Value |
| --- | --- |
| Severity | high |
| Confidence | high |
| Confidence rationale | Independent worker plus parent source trace. |
| Category | privilege-escalation |
| CWE | CWE-367, CWE-377 |
| Affected lines | src-tauri/src/update.rs:460 |

#### Summary

portable updater在固定user TEMP寫入後重新依path執行/移動/啟動，未綁定驗證後file identity。

#### Validation

Validated against current source.

Validation method: Manual static source trace

- **Status:** validated
- **Disposition:** reportable

#### Dataflow

portable updater在固定user TEMP寫入後重新依path執行/移動/啟動，未綁定驗證後file identity。

#### Reachability

portable updater在固定user TEMP寫入後重新依path執行/移動/啟動，未綁定驗證後file identity。

#### Severity

**High** — Previously validated and calibrated.

Additional runtime or deployment evidence could raise or lower this severity.

#### Remediation

使用high-only隨機no-reparse staging與handle/file-ID binding。

<a id="finding-11"></a>

### [11] 最高權限排程工作指向可置換 executable

| Field | Value |
| --- | --- |
| Severity | high |
| Confidence | high |
| Confidence rationale | Independently reviewed and parent-validated against current source. |
| Category | privilege-escalation |
| CWE | CWE-732 |
| Affected lines | src-tauri/src/autostart.rs:14-21, src-tauri/tauri.conf.json:39-42 |

#### Summary

ONLOGON/HIGHEST task保存可寫current_exe pathname；split-admin same-user attacker可置換後於登入取得administrator execution。

#### Root Cause

ONLOGON/HIGHEST task保存可寫current_exe pathname；split-admin same-user attacker可置換後於登入取得administrator execution。

#### Validation

Validated against current source.

Validation method: Manual static source trace

- **Status:** validated
- **Disposition:** reportable

Evidence:
- src-tauri/src/autostart.rs:14
- src-tauri/tauri.conf.json:39

#### Dataflow

ONLOGON/HIGHEST task保存可寫current_exe pathname；split-admin same-user attacker可置換後於登入取得administrator execution。

#### Reachability

ONLOGON/HIGHEST task保存可寫current_exe pathname；split-admin same-user attacker可置換後於登入取得administrator execution。

#### Severity

**High** — Severity calibrated from established impact and likelihood.

Additional runtime or deployment evidence could raise or lower this severity.

Impact assessment:
- **Level:** high
- **Rationale:** ONLOGON/HIGHEST task保存可寫current_exe pathname；split-admin same-user attacker可置換後於登入取得administrator execution。

Likelihood assessment:
- **Level:** high
- **Rationale:** Conditional on the stated attacker prerequisites.

#### Remediation

使用受保護signed machine-wide target並驗證完整path ACL/owner/reparse。

<a id="finding-12"></a>

### [12] 未限定系統工具路徑可啟動攻擊者binary

| Field | Value |
| --- | --- |
| Severity | high |
| Confidence | high |
| Confidence rationale | Independently reviewed and parent-validated against current source. |
| Category | untrusted-search-path |
| CWE | CWE-426 |
| Affected lines | src-tauri/src/autostart.rs:54-59, src-tauri/src/tray.rs:79-90 |

#### Summary

elevated host以裸名稱解析schtasks/powershell/explorer，tray startup自動到達schtasks。

#### Root Cause

elevated host以裸名稱解析schtasks/powershell/explorer，tray startup自動到達schtasks。

#### Validation

Validated against current source.

Validation method: Manual static source trace

- **Status:** validated
- **Disposition:** reportable

Evidence:
- src-tauri/src/autostart.rs:54
- src-tauri/src/tray.rs:79

#### Dataflow

elevated host以裸名稱解析schtasks/powershell/explorer，tray startup自動到達schtasks。

#### Reachability

elevated host以裸名稱解析schtasks/powershell/explorer，tray startup自動到達schtasks。

#### Severity

**High** — Severity calibrated from established impact and likelihood.

Additional runtime or deployment evidence could raise or lower this severity.

Impact assessment:
- **Level:** high
- **Rationale:** elevated host以裸名稱解析schtasks/powershell/explorer，tray startup自動到達schtasks。

Likelihood assessment:
- **Level:** high
- **Rationale:** Conditional on the stated attacker prerequisites.

#### Remediation

使用System32可信絕對路徑或Windows API。

<a id="finding-13"></a>

### [13] 最高權限排程工作指向可由未提升程序置換的 executable

| Field | Value |
| --- | --- |
| Severity | high |
| Confidence | high |
| Confidence rationale | 排程參數、提升層級、target 選擇與缺少 ACL/owner/reparse/signature 驗證均可直接由 source 建立；實際安裝絕對路徑仍是明示前提。 |
| Category | privilege-escalation |
| CWE | CWE-732 |
| Affected lines | src-tauri/src/autostart.rs:14-21, src-tauri/tauri.conf.json:39-42, src-tauri/build.rs:27-31, README.md:68-70, README.md:92-97 |

#### Summary

啟用開機自啟後，ONLOGON/HIGHEST 工作只保存目前 executable 的路徑；在 currentUser 或使用者可寫的 portable 部署中，同帳戶 medium-integrity 程序可於程式退出後置換該檔，讓下次登入無 UAC 執行攻擊者程式。

#### Root Cause

信任邊界只綁定 mutable pathname，未把排程權限綁定到受保護且可驗證的 executable identity。

**HIGHEST task uses current executable** — `src-tauri/src/autostart.rs:14-21`

排程直接信任 current_exe 路徑，沒有檔案或父目錄完整性驗證。

```Rust
let exe = std::env::current_exe()?;
let tr = format!("\"{}\" --minimized", exe.display());
Command::new("schtasks").args(["/Create", "/TN", TASK_NAME, "/SC", "ONLOGON", "/RL", "HIGHEST", "/TR", &tr, "/F"])
```

#### Validation

source 確認 current_exe 被直接寫入 /RL HIGHEST ONLOGON task；路徑 quoting 只解決參數剖析，不阻止後續檔案置換。

Validation method: Manual static source trace against the current repository state

- **Status:** validated
- **Disposition:** reportable

Evidence:
- src-tauri/src/autostart.rs:14-21
- src-tauri/tauri.conf.json:39-42
- src-tauri/build.rs:27-31

Counterevidence and remaining uncertainty:
- autostart 預設關閉。
- 標準非管理員帳戶的 HIGHEST 不會憑空產生 administrator token。
- 若 target 位於 Program Files 類受保護樹且所有父路徑不可重導，攻擊前提不成立。

#### Dataflow

同帳戶 medium-integrity attacker 置換 current_exe 路徑的 PE；Task Scheduler 在 ONLOGON 解析同一路徑，使用 HIGHEST token 建立攻擊者程序。

**HIGHEST task uses current executable** — `src-tauri/src/autostart.rs:14-21`

排程直接信任 current_exe 路徑，沒有檔案或父目錄完整性驗證。

```Rust
let exe = std::env::current_exe()?;
let tr = format!("\"{}\" --minimized", exe.display());
Command::new("schtasks").args(["/Create", "/TN", TASK_NAME, "/SC", "ONLOGON", "/RL", "HIGHEST", "/TR", &tr, "/F"])
```

#### Reachability

入口是可寫的 task target path；sink 是 Task Scheduler 的 HIGHEST process creation。排程保存的是字串路徑，未保存原檔 file identity。

Preconditions:
- 受害者為具 split administrator token 的帳戶。
- 受害者曾啟用 PaceDock autostart。
- current_exe 或任一可重導父路徑可由 medium-integrity attacker 修改。

Existing controls:
- task action 的 executable path 有雙引號包覆。
- 建立 task 當下 PaceDock 已提升。

#### Severity

**High** — 此路徑可持續取得管理員程式碼執行；對使用 split administrator token、已啟用 autostart 且 target path 可寫的部署，利用只需檔案置換與下一次登入。

Additional runtime or deployment evidence could raise or lower this severity.

Impact assessment:
- **Level:** high
- **Rationale:** 取得持續、無 UAC 的 administrator native code execution。

Likelihood assessment:
- **Level:** high
- **Rationale:** 在明示的 split-admin、autostart enabled、writable-target 條件下，置換與登入觸發直接且可重複。

#### Remediation

不得為 medium-integrity 可寫位置建立 HIGHEST task。將主程式或最小 signed launcher 安裝到 machine-wide、Administrators/SYSTEM 才可修改的目錄；建立及每次更新後驗證所有父目錄與檔案的 DACL、owner、reparse 狀態及簽章。若做不到，將 task 降為 LIMITED 或拒絕啟用。

Tests:
- 在可寫測試目錄放置 signed benign fixture、建立 HIGHEST task 後置換檔案，確認新版本會拒絕建立或執行。
- 以 protected Program Files 與 portable writable directory 各測一次，驗證 ACL/reparse/signature policy。

Preventive controls:
- Privileged executable 必須位於 medium-integrity 不可寫位置。
- 排程建立前後綁定並驗證 executable identity。

<a id="finding-14"></a>

### [14] 最高權限排程工作指向可置換 executable

| Field | Value |
| --- | --- |
| Severity | high |
| Confidence | high |
| Confidence rationale | Independent worker plus parent source trace. |
| Category | privilege-escalation |
| CWE | CWE-732 |
| Affected lines | src-tauri/src/autostart.rs:14 |

#### Summary

ONLOGON/HIGHEST task保存可寫current_exe pathname；split-admin same-user attacker可置換後於登入取得administrator execution。

#### Validation

Validated against current source.

Validation method: Manual static source trace

- **Status:** validated
- **Disposition:** reportable

#### Dataflow

ONLOGON/HIGHEST task保存可寫current_exe pathname；split-admin same-user attacker可置換後於登入取得administrator execution。

#### Reachability

ONLOGON/HIGHEST task保存可寫current_exe pathname；split-admin same-user attacker可置換後於登入取得administrator execution。

#### Severity

**High** — Previously validated and calibrated.

Additional runtime or deployment evidence could raise or lower this severity.

#### Remediation

使用受保護signed machine-wide target並驗證完整path ACL/owner/reparse。

<a id="finding-15"></a>

### [15] 未限定系統工具路徑可在提升後啟動攻擊者 binary

| Field | Value |
| --- | --- |
| Severity | high |
| Confidence | high |
| Confidence rationale | 三個 executable selector、啟動時 caller 與提升 manifest 均直接可見；部署可寫性與 Windows 搜尋目錄是明示的環境前提。 |
| Category | untrusted-search-path |
| CWE | CWE-426 |
| Affected lines | src-tauri/src/autostart.rs:54-59, src-tauri/src/tray.rs:79-90, src-tauri/src/update.rs:695-706, src-tauri/src/commands.rs:154-162, src-tauri/build.rs:27-31 |

#### Summary

提升權限的 host 以裸名稱啟動 schtasks、powershell 與 explorer；tray 初始化會無條件查詢 schtasks。若 application/current/search directory 可寫，攻擊者放置的同名 executable 可先於真正系統工具被解析並繼承 administrator token。

#### Root Cause

privileged process 把安全敏感 executable selection 交給環境搜尋，而非受保護絕對 identity。

**Startup launches bare schtasks** — `src-tauri/src/autostart.rs:54-59`

未指定 System32 絕對路徑，且此函式由 tray 初始化自動呼叫。

```Rust
Command::new("schtasks")
    .args(["/Query", "/TN", TASK_NAME])
    .creation_flags(CREATE_NO_WINDOW)
    .output()
```

**Updater launches bare PowerShell** — `src-tauri/src/update.rs:695-706`

portable updater 的 helper interpreter 同樣依名稱解析。

```Rust
std::process::Command::new("powershell")
    .args(["-WindowStyle", "Hidden", "-ExecutionPolicy", "Bypass", "-File"])
    .arg(&script_path)
    .spawn()
```

#### Validation

逐一追蹤三個 `Command::new` sink；參數採 argv，因此排除 shell-metacharacter injection，但裸 executable selector 仍未受保護。

Validation method: Manual static source trace against the current repository state

- **Status:** validated
- **Disposition:** reportable

Evidence:
- src-tauri/src/autostart.rs:54-59
- src-tauri/src/tray.rs:79-90
- src-tauri/src/update.rs:695-706
- src-tauri/src/commands.rs:154-162

Counterevidence and remaining uncertainty:
- 若有效 application/current/PATH directories 全由 high-integrity ACL 保護，binary planting 不成立。
- 真正 System32 工具存在不會自行證明它一定被解析。
- `.args()` 避免的是參數注入，不解決 executable resolution。

#### Dataflow

attacker 在 privileged child lookup 的較早可寫目錄種植 `schtasks.exe`（或其他同名工具）；main startup 建立 tray 並查詢 autostart；Rust/Windows process creation 解析 attacker binary 並用 inherited administrator token 啟動。

**Startup launches bare schtasks** — `src-tauri/src/autostart.rs:54-59`

未指定 System32 絕對路徑，且此函式由 tray 初始化自動呼叫。

```Rust
Command::new("schtasks")
    .args(["/Query", "/TN", TASK_NAME])
    .creation_flags(CREATE_NO_WINDOW)
    .output()
```

**Updater launches bare PowerShell** — `src-tauri/src/update.rs:695-706`

portable updater 的 helper interpreter 同樣依名稱解析。

```Rust
std::process::Command::new("powershell")
    .args(["-WindowStyle", "Hidden", "-ExecutionPolicy", "Bypass", "-File"])
    .arg(&script_path)
    .spawn()
```

#### Reachability

`build_tray` 到 `build_menu` 再到 `autostart::is_enabled` 是無互動 startup path；另外兩個 sink 在 portable update 與 open-data-folder 操作可達。

Preconditions:
- 至少一個早於受保護 System32 的有效 executable 搜尋目錄可由 attacker 寫入。
- 受害者啟動 PaceDock 並完成 UAC，或由 HIGHEST task 啟動。

Existing controls:
- child arguments 不經 cmd.exe。
- currentUser/portable deployment facts support but do not prove every concrete machine ACL。

#### Severity

**High** — `schtasks` sink 在每次 tray 建立時自動到達；對 currentUser/portable 可寫搜尋目錄中的 resident local attacker，影響為直接 administrator code execution。

至少一個早於受保護 System32 的有效 executable 搜尋目錄可由 attacker 寫入。；受害者啟動 PaceDock 並完成 UAC，或由 HIGHEST task 啟動。

Impact assessment:
- **Level:** high
- **Rationale:** 取得 administrator native code execution。

Likelihood assessment:
- **Level:** high
- **Rationale:** 在常見 user-writable portable/currentUser 或 attacker-controlled current directory 前提下，每次啟動自動觸發 schtasks lookup。

#### Remediation

以 GetSystemDirectoryW 等可信 API 組出 `%SystemRoot%\System32\schtasks.exe`、`explorer.exe` 與 Windows PowerShell 的絕對路徑，固定 child current directory 到受保護位置；可行時改用 Windows API，不依賴 PATH/PATHEXT/application/current directory。

Tests:
- 在測試 PATH/current/application directory 放置同名 fixture，確認新版本仍只執行解析後的 System32 file ID。
- 測試 WOW64/System32 路徑解析與不存在時 fail closed。

Preventive controls:
- 所有 privileged child selectors 使用 validated absolute paths。
- 不要繼承不可信 child CWD/PATH。

<a id="finding-16"></a>

### [16] 可偽造的 recovery/restore JSON 驅動 elevated HKLM GPU mutation

| Field | Value |
| --- | --- |
| Severity | medium |
| Confidence | high |
| Confidence rationale | source完整建立 APPDATA路徑、無authenticity/schema bounds、寫入先於membership check及startup/IPC sinks。 |
| Category | external-control-of-system-setting |
| CWE | CWE-15, CWE-345 |
| Affected lines | src-tauri/src/benchmark/recovery.rs:83-91, src-tauri/src/benchmark/manager.rs:341-365, src-tauri/src/benchmark/manager.rs:380-388, src-tauri/src/benchmark/manager.rs:328-338, src-tauri/src/gpu.rs:286-312, src-tauri/src/gpu.rs:513-539, src-tauri/src/main.rs:97-100 |

#### Summary

`benchmark-recovery.json` 在啟動時自動處理，`gpu-restore.json` 可由 restore IPC處理；兩者都位於使用者 profile，只有 serde parsing。攻擊者可控制 adapter identity 與 registry type/bytes，使提升程序寫入或刪除兩個 GPU interrupt-policy values，並對相符的顯示裝置執行 disable/enable。

#### Root Cause

高權限 recovery protocol把可修改 JSON視為可信 transaction journal，沒有來源認證或 privileged schema validation。

**Untrusted recovery JSON is loaded** — `src-tauri/src/benchmark/recovery.rs:83-91`

只有JSON型別解析，沒有owner/MAC/size或semantic checks。

```Rust
let text = std::fs::read_to_string(path)?;
serde_json::from_str(&text).map(Some)
```

**Snapshot is written before adapter restart validation** — `src-tauri/src/gpu.rs:574-588`

attacker-shaped snapshot先寫入，readback只證明寫入結果等於輸入。

```Rust
backend.write_affinity_policy(snapshot)?;
backend.restart_device(&snapshot.instance_id, sleeper)?;
let current = backend.read_affinity_policy(&snapshot.instance_id)?;
```

#### Validation

確認 startup與manual restore共用restore_snapshot，兩者的修復相同；startup path無互動且在adapter membership check前寫registry，manual sibling另需user/renderer trigger。

Validation method: Manual static source trace against the current repository state

- **Status:** validated
- **Disposition:** reportable

Evidence:
- src-tauri/src/benchmark/recovery.rs:83-91
- src-tauri/src/benchmark/manager.rs:341-365
- src-tauri/src/benchmark/manager.rs:380-388
- src-tauri/src/gpu.rs:286-312
- src-tauri/src/gpu.rs:513-539

Counterevidence and remaining uncertainty:
- SnapshotTaken stage只讀取比較。
- restart_device只會命中目前存在display adapter；無效ID在registry write後失敗。
- 影響限於構造路徑下的 DevicePolicy 與 AssignmentSetOverride；manual restore另需trigger。

#### Dataflow

attacker在APPDATA建立或替換合法JSON；startup或restore command反序列化；restore_snapshot先把attacker type/bytes寫入HKLM，後檢查/重啟adapter並readback。

**Untrusted recovery JSON is loaded** — `src-tauri/src/benchmark/recovery.rs:83-91`

只有JSON型別解析，沒有owner/MAC/size或semantic checks。

```Rust
let text = std::fs::read_to_string(path)?;
serde_json::from_str(&text).map(Some)
```

**Snapshot is written before adapter restart validation** — `src-tauri/src/gpu.rs:574-588`

attacker-shaped snapshot先寫入，readback只證明寫入結果等於輸入。

```Rust
backend.write_affinity_policy(snapshot)?;
backend.restart_device(&snapshot.instance_id, sleeper)?;
let current = backend.read_affinity_policy(&snapshot.instance_id)?;
```

#### Reachability

startup sink在UI建立前自動可達；manual restore sink需使用者或renderer呼叫。選擇真實display adapter ID時，write/restart/readback均可成功。

Preconditions:
- attacker與受害者使用相同Windows profile且能寫APPDATA。
- 對完整裝置重啟影響，attacker需提供目前display adapter instance ID。

Existing controls:
- serde拒絕語法錯誤JSON。
- 互動式normal apply另有present-adapter、LP、BasicDisplay checks，但recovery/restore未重用完整集合。

#### Severity

**Medium** — 已證明的影響是受限 HKLM Enum 子樹中的兩個 fixed value 與顯示裝置重啟，可造成持久設定破壞/顯示中斷；未證明任意 HKLM write或code execution。

Additional runtime or deployment evidence could raise or lower this severity.

Impact assessment:
- **Level:** medium
- **Rationale:** 持久修改 GPU interrupt policy、顯示中斷、driver不穩或啟動阻斷。

Likelihood assessment:
- **Level:** high
- **Rationale:** same-user APPDATA write與victim startup是常見且直接；manual sibling需要額外trigger。

#### Remediation

將 recovery/restore state 存於 medium-integrity 不可寫的 ACL/MIC 位置，或以 only-elevated key 對內容做 MAC；讀取前限制檔案大小。要求 journal 與 snapshot instance ID 相等，在任何 registry write 前驗證目前 present display adapter；限制 DevicePolicy/AssignmentSetOverride type、長度及語意，並把 restore record 綁定最近一次可信 mutation。

Tests:
- 以medium-integrity建立PolicyApplied journal與present GPU fixture，確認新版本在任何registry write前拒絕。
- 測試oversized bytes、非DWORD DevicePolicy、\>8-byte mask、mismatched IDs、unknown adapter與forged gpu-restore.json。

Preventive controls:
- Authenticate privileged transaction journals。
- Validate target membership and exact schema before side effects。
- Bound untrusted persisted data sizes。

<a id="finding-17"></a>

### [17] production BenchmarkConfig 可選取未驗證 executable

| Field | Value |
| --- | --- |
| Severity | medium |
| Confidence | high |
| Confidence rationale | Validated by an independent worker and parent static source trace. |
| Category | externally-controlled-executable |
| CWE | CWE-73 |
| Affected lines | src-tauri/src/benchmark/mod.rs:114-127, src-tauri/src/benchmark/manager.rs:1501-1523, src-tauri/src/benchmark/runner.rs:1782-1816, src-tauri/src/benchmark/process_win.rs:140-149 |

#### Summary

production IPC/session中的workloadExePath/presentmonPath覆寫實際spawn路徑，existing verifier不綁定override；renderer或forged equivalent-session path可執行administrator PE。

#### Root Cause

production IPC/session中的workloadExePath/presentmonPath覆寫實際spawn路徑，existing verifier不綁定override；renderer或forged equivalent-session path可執行administrator PE。

#### Validation

Validated against current repository source with counterevidence and prerequisites retained.

Validation method: Manual static source trace

- **Status:** validated
- **Disposition:** reportable

#### Dataflow

production IPC/session中的workloadExePath/presentmonPath覆寫實際spawn路徑，existing verifier不綁定override；renderer或forged equivalent-session path可執行administrator PE。

#### Reachability

Source-backed conditional path described in summary.

#### Severity

**Medium** — Severity was calibrated from the validated attacker, prerequisites, impact and likelihood.

Additional runtime or deployment evidence could raise or lower this severity.

Impact assessment:
- **Level:** medium
- **Rationale:** production IPC/session中的workloadExePath/presentmonPath覆寫實際spawn路徑，existing verifier不綁定override；renderer或forged equivalent-session path可執行administrator PE。

Likelihood assessment:
- **Level:** medium
- **Rationale:** Calibrated from explicit prerequisites.

#### Remediation

移除production overrides；固定受保護路徑並驗證實際file identity。

<a id="finding-18"></a>

### [18] benchmark 接受或重讀未綁定本次 capture identity 的 CSV

| Field | Value |
| --- | --- |
| Severity | medium |
| Confidence | high |
| Confidence rationale | Independently reviewed and parent-validated against current source. |
| Category | benchmark-evidence-integrity |
| CWE | CWE-345, CWE-367 |
| Affected lines | src-tauri/src/benchmark/runner.rs:1893-1896, src-tauri/src/benchmark/runner.rs:1958-2026, src-tauri/src/benchmark/metrics.rs:94-194 |

#### Summary

stale CSV刪除失敗只警告，PresentMon exit code不參與success判斷；existing shaped CSV可被當成本次capture。

#### Root Cause

stale CSV刪除失敗只警告，PresentMon exit code不參與success判斷；existing shaped CSV可被當成本次capture。

#### Validation

Validated against current source.

Validation method: Manual static source trace

- **Status:** validated
- **Disposition:** reportable

Evidence:
- src-tauri/src/benchmark/runner.rs:1893
- src-tauri/src/benchmark/runner.rs:1958
- src-tauri/src/benchmark/metrics.rs:94

Counterevidence and remaining uncertainty:
- PresentMon spawn failure是fatal。
- parser要求finite positive frametime，時間欄存在時檢查monotonic/duration。
- valid prepositioned data仍可通過。

#### Dataflow

stale CSV刪除失敗只警告，PresentMon exit code不參與success判斷；existing shaped CSV可被當成本次capture。

#### Reachability

stale CSV刪除失敗只警告，PresentMon exit code不參與success判斷；existing shaped CSV可被當成本次capture。

Preconditions:
- attacker可寫/鎖benchmark APPDATA capture path。
- 使用者執行benchmark並後續套用結果。

#### Severity

**Medium** — same-user attacker可操控benchmark ranking與後續privileged GPU choice，造成GPU完整性/可用性影響；非直接code execution。

Additional runtime or deployment evidence could raise or lower this severity.

Impact assessment:
- **Level:** medium
- **Rationale:** stale CSV刪除失敗只警告，PresentMon exit code不參與success判斷；existing shaped CSV可被當成本次capture。

Likelihood assessment:
- **Level:** medium
- **Rationale:** Conditional on the stated attacker prerequisites.

#### Remediation

stale delete失敗即fail closed，要求PresentMon成功；每次capture用high-only隨機directory並綁定新file identity/creation。

<a id="finding-19"></a>

### [19] 可寫manifest與D3D9 exists-only無法保護elevated sidecars

| Field | Value |
| --- | --- |
| Severity | medium |
| Confidence | high |
| Confidence rationale | Independently reviewed and parent-validated against current source. |
| Category | missing-executable-integrity |
| CWE | CWE-353 |
| Affected lines | src-tauri/src/benchmark/assets.rs:71-96, src-tauri/src/benchmark/process_win.rs:140-149 |

#### Summary

manifest與sidecars共置可寫，D3D9只exists，benchmark可執行attacker PE。

#### Root Cause

manifest與sidecars共置可寫，D3D9只exists，benchmark可執行attacker PE。

#### Validation

Validated against current source.

Validation method: Manual static source trace

- **Status:** validated
- **Disposition:** reportable

Evidence:
- src-tauri/src/benchmark/assets.rs:71
- src-tauri/src/benchmark/process_win.rs:140

#### Dataflow

manifest與sidecars共置可寫，D3D9只exists，benchmark可執行attacker PE。

#### Reachability

manifest與sidecars共置可寫，D3D9只exists，benchmark可執行attacker PE。

#### Severity

**Medium** — Severity calibrated from established impact and likelihood.

Additional runtime or deployment evidence could raise or lower this severity.

Impact assessment:
- **Level:** medium
- **Rationale:** manifest與sidecars共置可寫，D3D9只exists，benchmark可執行attacker PE。

Likelihood assessment:
- **Level:** medium
- **Rationale:** Conditional on the stated attacker prerequisites.

#### Remediation

嵌入trust root、驗證固定三個sidecars並保護resource tree。

<a id="finding-20"></a>

### [20] 可修改的同目錄 manifest 與未驗證 D3D9 無法保護 elevated benchmark sidecars

| Field | Value |
| --- | --- |
| Severity | medium |
| Confidence | high |
| Confidence rationale | resource resolution、manifest parser、D3D9 exists-only check及兩個spawn sinks都能由source確認；initial bundle controls不涵蓋post-install replacement。 |
| Category | missing-executable-integrity |
| CWE | CWE-353 |
| Affected lines | src-tauri/src/benchmark/assets.rs:71-96, src-tauri/src/benchmark/assets.rs:99-128, src-tauri/resources/benchmark/SHA256SUMS:1-7, src-tauri/src/benchmark/runner.rs:1782-1816, src-tauri/src/benchmark/runner.rs:1898-1902, src-tauri/src/benchmark/process_win.rs:140-149, src-tauri/tauri.conf.json:39-42 |

#### Summary

PresentMon/lava的SHA256SUMS與binaries共置，parser不強制固定entries；同時可寫兩者的attacker可改manifest或讓它只驗證無關檔。D3D9更只做exists檢查。benchmark之後直接以administrator token執行選定sidecars。

#### Root Cause

manifest是與待驗證files同一可寫authority下的資料而非trust root，且D3D9完全沒有內容驗證。

**Manifest controls its own verification set** — `src-tauri/src/benchmark/assets.rs:71-96`

expected digests不是內嵌trust root，且D3D9只驗證存在。

```Rust
let expected = parse_manifest(&assets.manifest)?;
for (file, want_hash) in &expected { /* hash manifest-parent/file */ }
if !assets.d3d9_workload.exists() { return Err(...); }
```

**Parser accepts arbitrary nonempty entries** — `src-tauri/src/benchmark/assets.rs:99-128`

沒有要求manifest恰好包含實際將執行的PresentMon/lava，也未包含D3D9。

```Rust
for line in text.lines() { /* parse any <hex> <filename> */ out.push((file.to_string(), hash.to_lowercase())); }
if out.is_empty() { return Err(...); }
```

#### Validation

PresentMon/lava co-writable manifest與D3D9 exists-only是同一privileged-sidecar integrity invariant的不同實例，合併並保留每個sink。

Validation method: Manual static source trace against the current repository state

- **Status:** validated
- **Disposition:** reportable

Evidence:
- src-tauri/src/benchmark/assets.rs:71-128
- src-tauri/resources/benchmark/SHA256SUMS:1-7
- src-tauri/src/benchmark/runner.rs:1782-1816
- src-tauri/src/benchmark/runner.rs:1898-1902

Counterevidence and remaining uncertainty:
- official repository manifest目前包含PresentMon/lava固定digest。
- release build與portable ZIP checksum保護初始bundle。
- D3D9由repository source建置。
- 這些控制不阻止可寫部署樹的post-install replacement；protected directory會移除attacker influence。

#### Dataflow

attacker替換sidecar並同步修改/弱化同目錄manifest，或只替換D3D9；assets::verify通過或僅exists；runner直接spawn attacker PE。

**Manifest controls its own verification set** — `src-tauri/src/benchmark/assets.rs:71-96`

expected digests不是內嵌trust root，且D3D9只驗證存在。

```Rust
let expected = parse_manifest(&assets.manifest)?;
for (file, want_hash) in &expected { /* hash manifest-parent/file */ }
if !assets.d3d9_workload.exists() { return Err(...); }
```

**Parser accepts arbitrary nonempty entries** — `src-tauri/src/benchmark/assets.rs:99-128`

沒有要求manifest恰好包含實際將執行的PresentMon/lava，也未包含D3D9。

```Rust
for line in text.lines() { /* parse any <hex> <filename> */ out.push((file.to_string(), hash.to_lowercase())); }
if out.is_empty() { return Err(...); }
```

#### Reachability

entry是使用者啟動GPU benchmark；sink依選擇的Vulkan/D3D9及每次PresentMon capture而到達。

Preconditions:
- 部署的resources/benchmark可由same-user medium-integrity attacker寫入。
- 使用者啟動受影響benchmark模式。

Existing controls:
- initial release/build checks。
- Vulkan/PresentMon在未遭manifest一起修改時會檢出hash mismatch。

#### Severity

**Medium** — 影響是administrator code execution；但需可寫currentUser/portable resource tree並等待使用者開始相應benchmark，因此likelihood為medium。

部署的resources/benchmark可由same-user medium-integrity attacker寫入。；使用者啟動受影響benchmark模式。

Impact assessment:
- **Level:** high
- **Rationale:** 任意sidecar native code繼承administrator token。

Likelihood assessment:
- **Level:** medium
- **Rationale:** 需要可寫resource tree與使用者benchmark trigger；portable與未受保護currentUser部署提供條件式路徑。

#### Remediation

把每個正式sidecar的digest或verification public key嵌入受信任main executable，明確要求並驗證固定三個名稱、protected absolute paths及同一file identity；為D3D9產生per-build digest/signature。將app/resources安裝到medium-integrity不可寫位置，拒絕reparse points並在spawn前最後驗證。

Tests:
- 同時替換PresentMon與SHA256SUMS，確認新版本仍以embedded trust root拒絕。
- 將manifest改為只列無關檔案，確認拒絕。
- 替換D3D9後確認digest/signature failure且無child。

Preventive controls:
- Separate the trust root from writable sidecars。
- Require exact executable set and exact file identities。
- Protect resource tree ACL/MIC。

<a id="finding-21"></a>

### [21] 可寫manifest與D3D9 exists-only無法保護elevated sidecars

| Field | Value |
| --- | --- |
| Severity | medium |
| Confidence | high |
| Confidence rationale | Independent worker plus parent source trace. |
| Category | privilege-escalation |
| CWE | CWE-353 |
| Affected lines | src-tauri/src/benchmark/assets.rs:71 |

#### Summary

manifest與sidecars共置可寫，D3D9只exists，使用者benchmark可執行attacker PE。

#### Validation

Validated against current source.

Validation method: Manual static source trace

- **Status:** validated
- **Disposition:** reportable

#### Dataflow

manifest與sidecars共置可寫，D3D9只exists，使用者benchmark可執行attacker PE。

#### Reachability

manifest與sidecars共置可寫，D3D9只exists，使用者benchmark可執行attacker PE。

#### Severity

**Medium** — Previously validated and calibrated.

Additional runtime or deployment evidence could raise or lower this severity.

#### Remediation

嵌入trust root、驗證固定三個sidecars並保護resource tree。

<a id="finding-22"></a>

### [22] production BenchmarkConfig 可選取未驗證 executable 供 elevated runner 執行

| Field | Value |
| --- | --- |
| Severity | medium |
| Confidence | high |
| Confidence rationale | 欄位、IPC、session load、resolver、缺少validate_config限制及direct spawn均可由source逐段確認。 |
| Category | externally-controlled-executable |
| CWE | CWE-73 |
| Affected lines | src-tauri/src/benchmark/mod.rs:114-127, src-tauri/src/benchmark/ipc.rs:160-168, src-tauri/src/benchmark/manager.rs:703-768, src-tauri/src/benchmark/manager.rs:1501-1523, src-tauri/src/benchmark/runner.rs:1782-1816, src-tauri/src/benchmark/runner.rs:1898-1902, src-tauri/src/benchmark/process_win.rs:140-149 |

#### Summary

`workloadExePath` 與 `presentmonPath` 是 production IPC及session schema欄位；resolver直接覆寫要執行的paths，而 verifier沒有驗證實際 override。renderer code可直接指定，same-user attacker也可偽造符合Equivalent契約的session並等待使用者啟動安全驗證。

#### Root Cause

測試/除錯欄位被保留在production trust boundary，且完整性檢查沒有綁到最終選取的executable。

**Caller paths replace trusted assets** — `src-tauri/src/benchmark/manager.rs:1501-1523`

production resolver接受caller-controlled executable paths。

```Rust
if let Some(p) = &config.workload_exe_path { assets.d3d9_workload = PathBuf::from(p); }
if let Some(p) = &config.presentmon_path { assets.presentmon = PathBuf::from(p); }
```

**Selected path is spawned directly** — `src-tauri/src/benchmark/process_win.rs:140-149`

direct argv避免shell injection，但執行的是未驗證selected PE。

```Rust
let mut cmd = std::process::Command::new(exe);
cmd.args(args).creation_flags(CREATE_NO_WINDOW);
let mut child = cmd.spawn()?;
```

#### Validation

同時追蹤workload與PresentMon sibling sinks；兩者共享同一BenchmarkConfig/resolver缺陷與相同remediation，因此合併。

Validation method: Manual static source trace against the current repository state

- **Status:** validated
- **Disposition:** reportable

Evidence:
- src-tauri/src/benchmark/mod.rs:114-127
- src-tauri/src/benchmark/manager.rs:1501-1523
- src-tauri/src/benchmark/runner.rs:1782-1816
- src-tauri/src/benchmark/runner.rs:1898-1902
- src-tauri/src/benchmark/process_win.rs:140-149

Counterevidence and remaining uncertainty:
- shipped GpuTest UI傳入null且沒有選檔元件。
- repository未找到remote content、XSS或任意外部caller直接到main-window IPC。
- persisted session route需有效Equivalent session結構、當前CPU/GPU資料與使用者啟動validation。

#### Dataflow

attacker控制IPC BenchmarkConfig或user-writable session config；resolve_assets將其字串轉PathBuf；workload_command或capture直接選取該path；RealProcessRunner以繼承administrator tokenspawn。

**Caller paths replace trusted assets** — `src-tauri/src/benchmark/manager.rs:1501-1523`

production resolver接受caller-controlled executable paths。

```Rust
if let Some(p) = &config.workload_exe_path { assets.d3d9_workload = PathBuf::from(p); }
if let Some(p) = &config.presentmon_path { assets.presentmon = PathBuf::from(p); }
```

**Selected path is spawned directly** — `src-tauri/src/benchmark/process_win.rs:140-149`

direct argv避免shell injection，但執行的是未驗證selected PE。

```Rust
let mut cmd = std::process::Command::new(exe);
cmd.args(args).creation_flags(CREATE_NO_WINDOW);
let mut child = cmd.spawn()?;
```

#### Reachability

direct renderer path在start_gpu_benchmark可達；file path透過storage::get與equivalent validation背景runner可達。最終sink不經shell但會執行指定PE。

Preconditions:
- 直接路徑需控制main renderer code。
- 或可寫APPDATA並構造可進入Equivalent validation的session，再使使用者觸發。

Existing controls:
- normal UI固定送null。
- GPU/LP/BasicDisplay與benchmark single-flight checks不驗證executable provenance。

#### Severity

**Medium** — 成功可取得administrator code execution；但正常UI固定傳null，直接路徑需要renderer compromise，持久化session路徑需要精心偽造與使用者trigger，故likelihood不是high。

直接路徑需控制main renderer code。；或可寫APPDATA並構造可進入Equivalent validation的session，再使使用者觸發。

Impact assessment:
- **Level:** high
- **Rationale:** 任意native executable繼承PaceDock administrator token。

Likelihood assessment:
- **Level:** medium
- **Rationale:** 存在兩條source-backed entry，但正常UI不暴露欄位且session route需user trigger。

#### Remediation

從 production IPC與persisted schema移除 executable override，僅用 `cfg(test)`/test dependency injection。後端固定解析受保護root下的sidecars，並對runner實際要spawn的同一file identity驗證 embedded digest/signature。若產品確需custom workload，不得以host administrator token執行。

Tests:
- 向release build IPC傳非null override，確認後端拒絕且沒有建立child。
- 竄改session config兩欄後觸發Equivalent validation，確認在spawn前fail closed。

Preventive controls:
- Test-only dependency injection must not cross production IPC。
- Integrity checks must bind to the exact executed file identity。

<a id="finding-23"></a>

### [23] 可修改的同目錄 manifest 與未驗證 D3D9 無法保護 elevated benchmark sidecars

| Field | Value |
| --- | --- |
| Severity | medium |
| Confidence | high |
| Confidence rationale | resource resolution、manifest parser、D3D9 exists-only check及兩個spawn sinks都能由source確認；initial bundle controls不涵蓋post-install replacement。 |
| Category | missing-executable-integrity |
| CWE | CWE-353 |
| Affected lines | src-tauri/src/benchmark/assets.rs:71-96, src-tauri/src/benchmark/assets.rs:99-128, src-tauri/resources/benchmark/SHA256SUMS:1-7, src-tauri/src/benchmark/runner.rs:1782-1816, src-tauri/src/benchmark/runner.rs:1898-1902, src-tauri/src/benchmark/process_win.rs:140-149, src-tauri/tauri.conf.json:39-42 |

#### Summary

PresentMon/lava的SHA256SUMS與binaries共置，parser不強制固定entries；同時可寫兩者的attacker可改manifest或讓它只驗證無關檔。D3D9更只做exists檢查。benchmark之後直接以administrator token執行選定sidecars。

#### Root Cause

manifest是與待驗證files同一可寫authority下的資料而非trust root，且D3D9完全沒有內容驗證。

**Manifest controls its own verification set** — `src-tauri/src/benchmark/assets.rs:71-96`

expected digests不是內嵌trust root，且D3D9只驗證存在。

```Rust
let expected = parse_manifest(&assets.manifest)?;
for (file, want_hash) in &expected { /* hash manifest-parent/file */ }
if !assets.d3d9_workload.exists() { return Err(...); }
```

**Parser accepts arbitrary nonempty entries** — `src-tauri/src/benchmark/assets.rs:99-128`

沒有要求manifest恰好包含實際將執行的PresentMon/lava，也未包含D3D9。

```Rust
for line in text.lines() { /* parse any <hex> <filename> */ out.push((file.to_string(), hash.to_lowercase())); }
if out.is_empty() { return Err(...); }
```

#### Validation

PresentMon/lava co-writable manifest與D3D9 exists-only是同一privileged-sidecar integrity invariant的不同實例，合併並保留每個sink。

Validation method: Manual static source trace against the current repository state

- **Status:** validated
- **Disposition:** reportable

Evidence:
- src-tauri/src/benchmark/assets.rs:71-128
- src-tauri/resources/benchmark/SHA256SUMS:1-7
- src-tauri/src/benchmark/runner.rs:1782-1816
- src-tauri/src/benchmark/runner.rs:1898-1902

Counterevidence and remaining uncertainty:
- official repository manifest目前包含PresentMon/lava固定digest。
- release build與portable ZIP checksum保護初始bundle。
- D3D9由repository source建置。
- 這些控制不阻止可寫部署樹的post-install replacement；protected directory會移除attacker influence。

#### Dataflow

attacker替換sidecar並同步修改/弱化同目錄manifest，或只替換D3D9；assets::verify通過或僅exists；runner直接spawn attacker PE。

**Manifest controls its own verification set** — `src-tauri/src/benchmark/assets.rs:71-96`

expected digests不是內嵌trust root，且D3D9只驗證存在。

```Rust
let expected = parse_manifest(&assets.manifest)?;
for (file, want_hash) in &expected { /* hash manifest-parent/file */ }
if !assets.d3d9_workload.exists() { return Err(...); }
```

**Parser accepts arbitrary nonempty entries** — `src-tauri/src/benchmark/assets.rs:99-128`

沒有要求manifest恰好包含實際將執行的PresentMon/lava，也未包含D3D9。

```Rust
for line in text.lines() { /* parse any <hex> <filename> */ out.push((file.to_string(), hash.to_lowercase())); }
if out.is_empty() { return Err(...); }
```

#### Reachability

entry是使用者啟動GPU benchmark；sink依選擇的Vulkan/D3D9及每次PresentMon capture而到達。

Preconditions:
- 部署的resources/benchmark可由same-user medium-integrity attacker寫入。
- 使用者啟動受影響benchmark模式。

Existing controls:
- initial release/build checks。
- Vulkan/PresentMon在未遭manifest一起修改時會檢出hash mismatch。

#### Severity

**Medium** — 影響是administrator code execution；但需可寫currentUser/portable resource tree並等待使用者開始相應benchmark，因此likelihood為medium。

Additional runtime or deployment evidence could raise or lower this severity.

Impact assessment:
- **Level:** high
- **Rationale:** 任意sidecar native code繼承administrator token。

Likelihood assessment:
- **Level:** medium
- **Rationale:** 需要可寫resource tree與使用者benchmark trigger；portable與未受保護currentUser部署提供條件式路徑。

#### Remediation

把每個正式sidecar的digest或verification public key嵌入受信任main executable，明確要求並驗證固定三個名稱、protected absolute paths及同一file identity；為D3D9產生per-build digest/signature。將app/resources安裝到medium-integrity不可寫位置，拒絕reparse points並在spawn前最後驗證。

Tests:
- 同時替換PresentMon與SHA256SUMS，確認新版本仍以embedded trust root拒絕。
- 將manifest改為只列無關檔案，確認拒絕。
- 替換D3D9後確認digest/signature failure且無child。

Preventive controls:
- Separate the trust root from writable sidecars。
- Require exact executable set and exact file identities。
- Protect resource tree ACL/MIC。

<a id="finding-24"></a>

### [24] 可偽造 recovery/restore JSON 驅動 elevated HKLM GPU mutation

| Field | Value |
| --- | --- |
| Severity | medium |
| Confidence | high |
| Confidence rationale | Validated by an independent worker and parent static source trace. |
| Category | external-control-of-system-setting |
| CWE | CWE-15, CWE-345 |
| Affected lines | src-tauri/src/benchmark/recovery.rs:83-91, src-tauri/src/benchmark/manager.rs:341-365, src-tauri/src/benchmark/manager.rs:380-388, src-tauri/src/gpu.rs:286-312 |

#### Summary

user-writable recovery/restore JSON只有serde驗證；startup/manual restore把attacker-shapedpolicy寫入HKLM兩個GPU policy values並restart adapter。

#### Root Cause

user-writable recovery/restore JSON只有serde驗證；startup/manual restore把attacker-shapedpolicy寫入HKLM兩個GPU policy values並restart adapter。

#### Validation

Validated against current repository source with counterevidence and prerequisites retained.

Validation method: Manual static source trace

- **Status:** validated
- **Disposition:** reportable

#### Dataflow

user-writable recovery/restore JSON只有serde驗證；startup/manual restore把attacker-shapedpolicy寫入HKLM兩個GPU policy values並restart adapter。

#### Reachability

Source-backed conditional path described in summary.

#### Severity

**Medium** — Severity was calibrated from the validated attacker, prerequisites, impact and likelihood.

Additional runtime or deployment evidence could raise or lower this severity.

Impact assessment:
- **Level:** medium
- **Rationale:** user-writable recovery/restore JSON只有serde驗證；startup/manual restore把attacker-shapedpolicy寫入HKLM兩個GPU policy values並restart adapter。

Likelihood assessment:
- **Level:** medium
- **Rationale:** Calibrated from explicit prerequisites.

#### Remediation

以high-only storage或MAC保護；限制size/type/length，驗證ID一致及present adapter後才寫。

<a id="finding-25"></a>

### [25] production BenchmarkConfig 可選取未驗證 executable 供 elevated runner 執行

| Field | Value |
| --- | --- |
| Severity | medium |
| Confidence | high |
| Confidence rationale | 欄位、IPC、session load、resolver、缺少validate_config限制及direct spawn均可由source逐段確認。 |
| Category | externally-controlled-executable |
| CWE | CWE-73 |
| Affected lines | src-tauri/src/benchmark/mod.rs:114-127, src-tauri/src/benchmark/ipc.rs:160-168, src-tauri/src/benchmark/manager.rs:703-768, src-tauri/src/benchmark/manager.rs:1501-1523, src-tauri/src/benchmark/runner.rs:1782-1816, src-tauri/src/benchmark/runner.rs:1898-1902, src-tauri/src/benchmark/process_win.rs:140-149 |

#### Summary

`workloadExePath` 與 `presentmonPath` 是 production IPC及session schema欄位；resolver直接覆寫要執行的paths，而 verifier沒有驗證實際 override。renderer code可直接指定，same-user attacker也可偽造符合Equivalent契約的session並等待使用者啟動安全驗證。

#### Root Cause

測試/除錯欄位被保留在production trust boundary，且完整性檢查沒有綁到最終選取的executable。

**Caller paths replace trusted assets** — `src-tauri/src/benchmark/manager.rs:1501-1523`

production resolver接受caller-controlled executable paths。

```Rust
if let Some(p) = &config.workload_exe_path { assets.d3d9_workload = PathBuf::from(p); }
if let Some(p) = &config.presentmon_path { assets.presentmon = PathBuf::from(p); }
```

**Selected path is spawned directly** — `src-tauri/src/benchmark/process_win.rs:140-149`

direct argv避免shell injection，但執行的是未驗證selected PE。

```Rust
let mut cmd = std::process::Command::new(exe);
cmd.args(args).creation_flags(CREATE_NO_WINDOW);
let mut child = cmd.spawn()?;
```

#### Validation

同時追蹤workload與PresentMon sibling sinks；兩者共享同一BenchmarkConfig/resolver缺陷與相同remediation，因此合併。

Validation method: Manual static source trace against the current repository state

- **Status:** validated
- **Disposition:** reportable

Evidence:
- src-tauri/src/benchmark/mod.rs:114-127
- src-tauri/src/benchmark/manager.rs:1501-1523
- src-tauri/src/benchmark/runner.rs:1782-1816
- src-tauri/src/benchmark/runner.rs:1898-1902
- src-tauri/src/benchmark/process_win.rs:140-149

Counterevidence and remaining uncertainty:
- shipped GpuTest UI傳入null且沒有選檔元件。
- repository未找到remote content、XSS或任意外部caller直接到main-window IPC。
- persisted session route需有效Equivalent session結構、當前CPU/GPU資料與使用者啟動validation。

#### Dataflow

attacker控制IPC BenchmarkConfig或user-writable session config；resolve_assets將其字串轉PathBuf；workload_command或capture直接選取該path；RealProcessRunner以繼承administrator tokenspawn。

**Caller paths replace trusted assets** — `src-tauri/src/benchmark/manager.rs:1501-1523`

production resolver接受caller-controlled executable paths。

```Rust
if let Some(p) = &config.workload_exe_path { assets.d3d9_workload = PathBuf::from(p); }
if let Some(p) = &config.presentmon_path { assets.presentmon = PathBuf::from(p); }
```

**Selected path is spawned directly** — `src-tauri/src/benchmark/process_win.rs:140-149`

direct argv避免shell injection，但執行的是未驗證selected PE。

```Rust
let mut cmd = std::process::Command::new(exe);
cmd.args(args).creation_flags(CREATE_NO_WINDOW);
let mut child = cmd.spawn()?;
```

#### Reachability

direct renderer path在start_gpu_benchmark可達；file path透過storage::get與equivalent validation背景runner可達。最終sink不經shell但會執行指定PE。

Preconditions:
- 直接路徑需控制main renderer code。
- 或可寫APPDATA並構造可進入Equivalent validation的session，再使使用者觸發。

Existing controls:
- normal UI固定送null。
- GPU/LP/BasicDisplay與benchmark single-flight checks不驗證executable provenance。

#### Severity

**Medium** — 成功可取得administrator code execution；但正常UI固定傳null，直接路徑需要renderer compromise，持久化session路徑需要精心偽造與使用者trigger，故likelihood不是high。

Additional runtime or deployment evidence could raise or lower this severity.

Impact assessment:
- **Level:** high
- **Rationale:** 任意native executable繼承PaceDock administrator token。

Likelihood assessment:
- **Level:** medium
- **Rationale:** 存在兩條source-backed entry，但正常UI不暴露欄位且session route需user trigger。

#### Remediation

從 production IPC與persisted schema移除 executable override，僅用 `cfg(test)`/test dependency injection。後端固定解析受保護root下的sidecars，並對runner實際要spawn的同一file identity驗證 embedded digest/signature。若產品確需custom workload，不得以host administrator token執行。

Tests:
- 向release build IPC傳非null override，確認後端拒絕且沒有建立child。
- 竄改session config兩欄後觸發Equivalent validation，確認在spawn前fail closed。

Preventive controls:
- Test-only dependency injection must not cross production IPC。
- Integrity checks must bind to the exact executed file identity。

<a id="finding-26"></a>

### [26] 可偽造的 APPDATA state 驅動 elevated GPU mutation

| Field | Value |
| --- | --- |
| Severity | medium |
| Confidence | high |
| Confidence rationale | Independently reviewed and parent-validated against current source. |
| Category | external-control-of-system-setting |
| CWE | CWE-15, CWE-345 |
| Affected lines | src-tauri/src/benchmark/recovery.rs:83-91, src-tauri/src/benchmark/manager.rs:341-365, src-tauri/src/benchmark/storage.rs:99-112, src-tauri/src/benchmark/manager.rs:82-141 |

#### Summary

recovery/restore JSON可控制HKLM GPU policy；unsigned session.json的Passed/bestLp也會在語意檢查後驅動apply_best。所有資料位於same-user可寫APPDATA且缺少MAC/high-only storage。

#### Root Cause

recovery/restore JSON可控制HKLM GPU policy；unsigned session.json的Passed/bestLp也會在語意檢查後驅動apply_best。所有資料位於same-user可寫APPDATA且缺少MAC/high-only storage。

#### Validation

Validated against current source.

Validation method: Manual static source trace

- **Status:** validated
- **Disposition:** reportable

Evidence:
- src-tauri/src/benchmark/recovery.rs:83
- src-tauri/src/benchmark/manager.rs:341
- src-tauri/src/benchmark/storage.rs:99
- src-tauri/src/benchmark/manager.rs:82

Counterevidence and remaining uncertainty:
- apply_best仍要求Completed、Passed、current CPU fingerprint、present GPU及valid LP。
- session攻擊可修改bestLp但不能提供任意registry bytes；recovery path影響限於兩個fixed policy values。

#### Dataflow

recovery/restore JSON可控制HKLM GPU policy；unsigned session.json的Passed/bestLp也會在語意檢查後驅動apply_best。所有資料位於same-user可寫APPDATA且缺少MAC/high-only storage。

#### Reachability

recovery/restore JSON可控制HKLM GPU policy；unsigned session.json的Passed/bestLp也會在語意檢查後驅動apply_best。所有資料位於same-user可寫APPDATA且缺少MAC/high-only storage。

Preconditions:
- same-user medium-integrity actor可寫%APPDATA%\\PaceDock。
- session路徑另需使用者觸發apply。

#### Severity

**Medium** — 可造成受限HKLM GPU policy修改、display restart或操控best LP；影響為系統完整性/可用性但非任意registry/code execution。

Additional runtime or deployment evidence could raise or lower this severity.

Impact assessment:
- **Level:** medium
- **Rationale:** recovery/restore JSON可控制HKLM GPU policy；unsigned session.json的Passed/bestLp也會在語意檢查後驅動apply_best。所有資料位於same-user可寫APPDATA且缺少MAC/high-only storage。

Likelihood assessment:
- **Level:** high
- **Rationale:** Conditional on the stated attacker prerequisites.

#### Remediation

以high-only storage或MAC保護完整recovery/restore/session payload；限制schema，並在同一驗證snapshot上執行target checks與mutation。

<a id="finding-27"></a>

### [27] benchmark 接受或重讀未綁定本次 capture identity 的 CSV

| Field | Value |
| --- | --- |
| Severity | medium |
| Confidence | high |
| Confidence rationale | Independently reviewed and parent-validated against current source. |
| Category | benchmark-evidence-integrity |
| CWE | CWE-345, CWE-367 |
| Affected lines | src-tauri/src/benchmark/runner.rs:1893-1896, src-tauri/src/benchmark/runner.rs:1958-2026, src-tauri/src/benchmark/runner.rs:819-822, src-tauri/src/benchmark/runner.rs:2338-2426, src-tauri/src/benchmark/runner.rs:2485-2524, src-tauri/src/benchmark/metrics.rs:94-194 |

#### Summary

stale CSV刪除失敗只警告且PresentMon exit code不參與success；通過初始檢查後runner只保存PathBuf，ranking/confirmation/final stats再次從same-user可寫APPDATA開檔。攻擊者可預置或在驗證後替換valid CSV，操控winner與Passed reliability。

#### Root Cause

stale CSV刪除失敗只警告且PresentMon exit code不參與success；通過初始檢查後runner只保存PathBuf，ranking/confirmation/final stats再次從same-user可寫APPDATA開檔。攻擊者可預置或在驗證後替換valid CSV，操控winner與Passed reliability。

#### Validation

Validated against current source.

Validation method: Manual static source trace

- **Status:** validated
- **Disposition:** reportable

Evidence:
- src-tauri/src/benchmark/runner.rs:1893
- src-tauri/src/benchmark/runner.rs:1958
- src-tauri/src/benchmark/runner.rs:819
- src-tauri/src/benchmark/runner.rs:2338
- src-tauri/src/benchmark/runner.rs:2485
- src-tauri/src/benchmark/metrics.rs:94

Counterevidence and remaining uncertainty:
- 每次parse仍拒絕invalid numeric data。
- attacker可提供完全valid且設計過的CSV，因此format validation不建立provenance。

#### Dataflow

stale CSV刪除失敗只警告且PresentMon exit code不參與success；通過初始檢查後runner只保存PathBuf，ranking/confirmation/final stats再次從same-user可寫APPDATA開檔。攻擊者可預置或在驗證後替換valid CSV，操控winner與Passed reliability。

#### Reachability

stale CSV刪除失敗只警告且PresentMon exit code不參與success；通過初始檢查後runner只保存PathBuf，ranking/confirmation/final stats再次從same-user可寫APPDATA開檔。攻擊者可預置或在驗證後替換valid CSV，操控winner與Passed reliability。

Preconditions:
- attacker可在benchmark期間寫APPDATA capture paths。
- 使用者執行benchmark並信任/套用結果。

#### Severity

**Medium** — same-user attacker可操控benchmark evidence與後續privileged GPU choice，造成完整性/可用性影響；非直接code execution。

Additional runtime or deployment evidence could raise or lower this severity.

Impact assessment:
- **Level:** medium
- **Rationale:** stale CSV刪除失敗只警告且PresentMon exit code不參與success；通過初始檢查後runner只保存PathBuf，ranking/confirmation/final stats再次從same-user可寫APPDATA開檔。攻擊者可預置或在驗證後替換valid CSV，操控winner與Passed reliability。

Likelihood assessment:
- **Level:** medium
- **Rationale:** Conditional on the stated attacker prerequisites.

#### Remediation

stale delete失敗即fail closed，要求PresentMon成功；從受保護handle只解析一次並在記憶體保存immutable results，或保存/驗證file ID與digest；使用high-only隨機capture directory。

<a id="finding-28"></a>

### [28] production BenchmarkConfig可選未驗證executable

| Field | Value |
| --- | --- |
| Severity | medium |
| Confidence | high |
| Confidence rationale | Independently reviewed and parent-validated against current source. |
| Category | externally-controlled-executable |
| CWE | CWE-73 |
| Affected lines | src-tauri/src/benchmark/manager.rs:1501-1523, src-tauri/src/benchmark/process_win.rs:140-149 |

#### Summary

workloadExePath/presentmonPath覆寫實際elevated spawn，verifier未綁定override。

#### Root Cause

workloadExePath/presentmonPath覆寫實際elevated spawn，verifier未綁定override。

#### Validation

Validated against current source.

Validation method: Manual static source trace

- **Status:** validated
- **Disposition:** reportable

Evidence:
- src-tauri/src/benchmark/manager.rs:1501
- src-tauri/src/benchmark/process_win.rs:140

#### Dataflow

workloadExePath/presentmonPath覆寫實際elevated spawn，verifier未綁定override。

#### Reachability

workloadExePath/presentmonPath覆寫實際elevated spawn，verifier未綁定override。

#### Severity

**Medium** — Severity calibrated from established impact and likelihood.

Additional runtime or deployment evidence could raise or lower this severity.

Impact assessment:
- **Level:** medium
- **Rationale:** workloadExePath/presentmonPath覆寫實際elevated spawn，verifier未綁定override。

Likelihood assessment:
- **Level:** medium
- **Rationale:** Conditional on the stated attacker prerequisites.

#### Remediation

移除production overrides並驗證實際executed file identity。

<a id="finding-29"></a>

### [29] production BenchmarkConfig可選未驗證executable

| Field | Value |
| --- | --- |
| Severity | medium |
| Confidence | high |
| Confidence rationale | Independent worker plus parent source trace. |
| Category | privilege-escalation |
| CWE | CWE-73 |
| Affected lines | src-tauri/src/benchmark/manager.rs:1501 |

#### Summary

workloadExePath/presentmonPath覆寫實際elevated spawn，verifier未綁定override。

#### Validation

Validated against current source.

Validation method: Manual static source trace

- **Status:** validated
- **Disposition:** reportable

#### Dataflow

workloadExePath/presentmonPath覆寫實際elevated spawn，verifier未綁定override。

#### Reachability

workloadExePath/presentmonPath覆寫實際elevated spawn，verifier未綁定override。

#### Severity

**Medium** — Previously validated and calibrated.

Additional runtime or deployment evidence could raise or lower this severity.

#### Remediation

移除production overrides並驗證實際executed file identity。

<a id="finding-30"></a>

### [30] 偽造recovery/restore JSON驅動HKLM GPU mutation

| Field | Value |
| --- | --- |
| Severity | medium |
| Confidence | high |
| Confidence rationale | Independent worker plus parent source trace. |
| Category | privileged-state |
| CWE | CWE-15, CWE-345 |
| Affected lines | src-tauri/src/benchmark/recovery.rs:83 |

#### Summary

user-writable JSON未認證即控制兩個GPU policy values與adapter restart。

#### Validation

Validated against current source.

Validation method: Manual static source trace

- **Status:** validated
- **Disposition:** reportable

#### Dataflow

user-writable JSON未認證即控制兩個GPU policy values與adapter restart。

#### Reachability

user-writable JSON未認證即控制兩個GPU policy values與adapter restart。

#### Severity

**Medium** — Previously validated and calibrated.

Additional runtime or deployment evidence could raise or lower this severity.

#### Remediation

high-only/MAC state，限制schema並在write前驗證present adapter。

<a id="finding-31"></a>

### [31] benchmark 接受或重讀未綁定本次 capture identity 的 CSV

| Field | Value |
| --- | --- |
| Severity | medium |
| Confidence | high |
| Confidence rationale | stale-delete handling、unused exit code、PathBuf retention與later rereads均由current source直接建立。 |
| Category | benchmark-evidence-integrity |
| CWE | CWE-345, CWE-367 |
| Affected lines | src-tauri/src/benchmark/runner.rs:1893-1896, src-tauri/src/benchmark/runner.rs:1958-2026, src-tauri/src/benchmark/runner.rs:819-822, src-tauri/src/benchmark/runner.rs:2338-2426, src-tauri/src/benchmark/runner.rs:2485-2524, src-tauri/src/benchmark/metrics.rs:94-194 |

#### Summary

`run_capture` 對 stale CSV 刪除失敗只記 warning，且讀到 PresentMon exit code後不要求成功；因此既有 shaped CSV 可被視為本次 capture。通過完整性檢查後，runner 只保存 `PathBuf`，ranking、confirmation與final statistics又從 same-user 可寫的 APPDATA 重新開檔，讓攻擊者在驗證後替換有效資料並操控 winner 與 Passed reliability。

#### Root Cause

Capture freshness, producer success and post-validation file identity are not bound across the benchmark decision pipeline.

**Stale output deletion failure is ignored** — `src-tauri/src/benchmark/runner.rs:1893-1896`

A sharing violation or other deletion failure leaves existing attacker-controlled content in place and capture continues.

```Rust
if let Err(e) = std::fs::remove_file(csv) {
    if e.kind() != std::io::ErrorKind::NotFound {
        log::warn!("capture 前清除舊 CSV 失敗 {}: {e}", csv.display());
    }
}
```

**PresentMon exit code is not required for success** — `src-tauri/src/benchmark/runner.rs:1958-2026`

The observed exit code is diagnostic only; success depends on whichever CSV is present.

```Rust
let pm_exit_code = if wait_completed && !wait_timed_out {
    ctx.processes.exit_code(pm_pid)
} else { None };
...
CaptureWaitOutcome::Exited => match &integ.code {
    Some(c) => (Err(c.clone()), integ.reason.clone()),
    None => (Ok(()), None),
}
```

**Validated capture is retained only as a path** — `src-tauri/src/benchmark/runner.rs:819-822`

The validated bytes/file identity are not retained.

```Rust
round_csvs.entry(lp).or_default().insert(round, csv);
```

**Ranking reopens mutable CSV paths** — `src-tauri/src/benchmark/runner.rs:2355-2419`

Later decision phases consume a new read from the same user-writable pathname.

```Rust
let csv = &rounds[&round];
let frames = read_csv_frames(csv)?;
...
let text = std::fs::read_to_string(csv)?;
parse_presentmon_csv(&text)
```

#### Validation

The stale-file acceptance and post-validation reread paths were independently traced. Numeric and optional timing validation checks content shape, not current-run provenance or immutability.

Validation method: Manual static source trace

- **Status:** validated
- **Disposition:** reportable

Evidence:
- src-tauri/src/benchmark/runner.rs:1888-2030
- src-tauri/src/benchmark/runner.rs:2238-2290
- src-tauri/src/benchmark/runner.rs:2338-2426

Counterevidence and remaining uncertainty:
- PresentMon spawn failure is fatal.
- Parser rejects missing/invalid frametimes and checks monotonic/duration fields when present.
- An attacker can provide valid-shaped CSV, and TimeInSeconds may be absent, so these controls do not establish freshness.

#### Dataflow

Attacker prepositions and locks a valid CSV or replaces it after initial validation. `run_capture` continues after delete failure and ignores nonzero PresentMon exit; later ranking reopens paths and parses attacker-chosen valid data.

**Stale output deletion failure is ignored** — `src-tauri/src/benchmark/runner.rs:1893-1896`

A sharing violation or other deletion failure leaves existing attacker-controlled content in place and capture continues.

```Rust
if let Err(e) = std::fs::remove_file(csv) {
    if e.kind() != std::io::ErrorKind::NotFound {
        log::warn!("capture 前清除舊 CSV 失敗 {}: {e}", csv.display());
    }
}
```

**PresentMon exit code is not required for success** — `src-tauri/src/benchmark/runner.rs:1958-2026`

The observed exit code is diagnostic only; success depends on whichever CSV is present.

```Rust
let pm_exit_code = if wait_completed && !wait_timed_out {
    ctx.processes.exit_code(pm_pid)
} else { None };
...
CaptureWaitOutcome::Exited => match &integ.code {
    Some(c) => (Err(c.clone()), integ.reason.clone()),
    None => (Ok(()), None),
}
```

**Validated capture is retained only as a path** — `src-tauri/src/benchmark/runner.rs:819-822`

The validated bytes/file identity are not retained.

```Rust
round_csvs.entry(lp).or_default().insert(round, csv);
```

**Ranking reopens mutable CSV paths** — `src-tauri/src/benchmark/runner.rs:2355-2419`

Later decision phases consume a new read from the same user-writable pathname.

```Rust
let csv = &rounds[&round];
let frames = read_csv_frames(csv)?;
...
let text = std::fs::read_to_string(csv)?;
parse_presentmon_csv(&text)
```

#### Reachability

The attacker writes the same-user APPDATA benchmark directory while a user runs a benchmark; multiple warmups/rounds provide a practical mutation window.

- **Attacker:** Same-user medium-integrity process.

- **Entry point:** benchmarks/\<uuid\>/capture CSV paths

- **Sink:** candidate ranking, reliability decision and session bestLp

- **Outcome:** Forged winner/evidence that can later drive privileged GPU affinity.

Preconditions:
- Same-user attacker can write/lock APPDATA benchmark files.
- User runs and trusts the benchmark.
- User runs a benchmark and later trusts or applies the result.

Existing controls:
- CSV parser validates positive finite frametimes and optional monotonic/duration evidence.
- Process spawn failure and capture timeout/integrity-break paths fail closed.

**Stale output deletion failure is ignored** — `src-tauri/src/benchmark/runner.rs:1893-1896`

A sharing violation or other deletion failure leaves existing attacker-controlled content in place and capture continues.

```Rust
if let Err(e) = std::fs::remove_file(csv) {
    if e.kind() != std::io::ErrorKind::NotFound {
        log::warn!("capture 前清除舊 CSV 失敗 {}: {e}", csv.display());
    }
}
```

**PresentMon exit code is not required for success** — `src-tauri/src/benchmark/runner.rs:1958-2026`

The observed exit code is diagnostic only; success depends on whichever CSV is present.

```Rust
let pm_exit_code = if wait_completed && !wait_timed_out {
    ctx.processes.exit_code(pm_pid)
} else { None };
...
CaptureWaitOutcome::Exited => match &integ.code {
    Some(c) => (Err(c.clone()), integ.reason.clone()),
    None => (Ok(()), None),
}
```

**Validated capture is retained only as a path** — `src-tauri/src/benchmark/runner.rs:819-822`

The validated bytes/file identity are not retained.

```Rust
round_csvs.entry(lp).or_default().insert(round, csv);
```

**Ranking reopens mutable CSV paths** — `src-tauri/src/benchmark/runner.rs:2355-2419`

Later decision phases consume a new read from the same user-writable pathname.

```Rust
let csv = &rounds[&round];
let frames = read_csv_frames(csv)?;
...
let text = std::fs::read_to_string(csv)?;
parse_presentmon_csv(&text)
```

#### Severity

**Medium** — same-user attacker可破壞benchmark證據並影響後續privileged GPU affinity選擇，造成完整性/可用性影響；未形成直接code execution。

若capture directory對medium-integrity不可寫且fresh file identity與PresentMon成功狀態都被綁定，此finding不成立；若自動套用結果或可造成更廣泛system compromise，嚴重度提高。

Impact assessment:
- **Level:** medium
- **Rationale:** Controls benchmark evidence and downstream GPU policy selection.

Likelihood assessment:
- **Level:** medium
- **Rationale:** Requires local file access plus timing/user workflow, but fixed predictable paths and repeated reads give multiple opportunities.

#### Remediation

Treat any stale-output deletion error as fatal and require PresentMon to exit successfully. Create each capture in a random high-only directory and require the output not to exist beforehand. Read and parse exactly once from a protected handle into immutable memory; if rereads are unavoidable, bind every read to the saved Windows file ID, creation time and digest.

Tests:
- Hold a valid stale CSV open with delete-deny sharing and force PresentMon output failure; verify the run rejects before parsing.
- Replace a valid CSV after initial integrity assessment but before ranking; verify immutable in-memory results or digest/file-ID checks detect it.
- Return a nonzero PresentMon exit with a valid stale file and verify capture fails.

Preventive controls:
- Protected per-capture working directories.
- Producer exit-status and file-freshness binding.
- Single-read immutable evidence pipeline.

<a id="finding-32"></a>

### [32] 可偽造的 APPDATA state 驅動 elevated GPU policy mutation

| Field | Value |
| --- | --- |
| Severity | medium |
| Confidence | high |
| Confidence rationale | source完整建立 APPDATA路徑、無authenticity/schema bounds、寫入先於membership check及startup/IPC sinks。 |
| Category | external-control-of-system-setting |
| CWE | CWE-15, CWE-345 |
| Affected lines | src-tauri/src/benchmark/recovery.rs:83-91, src-tauri/src/benchmark/manager.rs:341-365, src-tauri/src/benchmark/manager.rs:380-388, src-tauri/src/benchmark/manager.rs:328-338, src-tauri/src/gpu.rs:286-312, src-tauri/src/gpu.rs:513-539, src-tauri/src/main.rs:97-100, src-tauri/src/benchmark/storage.rs:99-112, src-tauri/src/benchmark/manager.rs:82-141, src-tauri/src/benchmark/manager.rs:573-591 |

#### Summary

`benchmark-recovery.json`、`gpu-restore.json` 與 benchmark `session.json` 都位於 same-user 可寫的 APPDATA，卻被提升後端當作可信 privileged state。前兩者可提供任意 registry value type/bytes；session 可偽造 Passed/bestLp。資料分別流入 startup/manual restore 或 apply_best，改寫 HKLM GPU interrupt policy並可重啟顯示裝置。

#### Root Cause

High-integrity GPU operations trust mutable same-user profile JSON without protected storage, MAC, provenance binding, or a complete privileged schema validation.

**Untrusted recovery JSON is loaded** — `src-tauri/src/benchmark/recovery.rs:83-91`

只有JSON型別解析，沒有owner/MAC/size或semantic checks。

```Rust
let text = std::fs::read_to_string(path)?;
serde_json::from_str(&text).map(Some)
```

**Snapshot is written before adapter restart validation** — `src-tauri/src/gpu.rs:574-588`

attacker-shaped snapshot先寫入，readback只證明寫入結果等於輸入。

```Rust
backend.write_affinity_policy(snapshot)?;
backend.restart_device(&snapshot.instance_id, sleeper)?;
let current = backend.read_affinity_policy(&snapshot.instance_id)?;
```

**Unsigned session state is reloaded** — `src-tauri/src/benchmark/storage.rs:104-110`

UUID path validation prevents traversal but does not authenticate session contents.

```Rust
let path = dir.join("session.json");
let text = std::fs::read_to_string(&path)?;
let mut detail: SessionDetail = serde_json::from_str(&text)?;
```

**Session best LP reaches privileged mutation** — `src-tauri/src/benchmark/manager.rs:93-140`

Semantic checks constrain GPU/LP but do not prove the Passed result or best_lp was produced by the benchmark.

```Rust
let detail = storage::get_at(storage_root, session_id)?;
let best_lp = detail.summary.best_lp.ok_or_else(...)?;
apply_affinity_to_gpu(backend, sleeper, instance_id, best_lp, journal_path, restore_path)
```

#### Validation

Source validation confirmed three independent consumers sharing the same missing state-authenticity control: automatic startup recovery, manual restore and session-derived apply_best.

Validation method: Manual static source trace against the current repository state

- **Status:** validated
- **Disposition:** reportable

Evidence:
- src-tauri/src/benchmark/recovery.rs:83-91
- src-tauri/src/benchmark/manager.rs:328-388
- src-tauri/src/benchmark/storage.rs:99-112
- src-tauri/src/benchmark/manager.rs:82-141
- src-tauri/src/gpu.rs:286-312

Counterevidence and remaining uncertainty:
- apply_best requires Completed, Passed reliability, current CPU fingerprint, a present GPU and an in-range LP.
- Those checks constrain target/value but do not authenticate the stored evidence; an attacker can clone a valid session and change bestLp to another valid LP.
- Recovery/restore impact is limited to DevicePolicy and AssignmentSetOverride under the constructed HKLM Enum path; arbitrary HKLM writes or code execution were not established.

#### Dataflow

A medium-integrity same-user process replaces recovery/restore/session JSON in APPDATA. Elevated startup or a later user action deserializes it. Recovery/restore forwards attacker-shaped AffinityPolicy to restore_snapshot; apply_best forwards attacker-selected valid bestLp. Both reach HKLM policy writes and optional display-adapter restart.

**Untrusted recovery JSON is loaded** — `src-tauri/src/benchmark/recovery.rs:83-91`

只有JSON型別解析，沒有owner/MAC/size或semantic checks。

```Rust
let text = std::fs::read_to_string(path)?;
serde_json::from_str(&text).map(Some)
```

**Snapshot is written before adapter restart validation** — `src-tauri/src/gpu.rs:574-588`

attacker-shaped snapshot先寫入，readback只證明寫入結果等於輸入。

```Rust
backend.write_affinity_policy(snapshot)?;
backend.restart_device(&snapshot.instance_id, sleeper)?;
let current = backend.read_affinity_policy(&snapshot.instance_id)?;
```

**Unsigned session state is reloaded** — `src-tauri/src/benchmark/storage.rs:104-110`

UUID path validation prevents traversal but does not authenticate session contents.

```Rust
let path = dir.join("session.json");
let text = std::fs::read_to_string(&path)?;
let mut detail: SessionDetail = serde_json::from_str(&text)?;
```

**Session best LP reaches privileged mutation** — `src-tauri/src/benchmark/manager.rs:93-140`

Semantic checks constrain GPU/LP but do not prove the Passed result or best_lp was produced by the benchmark.

```Rust
let detail = storage::get_at(storage_root, session_id)?;
let best_lp = detail.summary.best_lp.ok_or_else(...)?;
apply_affinity_to_gpu(backend, sleeper, instance_id, best_lp, journal_path, restore_path)
```

#### Reachability

Startup recovery is automatic when a forged journal uses PolicyApplied/DeviceRestarted. Manual restore and session apply require a UI/renderer trigger. The attacker must know a present GPU instance ID for successful restart.

- **Attacker:** Same-user medium-integrity process able to write %APPDATA%\\PaceDock.

- **Entry point:** benchmark-recovery.json, gpu-restore.json or benchmarks/\<uuid\>/session.json

- **Sink:** RegSetValueExW/RegDeleteValueW and SetupAPI device restart

- **Outcome:** GPU policy integrity loss, display interruption or performance degradation.

Preconditions:
- attacker與受害者使用相同Windows profile且能寫APPDATA。
- 對完整裝置重啟影響，attacker需提供目前display adapter instance ID。
- Victim later launches PaceDock elevated; session/restore routes additionally require the corresponding user action.

Existing controls:
- serde拒絕語法錯誤JSON。
- 互動式normal apply另有present-adapter、LP、BasicDisplay checks，但recovery/restore未重用完整集合。

**Untrusted recovery JSON is loaded** — `src-tauri/src/benchmark/recovery.rs:83-91`

只有JSON型別解析，沒有owner/MAC/size或semantic checks。

```Rust
let text = std::fs::read_to_string(path)?;
serde_json::from_str(&text).map(Some)
```

**Snapshot is written before adapter restart validation** — `src-tauri/src/gpu.rs:574-588`

attacker-shaped snapshot先寫入，readback只證明寫入結果等於輸入。

```Rust
backend.write_affinity_policy(snapshot)?;
backend.restart_device(&snapshot.instance_id, sleeper)?;
let current = backend.read_affinity_policy(&snapshot.instance_id)?;
```

**Unsigned session state is reloaded** — `src-tauri/src/benchmark/storage.rs:104-110`

UUID path validation prevents traversal but does not authenticate session contents.

```Rust
let path = dir.join("session.json");
let text = std::fs::read_to_string(&path)?;
let mut detail: SessionDetail = serde_json::from_str(&text)?;
```

**Session best LP reaches privileged mutation** — `src-tauri/src/benchmark/manager.rs:93-140`

Semantic checks constrain GPU/LP but do not prove the Passed result or best_lp was produced by the benchmark.

```Rust
let detail = storage::get_at(storage_root, session_id)?;
let best_lp = detail.summary.best_lp.ok_or_else(...)?;
apply_affinity_to_gpu(backend, sleeper, instance_id, best_lp, journal_path, restore_path)
```

#### Severity

**Medium** — Medium system-integrity and availability impact with a high-likelihood local file boundary; no arbitrary registry write or code execution was proven.

Severity increases if these fixed values can be shown to yield code execution or broader device compromise; it decreases if deployment enforces medium-integrity-deny ACL/MIC or authenticated state.

Impact assessment:
- **Level:** medium
- **Rationale:** The path crosses into privileged HKLM GPU settings and display restart but remains bounded to fixed policy values and does not establish arbitrary code execution.

Likelihood assessment:
- **Level:** high
- **Rationale:** Same-user APPDATA write and later app startup are direct; apply/restore sibling routes require an additional visible user action.

#### Remediation

Store recovery, restore and benchmark evidence where medium-integrity processes cannot write, or authenticate complete payloads with a key only available to the elevated component. Bound file sizes and registry value types/lengths; require journal/snapshot IDs to agree; verify a present display adapter before any write. For session apply, MAC the config, results, reliability, CPU/GPU identity and selected LP, and apply exactly the verified in-memory snapshot.

Tests:
- 以medium-integrity建立PolicyApplied journal與present GPU fixture，確認新版本在任何registry write前拒絕。
- 測試oversized bytes、非DWORD DevicePolicy、\>8-byte mask、mismatched IDs、unknown adapter與forged gpu-restore.json。
- Clone a valid Passed session, change bestLp to another in-range LP and verify apply_best rejects the altered MAC before any registry mutation.
- Create forged recovery/restore JSON with mismatched IDs, arbitrary REG types, oversized bytes and an unknown adapter; verify all fail before RegCreateKeyExW.

Preventive controls:
- Authenticate privileged transaction journals。
- Validate target membership and exact schema before side effects。
- Bound untrusted persisted data sizes。
- Authenticate all privileged persisted state end to end.
- Use a protected high-integrity state directory with verified owner/DACL/MIC.

<a id="finding-33"></a>

### [33] 可修改同目錄 manifest 與未驗證 D3D9 無法保護 elevated sidecars

| Field | Value |
| --- | --- |
| Severity | medium |
| Confidence | high |
| Confidence rationale | Validated by an independent worker and parent static source trace. |
| Category | missing-executable-integrity |
| CWE | CWE-353 |
| Affected lines | src-tauri/src/benchmark/assets.rs:71-96, src-tauri/src/benchmark/assets.rs:99-128, src-tauri/src/benchmark/runner.rs:1782-1816, src-tauri/src/benchmark/process_win.rs:140-149 |

#### Summary

SHA256SUMS與sidecars共置且parser不要求固定entries，D3D9只exists；可寫resource tree的attacker可在benchmark trigger執行administrator code。

#### Root Cause

SHA256SUMS與sidecars共置且parser不要求固定entries，D3D9只exists；可寫resource tree的attacker可在benchmark trigger執行administrator code。

#### Validation

Validated against current repository source with counterevidence and prerequisites retained.

Validation method: Manual static source trace

- **Status:** validated
- **Disposition:** reportable

#### Dataflow

SHA256SUMS與sidecars共置且parser不要求固定entries，D3D9只exists；可寫resource tree的attacker可在benchmark trigger執行administrator code。

#### Reachability

Source-backed conditional path described in summary.

#### Severity

**Medium** — Severity was calibrated from the validated attacker, prerequisites, impact and likelihood.

Additional runtime or deployment evidence could raise or lower this severity.

Impact assessment:
- **Level:** medium
- **Rationale:** SHA256SUMS與sidecars共置且parser不要求固定entries，D3D9只exists；可寫resource tree的attacker可在benchmark trigger執行administrator code。

Likelihood assessment:
- **Level:** medium
- **Rationale:** Calibrated from explicit prerequisites.

#### Remediation

把固定digests/public key嵌入trusted main binary，驗證全部三個實際file identities並保護resource ACL。

<a id="finding-34"></a>

### [34] 可變 GitHub Action ref 接觸 signing key 與 release token

| Field | Value |
| --- | --- |
| Severity | low |
| Confidence | high |
| Confidence rationale | Validated by an independent worker and parent static source trace. |
| Category | ci-supply-chain |
| CWE | CWE-829 |
| Affected lines | .github/workflows/release.yml:122-127, .github/workflows/release.yml:8-9 |

#### Summary

tauri-action@v1不是immutable SHA，卻直接接收GITHUB_TOKEN與TAURI signing credentials；Action compromise可竊key/發布signed malicious update。

#### Root Cause

tauri-action@v1不是immutable SHA，卻直接接收GITHUB_TOKEN與TAURI signing credentials；Action compromise可竊key/發布signed malicious update。

#### Validation

Validated against current repository source with counterevidence and prerequisites retained.

Validation method: Manual static source trace

- **Status:** validated
- **Disposition:** reportable

#### Dataflow

tauri-action@v1不是immutable SHA，卻直接接收GITHUB_TOKEN與TAURI signing credentials；Action compromise可竊key/發布signed malicious update。

#### Reachability

Source-backed conditional path described in summary.

#### Severity

**Low** — Severity was calibrated from the validated attacker, prerequisites, impact and likelihood.

Additional runtime or deployment evidence could raise or lower this severity.

Impact assessment:
- **Level:** high
- **Rationale:** tauri-action@v1不是immutable SHA，卻直接接收GITHUB_TOKEN與TAURI signing credentials；Action compromise可竊key/發布signed malicious update。

Likelihood assessment:
- **Level:** low
- **Rationale:** Calibrated from explicit prerequisites.

#### Remediation

pin full SHAs；隔離approval-gated signing/upload job與least-privilege credentials。

<a id="finding-35"></a>

### [35] PE 診斷 parser 可因未終止名稱進入無限迴圈

| Field | Value |
| --- | --- |
| Severity | low |
| Confidence | high |
| Confidence rationale | Independently reviewed and parent-validated against current source. |
| Category | parser-denial-of-service |
| CWE | CWE-835 |
| Affected lines | scripts/pe-exports.mjs:30-43, scripts/pe-imports.mjs:36-59 |

#### Summary

兩個手動PE工具的cstr以`while (buf[end] !== 0) end++`掃描caller-selected binary；越界Buffer indexing回undefined，缺NUL時條件永遠成立並消耗CPU。

#### Root Cause

兩個手動PE工具的cstr以`while (buf[end] !== 0) end++`掃描caller-selected binary；越界Buffer indexing回undefined，缺NUL時條件永遠成立並消耗CPU。

#### Validation

Validated against current source.

Validation method: Manual static source trace

- **Status:** validated
- **Disposition:** reportable

Evidence:
- scripts/pe-exports.mjs:30
- scripts/pe-imports.mjs:36

Counterevidence and remaining uncertainty:
- 工具不被product runtime或CI自動呼叫。
- 其他out-of-range Buffer reads可能throw，但此indexed loop會持續。

#### Dataflow

兩個手動PE工具的cstr以`while (buf[end] !== 0) end++`掃描caller-selected binary；越界Buffer indexing回undefined，缺NUL時條件永遠成立並消耗CPU。

#### Reachability

兩個手動PE工具的cstr以`while (buf[end] !== 0) end++`掃描caller-selected binary；越界Buffer indexing回undefined，缺NUL時條件永遠成立並消耗CPU。

Preconditions:
- developer手動對attacker-crafted PE執行工具。

#### Severity

**Low** — 只影響手動developer diagnostic process的availability，沒有runtime/elevated/code-execution sink。

Additional runtime or deployment evidence could raise or lower this severity.

Impact assessment:
- **Level:** low
- **Rationale:** 兩個手動PE工具的cstr以`while (buf[end] !== 0) end++`掃描caller-selected binary；越界Buffer indexing回undefined，缺NUL時條件永遠成立並消耗CPU。

Likelihood assessment:
- **Level:** low
- **Rationale:** Conditional on the stated attacker prerequisites.

#### Remediation

所有PE offsets/counts先做bounds checks；cstr用bounded indexOf並拒絕missing NUL/超長string；限制section/name/descriptor/thunk counts。

<a id="finding-36"></a>

### [36] PE 診斷工具未中和名稱中的 terminal control sequences

| Field | Value |
| --- | --- |
| Severity | low |
| Confidence | high |
| Confidence rationale | raw ASCII decode到console.log的direct dataflow存在於兩個scripts，沒有任何escaping。 |
| Category | terminal-output-injection |
| CWE | CWE-150 |
| Affected lines | scripts/pe-exports.mjs:30-46, scripts/pe-imports.mjs:36-61 |

#### Summary

export/import DLL與function names由untrusted PE bytes直接ASCII decode，再插入`console.log`。ESC、CSI、OSC、CR、backspace等control bytes可由terminal解讀，讓crafted PE偽造/抹除diagnostic output或改變terminal title。

#### Root Cause

Diagnostic rendering does not encode untrusted binary metadata as inert text.

**Export names reach terminal unescaped** — `scripts/pe-exports.mjs:30-46`

Untrusted name bytes may contain terminal control characters and are emitted verbatim.

```JavaScript
return buf.toString('ascii', o, end);
...
console.log(out.join('\n'));
```

**Import names reach terminal unescaped** — `scripts/pe-imports.mjs:47-61`

DLL/function names can inject control sequences into the analyst terminal.

```JavaScript
const dll = cstr(rva2off(nameRva));
...
console.log(`\n${dll} (${funcs.length})`);
console.log('  ' + funcs.join(', '));
```

#### Validation

The source-to-terminal path is direct and contains no neutralization.

Validation method: Manual static output trace

- **Status:** validated
- **Disposition:** reportable

Evidence:
- scripts/pe-exports.mjs:30-46
- scripts/pe-imports.mjs:36-61

Counterevidence and remaining uncertainty:
- Values are not passed to a shell, filesystem or IPC.
- Terminal effects vary by emulator/configuration, so code execution is not claimed.
- The tools are manually invoked and not packaged runtime entry points.

#### Dataflow

Crafted PE embeds terminal control bytes in import/export names; parser decodes them and console.log writes them verbatim.

**Export names reach terminal unescaped** — `scripts/pe-exports.mjs:30-46`

Untrusted name bytes may contain terminal control characters and are emitted verbatim.

```JavaScript
return buf.toString('ascii', o, end);
...
console.log(out.join('\n'));
```

**Import names reach terminal unescaped** — `scripts/pe-imports.mjs:47-61`

DLL/function names can inject control sequences into the analyst terminal.

```JavaScript
const dll = cstr(rva2off(nameRva));
...
console.log(`\n${dll} (${funcs.length})`);
console.log('  ' + funcs.join(', '));
```

#### Reachability

Developer runs the diagnostic script in a terminal that interprets the supplied sequences.

- **Attacker:** Author of the inspected PE.

- **Entry point:** CLI-selected PE

- **Sink:** terminal output

- **Outcome:** Spoofed/erased output or changed terminal presentation.

Preconditions:
- Developer inspects attacker PE.
- Manual inspection in a control-sequence-aware terminal.

Existing controls:
- No downstream shell/command sink was found.

**Export names reach terminal unescaped** — `scripts/pe-exports.mjs:30-46`

Untrusted name bytes may contain terminal control characters and are emitted verbatim.

```JavaScript
return buf.toString('ascii', o, end);
...
console.log(out.join('\n'));
```

**Import names reach terminal unescaped** — `scripts/pe-imports.mjs:47-61`

DLL/function names can inject control sequences into the analyst terminal.

```JavaScript
const dll = cstr(rva2off(nameRva));
...
console.log(`\n${dll} (${funcs.length})`);
console.log('  ' + funcs.join(', '));
```

#### Severity

**Low** — 已建立的影響是手動developer terminal的diagnostic integrity；未建立shell、filesystem、IPC或command execution。

若輸出進入支援危險terminal extensions的環境或自動化log consumer，impact可能提高；把control bytes可視化後finding消失。

Impact assessment:
- **Level:** low
- **Rationale:** Diagnostic integrity and presentation only.

Likelihood assessment:
- **Level:** low
- **Rationale:** Requires manual tool use and terminal support.

#### Remediation

Emit structured JSON or a visible escaped representation. Replace C0/C1 controls, DEL, ESC, newline, carriage return, backspace and terminal-sensitive Unicode with `\xNN`/`\uNNNN` before display.

Tests:
- Create names containing ESC, CSI, OSC, CR, LF, backspace, DEL and C1 bytes; assert output contains only escaped printable text.
- Snapshot JSON output and ensure a terminal cannot interpret embedded controls.

Preventive controls:
- Escape untrusted terminal output by default.
- Prefer structured serialization for binary metadata.

<a id="finding-37"></a>

### [37] portable update 缺少獨立 publisher signature

| Field | Value |
| --- | --- |
| Severity | low |
| Confidence | high |
| Confidence rationale | source明確顯示兩個asset由同一release metadata選取、唯一信任判斷是SHA256，且release workflow用相同token上傳兩者。 |
| Category | insufficient-update-authenticity |
| CWE | CWE-494 |
| Affected lines | src-tauri/src/update.rs:248-294, src-tauri/src/update.rs:299-364, src-tauri/src/commands.rs:276-329, .github/workflows/release.yml:202-235, src-tauri/tauri.conf.json:47-54 |

#### Summary

portable updater從同一GitHub release取得ZIP與`.sha256`，一致即安裝；能修改release assets的actor可同時上傳惡意ZIP與配對checksum。installed updater已配置public key，但custom portable path沒有使用。

#### Root Cause

checksum與payload由相同發布authority控制，沒有獨立publisher identity trust root。

**Checksum comes from the same release** — `src-tauri/src/update.rs:358-360`

配對checksum不是不可由release publisher修改的signature。

```Rust
let expected_hex = fetch_and_parse_checksum(&release.checksum_asset)?;
verify_checksum(&buf, &expected_hex)?;
```

**One token uploads artifact and checksum** — `.github/workflows/release.yml:202-235`

發布端以相同authority產生並上傳兩者。

```YAML
$sha256 = (Get-FileHash -Path $zipName -Algorithm SHA256).Hash.ToLower()
gh release upload $tag $zipName "$zipName.sha256" --clobber
```

#### Validation

確認installed與portable更新路徑分離；只有installed plugin path使用配置pubkey，portable code只驗證same-release checksum。

Validation method: Manual static source trace against the current repository state

- **Status:** validated
- **Disposition:** reportable

Evidence:
- src-tauri/src/update.rs:248-364
- .github/workflows/release.yml:202-235
- src-tauri/tauri.conf.json:47-54

Counterevidence and remaining uncertainty:
- TLS、正式release篩選、semver、精確asset name、大小、ZIP magic/allowlist與SHA-256可防傳輸損壞或選錯資產。
- 它們不能抵抗可同時修改payload與checksum的publisher-side actor。

#### Dataflow

attacker取得release upload authority，發布較高版本惡意ZIP與配對checksum；client從同一release下載兩者；verify_checksum成功；extract/replacement安裝並執行。

**Checksum comes from the same release** — `src-tauri/src/update.rs:358-360`

配對checksum不是不可由release publisher修改的signature。

```Rust
let expected_hex = fetch_and_parse_checksum(&release.checksum_asset)?;
verify_checksum(&buf, &expected_hex)?;
```

**One token uploads artifact and checksum** — `.github/workflows/release.yml:202-235`

發布端以相同authority產生並上傳兩者。

```YAML
$sha256 = (Get-FileHash -Path $zipName -Algorithm SHA256).Hash.ToLower()
gh release upload $tag $zipName "$zipName.sha256" --clobber
```

#### Reachability

portable使用者檢查/安裝更新時可達。需要publisher-side compromise，不把一般network attacker視為可突破TLS。

Preconditions:
- attacker可修改目標GitHub release ZIP與checksum。
- portable使用者接受更新。

Existing controls:
- HTTPS與多項結構/大小檢查。
- installed updater的public-key signature不套用此custom path。

#### Severity

**Low** — 影響是所有portable使用者的administrator code execution，但攻擊前提是GitHub release publication authority或等價憑證已遭控制，依規則屬high impact、low likelihood。

attacker可修改目標GitHub release ZIP與checksum。；portable使用者接受更新。

Impact assessment:
- **Level:** high
- **Rationale:** 可向全部portable使用者散布administrator code。

Likelihood assessment:
- **Level:** low
- **Rationale:** 需先取得release publication authority或等價高價值憑證。

#### Remediation

以既有Tauri signing key或專用offline key對portable ZIP與version metadata簽章，將verification public key編入application；簽署內容綁定version、asset name、digest及release channel，缺簽章或rollback一律拒絕。

Tests:
- 建立測試release metadata含有效checksum但無signature，確認client拒絕。
- 修改version/name/digest任一欄位，確認signature binding失敗。

Preventive controls:
- Independent publisher signature for every update channel。
- Rollback/version binding。

<a id="finding-38"></a>

### [38] 第三方binary下載後才自生digest

| Field | Value |
| --- | --- |
| Severity | low |
| Confidence | high |
| Confidence rationale | Independent worker plus parent source trace. |
| Category | supply-chain |
| CWE | CWE-494 |
| Affected lines | scripts/fetch-benchmark-assets.mjs:59 |

#### Summary

refresh script不先驗證pretrusted digest/signature，下載後才生成SHA256SUMS。

#### Validation

Validated against current source.

Validation method: Manual static source trace

- **Status:** validated
- **Disposition:** reportable

#### Dataflow

refresh script不先驗證pretrusted digest/signature，下載後才生成SHA256SUMS。

#### Reachability

refresh script不先驗證pretrusted digest/signature，下載後才生成SHA256SUMS。

#### Severity

**Low** — Previously validated and calibrated.

Additional runtime or deployment evidence could raise or lower this severity.

#### Remediation

下載後先驗證known-good digest/publisher identity。

<a id="finding-39"></a>

### [39] 可變 GitHub Action ref 直接接觸 updater signing key 與 release token

| Field | Value |
| --- | --- |
| Severity | low |
| Confidence | high |
| Confidence rationale | workflow source直接證明mutable ref、job permission與secret injection；外部tag protection未知但不是必要source claim。 |
| Category | ci-supply-chain |
| CWE | CWE-829 |
| Affected lines | .github/workflows/release.yml:122-127, .github/workflows/release.yml:8-9, .github/workflows/release.yml:23-25 |

#### Summary

`tauri-apps/tauri-action@v1` 不是immutable commit SHA，卻在contents:write job中直接接收GITHUB_TOKEN、TAURI_SIGNING_PRIVATE_KEY及password；Action ref被上游移動或控制時可竊取key並發布有效簽章惡意更新。

#### Root Cause

長期高價值credentials交給由可移動名稱解析的第三方程式碼。

**Mutable action receives signing credentials** — `.github/workflows/release.yml:122-127`

major tag不是完整commit identity，且此step同時取得publish與signing authority。

```YAML
uses: tauri-apps/tauri-action@v1
env:
  GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
  TAURI_SIGNING_PRIVATE_KEY: ${{ secrets.TAURI_SIGNING_PRIVATE_KEY }}
```

#### Validation

確認外部PR不會直接觸發release job；這降低暴露，但不保護tag-triggered run免於Action ref本身被控制。

Validation method: Manual static source trace against the current repository state

- **Status:** validated
- **Disposition:** reportable

Evidence:
- .github/workflows/release.yml:8-9
- .github/workflows/release.yml:122-127

Counterevidence and remaining uncertainty:
- workflow只由v\* tag push觸發。
- secrets沒有傳給一般CI PR job。
- Action repository/tag是否實際遭控制未知，因此likelihood low。

#### Dataflow

attacker控制mutable tauri-action ref；tag release run下載攻擊者Action；同一步注入signing key與write token；Action exfiltrates key或上傳signed malicious artifact。

**Mutable action receives signing credentials** — `.github/workflows/release.yml:122-127`

major tag不是完整commit identity，且此step同時取得publish與signing authority。

```YAML
uses: tauri-apps/tauri-action@v1
env:
  GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
  TAURI_SIGNING_PRIVATE_KEY: ${{ secrets.TAURI_SIGNING_PRIVATE_KEY }}
```

#### Reachability

只有tag release workflow可達；不需PR content注入，但需Action upstream compromise或tag移動。

Preconditions:
- third-party action ref可被攻擊者控制。
- repository執行tag release workflow。

Existing controls:
- release job不由pull_request觸發。
- version/pubkey/secret presence checks不驗證Action implementation identity。

#### Severity

**Low** — 影響跨越全部installed users且長期signing key可能外洩，但需要third-party Action supply-chain compromise，屬high impact、low likelihood。

third-party action ref可被攻擊者控制。；repository執行tag release workflow。

Impact assessment:
- **Level:** high
- **Rationale:** 竊取長期updater key並發布可通過client驗證的惡意更新。

Likelihood assessment:
- **Level:** low
- **Rationale:** 需要受信任Action供應鏈被控制。

#### Remediation

將所有Actions固定到經審核完整commit SHA並用Dependabot/人工review更新；把untrusted build/test與sign/upload拆成jobs，只有environment-approved最終job可取得private key與least-privilege release token；優先使用短期/硬體或受控signing service。

Tests:
- 以policy/linter拒絕非40-hex SHA的uses refs。
- 驗證build job不可讀signing secrets，只有approval-gated signer可讀。

Preventive controls:
- Immutable action pinning。
- Credential isolation and least privilege。
- Protected release environments。

<a id="finding-40"></a>

### [40] portable update缺獨立publisher signature

| Field | Value |
| --- | --- |
| Severity | low |
| Confidence | high |
| Confidence rationale | Independently reviewed and parent-validated against current source. |
| Category | insufficient-update-authenticity |
| CWE | CWE-494 |
| Affected lines | src-tauri/src/update.rs:248-294 |

#### Summary

same-release ZIP/checksum可由publisher-side actor一起替換。

#### Root Cause

same-release ZIP/checksum可由publisher-side actor一起替換。

#### Validation

Validated against current source.

Validation method: Manual static source trace

- **Status:** validated
- **Disposition:** reportable

Evidence:
- src-tauri/src/update.rs:248

#### Dataflow

same-release ZIP/checksum可由publisher-side actor一起替換。

#### Reachability

same-release ZIP/checksum可由publisher-side actor一起替換。

#### Severity

**Low** — Severity calibrated from established impact and likelihood.

Additional runtime or deployment evidence could raise or lower this severity.

Impact assessment:
- **Level:** low
- **Rationale:** same-release ZIP/checksum可由publisher-side actor一起替換。

Likelihood assessment:
- **Level:** low
- **Rationale:** Conditional on the stated attacker prerequisites.

#### Remediation

以embedded public key驗證portable artifact與metadata signature。

<a id="finding-41"></a>

### [41] 第三方 benchmark executable 下載後才從自身產生 digest

| Field | Value |
| --- | --- |
| Severity | low |
| Confidence | high |
| Confidence rationale | Validated by an independent worker and parent static source trace. |
| Category | build-supply-chain |
| CWE | CWE-494 |
| Affected lines | scripts/fetch-benchmark-assets.mjs:59-64, scripts/fetch-benchmark-assets.mjs:67-108 |

#### Summary

refresh script不先驗證known-good digest/signature，下載後才生成SHA256SUMS；upstream replacement可在manual refresh後進入elevated runtime。

#### Root Cause

refresh script不先驗證known-good digest/signature，下載後才生成SHA256SUMS；upstream replacement可在manual refresh後進入elevated runtime。

#### Validation

Validated against current repository source with counterevidence and prerequisites retained.

Validation method: Manual static source trace

- **Status:** validated
- **Disposition:** reportable

#### Dataflow

refresh script不先驗證known-good digest/signature，下載後才生成SHA256SUMS；upstream replacement可在manual refresh後進入elevated runtime。

#### Reachability

Source-backed conditional path described in summary.

#### Severity

**Low** — Severity was calibrated from the validated attacker, prerequisites, impact and likelihood.

Additional runtime or deployment evidence could raise or lower this severity.

Impact assessment:
- **Level:** high
- **Rationale:** refresh script不先驗證known-good digest/signature，下載後才生成SHA256SUMS；upstream replacement可在manual refresh後進入elevated runtime。

Likelihood assessment:
- **Level:** low
- **Rationale:** Calibrated from explicit prerequisites.

#### Remediation

下載後先對pretrusted digest或publisher identity驗證；digest更新獨立review。

<a id="finding-42"></a>

### [42] 第三方binary下載後才自生digest

| Field | Value |
| --- | --- |
| Severity | low |
| Confidence | high |
| Confidence rationale | Independently reviewed and parent-validated against current source. |
| Category | build-supply-chain |
| CWE | CWE-494 |
| Affected lines | scripts/fetch-benchmark-assets.mjs:59-108 |

#### Summary

refresh script不先驗證pretrusted digest/signature，下載後才生成SHA256SUMS。

#### Root Cause

refresh script不先驗證pretrusted digest/signature，下載後才生成SHA256SUMS。

#### Validation

Validated against current source.

Validation method: Manual static source trace

- **Status:** validated
- **Disposition:** reportable

Evidence:
- scripts/fetch-benchmark-assets.mjs:59

#### Dataflow

refresh script不先驗證pretrusted digest/signature，下載後才生成SHA256SUMS。

#### Reachability

refresh script不先驗證pretrusted digest/signature，下載後才生成SHA256SUMS。

#### Severity

**Low** — Severity calibrated from established impact and likelihood.

Additional runtime or deployment evidence could raise or lower this severity.

Impact assessment:
- **Level:** low
- **Rationale:** refresh script不先驗證pretrusted digest/signature，下載後才生成SHA256SUMS。

Likelihood assessment:
- **Level:** low
- **Rationale:** Conditional on the stated attacker prerequisites.

#### Remediation

下載後先驗證known-good digest/publisher identity。

<a id="finding-43"></a>

### [43] 可變 GitHub Action ref 直接接觸 updater signing key 與 release token

| Field | Value |
| --- | --- |
| Severity | low |
| Confidence | high |
| Confidence rationale | workflow source直接證明mutable ref、job permission與secret injection；外部tag protection未知但不是必要source claim。 |
| Category | ci-supply-chain |
| CWE | CWE-829 |
| Affected lines | .github/workflows/release.yml:122-127, .github/workflows/release.yml:8-9, .github/workflows/release.yml:23-25 |

#### Summary

`tauri-apps/tauri-action@v1` 不是immutable commit SHA，卻在contents:write job中直接接收GITHUB_TOKEN、TAURI_SIGNING_PRIVATE_KEY及password；Action ref被上游移動或控制時可竊取key並發布有效簽章惡意更新。

#### Root Cause

長期高價值credentials交給由可移動名稱解析的第三方程式碼。

**Mutable action receives signing credentials** — `.github/workflows/release.yml:122-127`

major tag不是完整commit identity，且此step同時取得publish與signing authority。

```YAML
uses: tauri-apps/tauri-action@v1
env:
  GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
  TAURI_SIGNING_PRIVATE_KEY: ${{ secrets.TAURI_SIGNING_PRIVATE_KEY }}
```

#### Validation

確認外部PR不會直接觸發release job；這降低暴露，但不保護tag-triggered run免於Action ref本身被控制。

Validation method: Manual static source trace against the current repository state

- **Status:** validated
- **Disposition:** reportable

Evidence:
- .github/workflows/release.yml:8-9
- .github/workflows/release.yml:122-127

Counterevidence and remaining uncertainty:
- workflow只由v\* tag push觸發。
- secrets沒有傳給一般CI PR job。
- Action repository/tag是否實際遭控制未知，因此likelihood low。

#### Dataflow

attacker控制mutable tauri-action ref；tag release run下載攻擊者Action；同一步注入signing key與write token；Action exfiltrates key或上傳signed malicious artifact。

**Mutable action receives signing credentials** — `.github/workflows/release.yml:122-127`

major tag不是完整commit identity，且此step同時取得publish與signing authority。

```YAML
uses: tauri-apps/tauri-action@v1
env:
  GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
  TAURI_SIGNING_PRIVATE_KEY: ${{ secrets.TAURI_SIGNING_PRIVATE_KEY }}
```

#### Reachability

只有tag release workflow可達；不需PR content注入，但需Action upstream compromise或tag移動。

Preconditions:
- third-party action ref可被攻擊者控制。
- repository執行tag release workflow。

Existing controls:
- release job不由pull_request觸發。
- version/pubkey/secret presence checks不驗證Action implementation identity。

#### Severity

**Low** — 影響跨越全部installed users且長期signing key可能外洩，但需要third-party Action supply-chain compromise，屬high impact、low likelihood。

Additional runtime or deployment evidence could raise or lower this severity.

Impact assessment:
- **Level:** high
- **Rationale:** 竊取長期updater key並發布可通過client驗證的惡意更新。

Likelihood assessment:
- **Level:** low
- **Rationale:** 需要受信任Action供應鏈被控制。

#### Remediation

將所有Actions固定到經審核完整commit SHA並用Dependabot/人工review更新；把untrusted build/test與sign/upload拆成jobs，只有environment-approved最終job可取得private key與least-privilege release token；優先使用短期/硬體或受控signing service。

Tests:
- 以policy/linter拒絕非40-hex SHA的uses refs。
- 驗證build job不可讀signing secrets，只有approval-gated signer可讀。

Preventive controls:
- Immutable action pinning。
- Credential isolation and least privilege。
- Protected release environments。

<a id="finding-44"></a>

### [44] PE 診斷 parser 可因未終止名稱進入無限迴圈

| Field | Value |
| --- | --- |
| Severity | low |
| Confidence | high |
| Confidence rationale | 兩個相同unbounded loop與attacker-controlled RVA call sites都可由source直接確認。 |
| Category | parser-denial-of-service |
| CWE | CWE-835 |
| Affected lines | scripts/pe-exports.mjs:30-43, scripts/pe-imports.mjs:36-59 |

#### Summary

兩個手動 PE 工具的 `cstr` 以 `while (buf[end] !== 0) end++` 掃描 caller-selected binary，沒有buffer bound。若RVA指向EOF前沒有NUL的名稱，Node Buffer越界索引為`undefined`，條件永遠為true並持續耗用CPU。

#### Root Cause

The parsers treat PE offsets, counts and C strings as trusted and do not bound string scans by the input buffer.

**Unbounded export-name scan** — `scripts/pe-exports.mjs:30-43`

The name offset is derived from the PE and the scan has no end condition at buf.length.

```JavaScript
const cstr = (o) => {
  let end = o;
  while (buf[end] !== 0) end++;
  return buf.toString('ascii', o, end);
};
...
names.push(cstr(rva2off(u32(namesOff + i * 4))));
```

**Unbounded import-name scan** — `scripts/pe-imports.mjs:36-59`

Malformed DLL/function names can enter the same unreachable-exit loop.

```JavaScript
const cstr = (o) => {
  let end = o;
  while (buf[end] !== 0) end++;
  return buf.toString('ascii', o, end);
};
...
const dll = cstr(rva2off(nameRva));
```

#### Validation

Manual static validation confirms indexed reads past Node Buffer return undefined, so the equality condition never reaches zero on an unterminated string.

Validation method: Manual static parser trace

- **Status:** validated
- **Disposition:** reportable

Evidence:
- scripts/pe-exports.mjs:30-43
- scripts/pe-imports.mjs:36-59

Counterevidence and remaining uncertainty:
- Other out-of-range Buffer numeric reads may throw, but that does not terminate this indexed-byte loop.
- The scripts are opt-in developer utilities.

#### Dataflow

Attacker supplies a PE with a valid table RVA pointing to an unterminated name; cstr increments beyond EOF forever.

**Unbounded export-name scan** — `scripts/pe-exports.mjs:30-43`

The name offset is derived from the PE and the scan has no end condition at buf.length.

```JavaScript
const cstr = (o) => {
  let end = o;
  while (buf[end] !== 0) end++;
  return buf.toString('ascii', o, end);
};
...
names.push(cstr(rva2off(u32(namesOff + i * 4))));
```

**Unbounded import-name scan** — `scripts/pe-imports.mjs:36-59`

Malformed DLL/function names can enter the same unreachable-exit loop.

```JavaScript
const cstr = (o) => {
  let end = o;
  while (buf[end] !== 0) end++;
  return buf.toString('ascii', o, end);
};
...
const dll = cstr(rva2off(nameRva));
```

#### Reachability

Developer explicitly invokes either diagnostic script on the crafted PE.

- **Attacker:** Author of an untrusted PE inspected by a developer.

- **Entry point:** CLI file path

- **Sink:** unbounded cstr loop

- **Outcome:** CPU-bound hang until termination.

Preconditions:
- Developer runs the script on attacker-controlled PE.
- Manual inspection of attacker-provided PE.

Existing controls:
- Some structural errors throw through Buffer read APIs; no bound protects the C-string loop.

**Unbounded export-name scan** — `scripts/pe-exports.mjs:30-43`

The name offset is derived from the PE and the scan has no end condition at buf.length.

```JavaScript
const cstr = (o) => {
  let end = o;
  while (buf[end] !== 0) end++;
  return buf.toString('ascii', o, end);
};
...
names.push(cstr(rva2off(u32(namesOff + i * 4))));
```

**Unbounded import-name scan** — `scripts/pe-imports.mjs:36-59`

Malformed DLL/function names can enter the same unreachable-exit loop.

```JavaScript
const cstr = (o) => {
  let end = o;
  while (buf[end] !== 0) end++;
  return buf.toString('ascii', o, end);
};
...
const dll = cstr(rva2off(nameRva));
```

#### Severity

**Low** — 影響限於手動developer diagnostic process availability；工具不由product runtime/CI自動呼叫，也沒有elevated或code-execution sink。

若工具被放進自動化服務或處理遠端上傳PE，likelihood與availability impact會提高；加入完整bounds/count限制後finding消失。

Impact assessment:
- **Level:** low
- **Rationale:** Single diagnostic process availability loss.

Likelihood assessment:
- **Level:** low
- **Rationale:** Requires manual developer use on hostile input.

#### Remediation

Validate DOS/PE/optional-header/section/RVA/raw-file bounds before every read. Implement cstr with bounded `buf.indexOf(0, offset)` and reject missing terminators or excessive length. Cap section, export-name, import-descriptor and thunk counts.

Tests:
- Provide export/import names with no NUL before EOF and assert bounded rejection within a fixed time.
- Fuzz every table offset/count and assert no unbounded loop or out-of-range read.

Preventive controls:
- Bound every attacker-derived parse offset/count.
- Parser-wide work budget and maximum string length.

<a id="finding-45"></a>

### [45] 第三方 benchmark executable 下載後才從自身產生信任 digest

| Field | Value |
| --- | --- |
| Severity | low |
| Confidence | high |
| Confidence rationale | download、copy與digest生成順序由script完整建立；正常release不自動執行refresh是重要counterevidence。 |
| Category | build-supply-chain |
| CWE | CWE-494 |
| Affected lines | scripts/fetch-benchmark-assets.mjs:59-64, scripts/fetch-benchmark-assets.mjs:67-108, package.json:13-16, src-tauri/src/benchmark/assets.rs:71-96 |

#### Summary

`fetch:benchmark-assets` 從versioned URLs下載PresentMon與AutoGpuAffinity，僅檢查curl成功/檔案存在；接著對未驗證bytes計算SHA256並覆寫manifest。上游asset被替換時，後續build/runtime檢查只證明惡意binary與自生manifest一致。

#### Root Cause

vendor refresh缺少下載前的獨立完整性信任根。

**Download has no pretrusted verification** — `scripts/fetch-benchmark-assets.mjs:59-64`

只檢查取得成功，沒有比對known-good digest或signature。

```JavaScript
const res = spawnSync("curl", ["-sL", "--max-time", "180", "-o", dest, url]);
if (res.status !== 0 || !existsSync(dest)) throw new Error(...);
```

**Manifest is generated from downloaded bytes** — `scripts/fetch-benchmark-assets.mjs:98-108`

剛下載的bytes自行決定後續trust manifest。

```JavaScript
`${sha256(path.join(DIR, "PresentMon-2.5.1-x64.exe"))}  PresentMon-2.5.1-x64.exe`,
`${sha256(path.join(DIR, "lava-triangle.exe"))}  lava-triangle.exe`,
writeFileSync(path.join(DIR, "SHA256SUMS"), manifest);
```

#### Validation

確認URL使用HTTPS且固定version、assets已vendor於Git，並確認release build不自動呼叫fetch；因此保留finding但降為low。

Validation method: Manual static source trace against the current repository state

- **Status:** validated
- **Disposition:** reportable

Evidence:
- scripts/fetch-benchmark-assets.mjs:48-108
- package.json:13-16

Counterevidence and remaining uncertainty:
- URLs是HTTPS且含固定release version。
- assets已vendor in Git，正常tauri release build只verify而不fetch。
- 需要maintainer顯式refresh與後續review/commit/release。

#### Dataflow

attacker替換上游versioned asset；maintainer執行refresh；script接受bytes、copy入resources並生成matching SHA256SUMS；review/build將其發布；PaceDock elevated benchmark spawn執行。

**Download has no pretrusted verification** — `scripts/fetch-benchmark-assets.mjs:59-64`

只檢查取得成功，沒有比對known-good digest或signature。

```JavaScript
const res = spawnSync("curl", ["-sL", "--max-time", "180", "-o", dest, url]);
if (res.status !== 0 || !existsSync(dest)) throw new Error(...);
```

**Manifest is generated from downloaded bytes** — `scripts/fetch-benchmark-assets.mjs:98-108`

剛下載的bytes自行決定後續trust manifest。

```JavaScript
`${sha256(path.join(DIR, "PresentMon-2.5.1-x64.exe"))}  PresentMon-2.5.1-x64.exe`,
`${sha256(path.join(DIR, "lava-triangle.exe"))}  lava-triangle.exe`,
writeFileSync(path.join(DIR, "SHA256SUMS"), manifest);
```

#### Reachability

入口是顯式npm script，不在一般end-user runtime或每次release自動到達。

Preconditions:
- upstream pinned release asset可被attacker修改。
- maintainer執行fetch:benchmark-assets並發布結果。

Existing controls:
- HTTPS與固定tag URL。
- source review/Git diff可能發現binary及manifest同時改變，但沒有自動independent verification。

#### Severity

**Low** — 成功影響為由administrator app執行的供應鏈code，但需要上游release控制且maintainer顯式執行refresh並提交/發布，為high impact、low likelihood。

upstream pinned release asset可被attacker修改。；maintainer執行fetch:benchmark-assets並發布結果。

Impact assessment:
- **Level:** high
- **Rationale:** 惡意third-party PE可進入所有release並以administrator token執行。

Likelihood assessment:
- **Level:** low
- **Rationale:** 需上游compromise、maintainer refresh及後續發布多個前提。

#### Remediation

在repository或獨立受審核metadata中固定known-good SHA256或publisher signer identity；下載後先驗證再copy/modify。digest更新必須獨立review，不可由下載script自動把剛取得bytes升格為trust root；可加Sigstore/Authenticode verification與provenance attestation。

Tests:
- 用被替換但matching新digest的fixture模擬upstream，確認refresh在known-good verification階段拒絕。
- 驗證更新known-good digest需要獨立review/approval。

Preventive controls:
- Pretrusted digest or signer verification before vendoring。
- Reproducible/provenance-attested third-party binaries。

<a id="finding-46"></a>

### [46] portable update 缺少獨立 publisher signature

| Field | Value |
| --- | --- |
| Severity | low |
| Confidence | high |
| Confidence rationale | Validated by an independent worker and parent static source trace. |
| Category | insufficient-update-authenticity |
| CWE | CWE-494 |
| Affected lines | src-tauri/src/update.rs:248-294, src-tauri/src/update.rs:299-364, .github/workflows/release.yml:202-235 |

#### Summary

portable ZIP與checksum由同一release authority控制；publisher-side compromise可提供matching pair並散布administrator update。

#### Root Cause

portable ZIP與checksum由同一release authority控制；publisher-side compromise可提供matching pair並散布administrator update。

#### Validation

Validated against current repository source with counterevidence and prerequisites retained.

Validation method: Manual static source trace

- **Status:** validated
- **Disposition:** reportable

#### Dataflow

portable ZIP與checksum由同一release authority控制；publisher-side compromise可提供matching pair並散布administrator update。

#### Reachability

Source-backed conditional path described in summary.

#### Severity

**Low** — Severity was calibrated from the validated attacker, prerequisites, impact and likelihood.

Additional runtime or deployment evidence could raise or lower this severity.

Impact assessment:
- **Level:** high
- **Rationale:** portable ZIP與checksum由同一release authority控制；publisher-side compromise可提供matching pair並散布administrator update。

Likelihood assessment:
- **Level:** low
- **Rationale:** Calibrated from explicit prerequisites.

#### Remediation

以embedded public key驗證portable artifact及version metadata signature。

<a id="finding-47"></a>

### [47] mutable Action ref接觸signing key

| Field | Value |
| --- | --- |
| Severity | low |
| Confidence | high |
| Confidence rationale | Independently reviewed and parent-validated against current source. |
| Category | ci-supply-chain |
| CWE | CWE-829 |
| Affected lines | .github/workflows/release.yml:122-127 |

#### Summary

tauri-action@v1直接接收release token與updater signing credentials。

#### Root Cause

tauri-action@v1直接接收release token與updater signing credentials。

#### Validation

Validated against current source.

Validation method: Manual static source trace

- **Status:** validated
- **Disposition:** reportable

Evidence:
- .github/workflows/release.yml:122

#### Dataflow

tauri-action@v1直接接收release token與updater signing credentials。

#### Reachability

tauri-action@v1直接接收release token與updater signing credentials。

#### Severity

**Low** — Severity calibrated from established impact and likelihood.

Additional runtime or deployment evidence could raise or lower this severity.

Impact assessment:
- **Level:** low
- **Rationale:** tauri-action@v1直接接收release token與updater signing credentials。

Likelihood assessment:
- **Level:** low
- **Rationale:** Conditional on the stated attacker prerequisites.

#### Remediation

pin完整SHA並隔離approval-gated signing/upload。

<a id="finding-48"></a>

### [48] PE 診斷工具未中和名稱中的 terminal control sequences

| Field | Value |
| --- | --- |
| Severity | low |
| Confidence | high |
| Confidence rationale | Independently reviewed and parent-validated against current source. |
| Category | terminal-output-injection |
| CWE | CWE-150 |
| Affected lines | scripts/pe-exports.mjs:30-46, scripts/pe-imports.mjs:36-61 |

#### Summary

export/import DLL與function names由untrusted PE bytes直接ASCII decode後console.log；ESC/CSI/OSC/CR/backspace等可被terminal解讀，造成輸出與title spoofing。

#### Root Cause

export/import DLL與function names由untrusted PE bytes直接ASCII decode後console.log；ESC/CSI/OSC/CR/backspace等可被terminal解讀，造成輸出與title spoofing。

#### Validation

Validated against current source.

Validation method: Manual static source trace

- **Status:** validated
- **Disposition:** reportable

Evidence:
- scripts/pe-exports.mjs:30
- scripts/pe-imports.mjs:36

Counterevidence and remaining uncertainty:
- decoded names沒有傳給shell或其他敏感API。
- 實際效果依terminal emulator設定。

#### Dataflow

export/import DLL與function names由untrusted PE bytes直接ASCII decode後console.log；ESC/CSI/OSC/CR/backspace等可被terminal解讀，造成輸出與title spoofing。

#### Reachability

export/import DLL與function names由untrusted PE bytes直接ASCII decode後console.log；ESC/CSI/OSC/CR/backspace等可被terminal解讀，造成輸出與title spoofing。

Preconditions:
- developer手動檢查attacker-crafted PE且使用解讀control sequences的terminal。

#### Severity

**Low** — 影響限於手動developer terminal的diagnostic integrity；未建立shell、filesystem、IPC或command execution。

Additional runtime or deployment evidence could raise or lower this severity.

Impact assessment:
- **Level:** low
- **Rationale:** export/import DLL與function names由untrusted PE bytes直接ASCII decode後console.log；ESC/CSI/OSC/CR/backspace等可被terminal解讀，造成輸出與title spoofing。

Likelihood assessment:
- **Level:** low
- **Rationale:** Conditional on the stated attacker prerequisites.

#### Remediation

將untrusted names輸出為JSON或visible escaped representation；把C0/C1/DEL/ESC/newline/CR及terminal-sensitive Unicode轉為\\xNN/\\uNNNN。

<a id="finding-49"></a>

### [49] portable update 缺少獨立 publisher signature

| Field | Value |
| --- | --- |
| Severity | low |
| Confidence | high |
| Confidence rationale | source明確顯示兩個asset由同一release metadata選取、唯一信任判斷是SHA256，且release workflow用相同token上傳兩者。 |
| Category | insufficient-update-authenticity |
| CWE | CWE-494 |
| Affected lines | src-tauri/src/update.rs:248-294, src-tauri/src/update.rs:299-364, src-tauri/src/commands.rs:276-329, .github/workflows/release.yml:202-235, src-tauri/tauri.conf.json:47-54 |

#### Summary

portable updater從同一GitHub release取得ZIP與`.sha256`，一致即安裝；能修改release assets的actor可同時上傳惡意ZIP與配對checksum。installed updater已配置public key，但custom portable path沒有使用。

#### Root Cause

checksum與payload由相同發布authority控制，沒有獨立publisher identity trust root。

**Checksum comes from the same release** — `src-tauri/src/update.rs:358-360`

配對checksum不是不可由release publisher修改的signature。

```Rust
let expected_hex = fetch_and_parse_checksum(&release.checksum_asset)?;
verify_checksum(&buf, &expected_hex)?;
```

**One token uploads artifact and checksum** — `.github/workflows/release.yml:202-235`

發布端以相同authority產生並上傳兩者。

```YAML
$sha256 = (Get-FileHash -Path $zipName -Algorithm SHA256).Hash.ToLower()
gh release upload $tag $zipName "$zipName.sha256" --clobber
```

#### Validation

確認installed與portable更新路徑分離；只有installed plugin path使用配置pubkey，portable code只驗證same-release checksum。

Validation method: Manual static source trace against the current repository state

- **Status:** validated
- **Disposition:** reportable

Evidence:
- src-tauri/src/update.rs:248-364
- .github/workflows/release.yml:202-235
- src-tauri/tauri.conf.json:47-54

Counterevidence and remaining uncertainty:
- TLS、正式release篩選、semver、精確asset name、大小、ZIP magic/allowlist與SHA-256可防傳輸損壞或選錯資產。
- 它們不能抵抗可同時修改payload與checksum的publisher-side actor。

#### Dataflow

attacker取得release upload authority，發布較高版本惡意ZIP與配對checksum；client從同一release下載兩者；verify_checksum成功；extract/replacement安裝並執行。

**Checksum comes from the same release** — `src-tauri/src/update.rs:358-360`

配對checksum不是不可由release publisher修改的signature。

```Rust
let expected_hex = fetch_and_parse_checksum(&release.checksum_asset)?;
verify_checksum(&buf, &expected_hex)?;
```

**One token uploads artifact and checksum** — `.github/workflows/release.yml:202-235`

發布端以相同authority產生並上傳兩者。

```YAML
$sha256 = (Get-FileHash -Path $zipName -Algorithm SHA256).Hash.ToLower()
gh release upload $tag $zipName "$zipName.sha256" --clobber
```

#### Reachability

portable使用者檢查/安裝更新時可達。需要publisher-side compromise，不把一般network attacker視為可突破TLS。

Preconditions:
- attacker可修改目標GitHub release ZIP與checksum。
- portable使用者接受更新。

Existing controls:
- HTTPS與多項結構/大小檢查。
- installed updater的public-key signature不套用此custom path。

#### Severity

**Low** — 影響是所有portable使用者的administrator code execution，但攻擊前提是GitHub release publication authority或等價憑證已遭控制，依規則屬high impact、low likelihood。

Additional runtime or deployment evidence could raise or lower this severity.

Impact assessment:
- **Level:** high
- **Rationale:** 可向全部portable使用者散布administrator code。

Likelihood assessment:
- **Level:** low
- **Rationale:** 需先取得release publication authority或等價高價值憑證。

#### Remediation

以既有Tauri signing key或專用offline key對portable ZIP與version metadata簽章，將verification public key編入application；簽署內容綁定version、asset name、digest及release channel，缺簽章或rollback一律拒絕。

Tests:
- 建立測試release metadata含有效checksum但無signature，確認client拒絕。
- 修改version/name/digest任一欄位，確認signature binding失敗。

Preventive controls:
- Independent publisher signature for every update channel。
- Rollback/version binding。

<a id="finding-50"></a>

### [50] mutable Action ref接觸signing key

| Field | Value |
| --- | --- |
| Severity | low |
| Confidence | high |
| Confidence rationale | Independent worker plus parent source trace. |
| Category | supply-chain |
| CWE | CWE-829 |
| Affected lines | .github/workflows/release.yml:122 |

#### Summary

tauri-action@v1直接接收release token與updater signing credentials。

#### Validation

Validated against current source.

Validation method: Manual static source trace

- **Status:** validated
- **Disposition:** reportable

#### Dataflow

tauri-action@v1直接接收release token與updater signing credentials。

#### Reachability

tauri-action@v1直接接收release token與updater signing credentials。

#### Severity

**Low** — Previously validated and calibrated.

Additional runtime or deployment evidence could raise or lower this severity.

#### Remediation

pin完整SHA並隔離approval-gated signing/upload。

<a id="finding-51"></a>

### [51] 第三方 benchmark executable 下載後才從自身產生信任 digest

| Field | Value |
| --- | --- |
| Severity | low |
| Confidence | high |
| Confidence rationale | download、copy與digest生成順序由script完整建立；正常release不自動執行refresh是重要counterevidence。 |
| Category | build-supply-chain |
| CWE | CWE-494 |
| Affected lines | scripts/fetch-benchmark-assets.mjs:59-64, scripts/fetch-benchmark-assets.mjs:67-108, package.json:13-16, src-tauri/src/benchmark/assets.rs:71-96 |

#### Summary

`fetch:benchmark-assets` 從versioned URLs下載PresentMon與AutoGpuAffinity，僅檢查curl成功/檔案存在；接著對未驗證bytes計算SHA256並覆寫manifest。上游asset被替換時，後續build/runtime檢查只證明惡意binary與自生manifest一致。

#### Root Cause

vendor refresh缺少下載前的獨立完整性信任根。

**Download has no pretrusted verification** — `scripts/fetch-benchmark-assets.mjs:59-64`

只檢查取得成功，沒有比對known-good digest或signature。

```JavaScript
const res = spawnSync("curl", ["-sL", "--max-time", "180", "-o", dest, url]);
if (res.status !== 0 || !existsSync(dest)) throw new Error(...);
```

**Manifest is generated from downloaded bytes** — `scripts/fetch-benchmark-assets.mjs:98-108`

剛下載的bytes自行決定後續trust manifest。

```JavaScript
`${sha256(path.join(DIR, "PresentMon-2.5.1-x64.exe"))}  PresentMon-2.5.1-x64.exe`,
`${sha256(path.join(DIR, "lava-triangle.exe"))}  lava-triangle.exe`,
writeFileSync(path.join(DIR, "SHA256SUMS"), manifest);
```

#### Validation

確認URL使用HTTPS且固定version、assets已vendor於Git，並確認release build不自動呼叫fetch；因此保留finding但降為low。

Validation method: Manual static source trace against the current repository state

- **Status:** validated
- **Disposition:** reportable

Evidence:
- scripts/fetch-benchmark-assets.mjs:48-108
- package.json:13-16

Counterevidence and remaining uncertainty:
- URLs是HTTPS且含固定release version。
- assets已vendor in Git，正常tauri release build只verify而不fetch。
- 需要maintainer顯式refresh與後續review/commit/release。

#### Dataflow

attacker替換上游versioned asset；maintainer執行refresh；script接受bytes、copy入resources並生成matching SHA256SUMS；review/build將其發布；PaceDock elevated benchmark spawn執行。

**Download has no pretrusted verification** — `scripts/fetch-benchmark-assets.mjs:59-64`

只檢查取得成功，沒有比對known-good digest或signature。

```JavaScript
const res = spawnSync("curl", ["-sL", "--max-time", "180", "-o", dest, url]);
if (res.status !== 0 || !existsSync(dest)) throw new Error(...);
```

**Manifest is generated from downloaded bytes** — `scripts/fetch-benchmark-assets.mjs:98-108`

剛下載的bytes自行決定後續trust manifest。

```JavaScript
`${sha256(path.join(DIR, "PresentMon-2.5.1-x64.exe"))}  PresentMon-2.5.1-x64.exe`,
`${sha256(path.join(DIR, "lava-triangle.exe"))}  lava-triangle.exe`,
writeFileSync(path.join(DIR, "SHA256SUMS"), manifest);
```

#### Reachability

入口是顯式npm script，不在一般end-user runtime或每次release自動到達。

Preconditions:
- upstream pinned release asset可被attacker修改。
- maintainer執行fetch:benchmark-assets並發布結果。

Existing controls:
- HTTPS與固定tag URL。
- source review/Git diff可能發現binary及manifest同時改變，但沒有自動independent verification。

#### Severity

**Low** — 成功影響為由administrator app執行的供應鏈code，但需要上游release控制且maintainer顯式執行refresh並提交/發布，為high impact、low likelihood。

Additional runtime or deployment evidence could raise or lower this severity.

Impact assessment:
- **Level:** high
- **Rationale:** 惡意third-party PE可進入所有release並以administrator token執行。

Likelihood assessment:
- **Level:** low
- **Rationale:** 需上游compromise、maintainer refresh及後續發布多個前提。

#### Remediation

在repository或獨立受審核metadata中固定known-good SHA256或publisher signer identity；下載後先驗證再copy/modify。digest更新必須獨立review，不可由下載script自動把剛取得bytes升格為trust root；可加Sigstore/Authenticode verification與provenance attestation。

Tests:
- 用被替換但matching新digest的fixture模擬upstream，確認refresh在known-good verification階段拒絕。
- 驗證更新known-good digest需要獨立review/approval。

Preventive controls:
- Pretrusted digest or signer verification before vendoring。
- Reproducible/provenance-attested third-party binaries。

<a id="finding-52"></a>

### [52] portable update缺獨立publisher signature

| Field | Value |
| --- | --- |
| Severity | low |
| Confidence | high |
| Confidence rationale | Independent worker plus parent source trace. |
| Category | update-integrity |
| CWE | CWE-494 |
| Affected lines | src-tauri/src/update.rs:248 |

#### Summary

same-release ZIP/checksum可由publisher-side actor一起替換。

#### Validation

Validated against current source.

Validation method: Manual static source trace

- **Status:** validated
- **Disposition:** reportable

#### Dataflow

same-release ZIP/checksum可由publisher-side actor一起替換。

#### Reachability

same-release ZIP/checksum可由publisher-side actor一起替換。

#### Severity

**Low** — Previously validated and calibrated.

Additional runtime or deployment evidence could raise or lower this severity.

#### Remediation

以embedded public key驗證portable artifact與metadata signature。

## Reviewed Surfaces

| Surface | Risk Area | Outcome | Notes |
| --- | --- | --- | --- |
| 最高權限排程工作指向可由未提升程序置換的 executable | privilege-escalation | Reported | Validated source-to-sink path; represented by privilege-escalation.writable-scheduled-task-target. |
| 未限定系統工具路徑可在提升後啟動攻擊者 binary | untrusted-search-path | Reported | Validated source-to-sink path; represented by privilege-escalation.untrusted-system-tool-search. |
| 固定使用者 TEMP staging 允許在驗證後置換更新腳本與 executable | insecure-temporary-file | Reported | Validated source-to-sink path; represented by privilege-escalation.portable-update-staging-race. |
| 可偽造的 APPDATA state 驅動 elevated GPU policy mutation | external-control-of-system-setting | Reported | Validated source-to-sink path; represented by privileged-state.unauthenticated-gpu-recovery. |
| production BenchmarkConfig 可選取未驗證 executable 供 elevated runner 執行 | externally-controlled-executable | Reported | Validated source-to-sink path; represented by privilege-escalation.benchmark-executable-overrides. |
| 可修改的同目錄 manifest 與未驗證 D3D9 無法保護 elevated benchmark sidecars | missing-executable-integrity | Reported | Validated source-to-sink path; represented by privilege-escalation.mutable-benchmark-sidecars. |
| portable update 缺少獨立 publisher signature | insufficient-update-authenticity | Reported | Validated source-to-sink path; represented by update-integrity.portable-artifact-unsigned. |
| 可變 GitHub Action ref 直接接觸 updater signing key 與 release token | ci-supply-chain | Reported | Validated source-to-sink path; represented by supply-chain.mutable-release-action. |
| 第三方 benchmark executable 下載後才從自身產生信任 digest | build-supply-chain | Reported | Validated source-to-sink path; represented by supply-chain.unverified-benchmark-refresh. |
| benchmark 接受或重讀未綁定本次 capture identity 的 CSV | benchmark-evidence-integrity | Reported | Validated source-to-sink path; represented by benchmark-integrity.untrusted-capture-files. |
| PE 診斷 parser 可因未終止名稱進入無限迴圈 | parser-denial-of-service | Reported | Validated source-to-sink path; represented by parser-dos.pe-unterminated-cstring. |
| PE 診斷工具未中和名稱中的 terminal control sequences | terminal-output-injection | Reported | Validated source-to-sink path; represented by output-injection.pe-terminal-control-sequences. |
| Frontend rendering, origins and privileged IPC reachability | xss-and-renderer-boundary | No issue found | No {@html}, innerHTML, eval, remote iframe/navigation/fetch/WebSocket or attacker-controlled renderer entry was found; backend-derived strings use Svelte text/attribute interpolation. CSP null remains defense-in-depth hardening, not a demonstrated exploit path. |
| Database, server, upload and XML surfaces | server-side-injection | Not applicable | Repository contains no database, HTTP server, upload endpoint or XML parser; SQL/NoSQL injection, request smuggling, SSRF upload paths and XXE are not applicable to the product source. |
| Archive extraction and session path containment | path-traversal | No issue found | Session directories require a UUID. Portable ZIP extraction rejects `..`, backslashes, nested/unexpected resources, duplicates and missing required entries. The reported update issue is post-extraction local object integrity, not archive-member traversal. |
| Process rule targeting and PID lifecycle controls | process-authorization-controls | No issue found | FullPath and explicit FileName modes, fixed/System32 blacklist, PID creation-time tracking and affinity readback were reviewed. Two unproven race/path-unknown questions remain documented separately. |
| GPU interactive mutation transaction controls | privileged-gpu-controls | No issue found | Normal apply validates LP, present adapter and BasicDisplay; CAS reservation, pre-write snapshot, readback, rollback and recoveryRequired controls prevent concurrent or silently partial mutations. Reported findings concern unauthenticated persisted state and benchmark evidence. |
| Unsafe Win32 memory boundaries | memory-safety | No issue found | Reviewed production unsafe blocks for buffer sizing, handles, callbacks and SetupAPI/registry structs; no attacker-reachable out-of-bounds, UAF or type confusion was established. |
| Secrets and credential material | secret-exposure | No issue found | No private keys, API tokens or passwords were found. Updater pubkey and Windows publicKeyToken are public verification/identity values. |
| Dependency lock integrity | dependency-integrity | No issue found | npm lockfile v3 entries have resolved URLs and integrity fields; Cargo locks contain registry checksums and no git/path sources. Current advisory/CVE matching was not performed offline. |
| Bundled benchmark binary metadata | binary-provenance | No issue found | Tracked PresentMon/lava hashes match SHA256SUMS; PresentMon Authenticode is valid (Intel), lava is unsigned. Opaque binary internals were not reverse engineered; trust-root/runtime weaknesses are reported separately. |
| 最高權限排程工作指向可置換 executable | privilege-escalation | Reported | Validated privilege-escalation.writable-scheduled-task-target. |
| 未限定系統工具路徑可啟動攻擊者 binary | untrusted-search-path | Reported | Validated privilege-escalation.untrusted-system-tool-search. |
| 固定 TEMP staging 允許置換 update script 與 executable | insecure-temporary-file | Reported | Validated privilege-escalation.portable-update-staging-race. |
| 可偽造的 APPDATA state 驅動 elevated GPU mutation | external-control-of-system-setting | Reported | Validated privileged-state.unauthenticated-gpu-recovery. |
| production BenchmarkConfig 可選未驗證 executable | externally-controlled-executable | Reported | Validated privilege-escalation.benchmark-executable-overrides. |
| 可寫 manifest 與 D3D9 exists-only 無法保護 elevated sidecars | missing-executable-integrity | Reported | Validated privilege-escalation.mutable-benchmark-sidecars. |
| benchmark 接受或重讀未綁定本次 capture identity 的 CSV | benchmark-evidence-integrity | Reported | Validated benchmark-integrity.untrusted-capture-files. |
| portable update 缺少獨立 publisher signature | insufficient-update-authenticity | Reported | Validated update-integrity.portable-artifact-unsigned. |
| mutable Action ref 接觸 signing key 與 release token | ci-supply-chain | Reported | Validated supply-chain.mutable-release-action. |
| 第三方 binary 下載後才自生 digest | build-supply-chain | Reported | Validated supply-chain.unverified-benchmark-refresh. |
| PE 診斷 parser 可因未終止名稱進入無限迴圈 | parser-denial-of-service | Reported | Validated parser-dos.pe-unterminated-cstring. |
| PE 診斷工具未中和 terminal control sequences | terminal-output-injection | Reported | Validated output-injection.pe-terminal-control-sequences. |

## Open Questions And Follow Up

- Can a local process reliably win the first-open PID reuse window between watcher rule/blacklist checks and OpenProcess, producing a meaningful protected-process impact?
  - Follow-up prompt: Use a Windows stress harness that binds authorization and image/creation-time reads to one handle, then attempt controlled PID reuse against a benign protected fixture.
- Can any concrete protected process have QueryFullProcessImageName fail while PROCESS_SET_INFORMATION succeeds, allowing FileName matching to bypass the intended System32 exclusion?
  - Follow-up prompt: Instrument a representative Windows 11 deployment and compare path-query and set-information access outcomes across protected process classes.
