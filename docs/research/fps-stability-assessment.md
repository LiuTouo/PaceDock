# PaceDock 核心方法與 FPS 穩定度完整評估

評估日期：2026-09-09。程式版本：0.2.7，commit `32f0bbed13feb070293e1b10b2500b49dfe05c1c`。評估包含現行原始碼、443 項 Rust 測試、一筆本機既有 benchmark 的原始 CSV，以及 [Microsoft、Intel、PresentMon 與 NIST 一手資料核對](fps-stability-primary-sources.md)。沒有重新啟動 GPU benchmark、修改 GPU 策略或遊戲程序設定。

**結論：核心方法有條件成立，但目前證據不足以認定 PaceDock 能普遍、可靠地改善實際遊戲 FPS 穩定度。** 它已具備排程規則執行與合成實驗工具的功能；目前最強證據是「在特定機器、特定高速 Vulkan 合成負載中，LP 2 相對 LP 14 有較好的部分 Present 間隔統計」。這尚未跨越到「優於原始 Windows 策略」，更沒有跨越到「推薦核心配置改善實際遊戲」。

**一、先定義要改善的結果**

平均 FPS、應用程式 Present 間隔、實際顯示間隔與輸入延遲是不同結果。FPS 穩定度應主要檢查慢幀尾端及長停頓，而非只看平均 FPS 或合成分數。240 FPS 的 frame budget 為 4.167 ms、144 FPS 為 6.944 ms、60 FPS 為 16.667 ms。應在固定畫質、解析度、限幀與同步條件下比較 p99/p99.9、超時幀數、超時時間占比及個別長停頓。

本專案只用 `MsBetweenPresents` 建立 benchmark 的效能統計；它沒有用該統計證明 displayed frame pacing 或輸入延遲改善。[程式：metrics.rs](../../src-tauri/src/benchmark/metrics.rs)、[PresentMon v1 欄位定義](https://github.com/GameTechDev/PresentMon/blob/v1.9.2/README.md#csv-columns)。

**二、目前真正執行的核心方法**

| 方法 | 現行實作事實 | 可能有效的條件與限制 |
| --- | --- | --- |
| 程序 CPU affinity | `Custom`、`NoSmtSibling`、`PCoresOnly` 先硬 affinity，再 CPU Sets，最後 thread ideal；硬 affinity／CPU Sets 有回讀。 | 若瓶頸是排程競爭或不利的執行位置，可能有效；限制所有執行緒也會犧牲平行度。 |
| `Prefer` | 只對當下列舉到的執行緒，以 round-robin 指派 ideal processor。 | 是軟提示，沒有辨識主／render／worker 執行緒；成功不等於實際隔離。 |
| `All` | 設回 group 0 全核心硬 mask，並清除 process-default CPU Sets。 | 是程式定義的還原，不能視為精確恢復所有原始 thread ideal／thread-selected 設定。 |
| CPU priority | 整個程序的 priority class；新規則預設 `High`。 | 可影響 CPU runnable 競爭；不能解決 GPU 算力、shader 編譯、I/O 或記憶體容量本身的限制。 |
| I/O／memory priority | 選用的程序層级設定；memory priority 最高 Normal。 | 不是硬體加速；需要對應資源競爭才可能有用，降低遊戲記憶體優先級可能造成反效果。 |
| GPU interrupt affinity | 每次寫 `DevicePolicy=4` 與單 bit mask，重啟 GPU，執行內建 workload。 | 改變裝置中斷的允許服務位置；不是分配 GPU shader 到 CPU，也不能單靠 registry 驗證所有 DPC 執行位置。 |
| 遊戲核心推薦 | 排除最佳 GPU LP 所屬整顆實體核心；若排除後還有至少六顆核心，額外排除 core 0。 | 避開 GPU 中斷干擾是合理假說，但這個完整配置沒有在 benchmark 中測量。 |

程式依據：[watcher.rs](../../src-tauri/src/watcher.rs)、[process.rs](../../src-tauri/src/process.rs)、[model.rs](../../src-tauri/src/model.rs)、[priority.rs](../../src-tauri/src/priority.rs)、[recommend.rs](../../src-tauri/src/benchmark/recommend.rs)。

Microsoft 指出硬 affinity 可能妨礙有效排程；Intel 的遊戲執行緒指引也要求針對工作性質與硬體驗證。不能由「API 可成功套用」推得「一定更快」。[Microsoft：Multiple Processors](https://learn.microsoft.com/en-us/windows/win32/procthread/multiple-processors)、[Intel：Optimizing Threading for Gaming Performance](https://www.intel.com/content/www/us/en/developer/articles/technical/optimizing-threading-for-gaming-performance.html)。

特別是 `NoSmtSibling` 只限制目標程序使用每顆實體核心的一條 LP，並沒有關閉 SMT，也沒有禁止其他程序在 sibling 上執行。把遊戲移走亦不等於把留下的核心專供 GPU 中斷。現有拓撲只有核心、LP、SMT、efficiency class，沒有 L3／CCD／NUMA 分組或遊戲 critical-path 執行緒辨識；不能宣稱已有針對跨 CCD 或 V-Cache 的自動最佳化。

**三、本機已存在的實測證據**

讀取本機唯一既有 session：`%APPDATA%\PaceDock\benchmarks\e0ded88f-01eb-4893-a7fb-e91ff8726987\session.json`。

- 日期：2026-09-03；GPU：RTX 5080；Vulkan，1280×720 視窗模式。
- Adaptive 校準到 4,000 FPS；候選僅 LP 2、4、6、8、10、12、14。
- 記錄為 Completed／Passed；26 次 capture 有效，記錄的 ETW event loss 為 0。
- 決選 LP 2 對 LP 14，獨立確認 round 100、101、102，每個 LP 每輪約 30 秒。
- 這是過去版本產生的歷史記錄；未把它當成當前 commit 重跑的結果，也未驗證該舊檔是否符合現行套用流程的認證要求。
- `session.json` SHA-256：`4D35C33CF99B21F5C3635B5858BD0237C15CABF3116E144F1995C592B363544E`。

原始 CSV 重新計算結果如下。p99／p99.9 使用排序後 `floor((n−1)q)`，Low 沿用專案最慢 N% instantaneous FPS 算術平均。

| 輪次 | LP | 有效幀數 | Avg FPS | 1% Low | 0.1% Low | p99 Present ms | p99.9 Present ms | 最長 Present ms |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 100 | 2 | 119330 | 3979.471 | 2402.755 | 1933.324 | 0.3738 | 0.4747 | 0.8500 |
| 100 | 14 | 119150 | 3974.036 | 2330.267 | 1847.506 | 0.3865 | 0.4902 | 1.2996 |
| 101 | 2 | 119348 | 3979.791 | 2398.869 | 1920.640 | 0.3749 | 0.4809 | 0.7276 |
| 101 | 14 | 119195 | 3973.863 | 2325.321 | 1855.834 | 0.3863 | 0.4935 | 1.1226 |
| 102 | 2 | 119332 | 3978.985 | 2387.430 | 1895.817 | 0.3746 | 0.4892 | 0.7033 |
| 102 | 14 | 119154 | 3973.070 | 2326.062 | 1857.959 | 0.3865 | 0.4884 | 0.7768 |

三輪指標各取中位數後，LP 2 的平均 FPS 高約 0.1411%、1% Low 高約 3.1300%、0.1% Low 高約 3.4920%。p99 Present 間隔為 0.3746 對 0.3865 ms，差 0.0119 ms，即 11.9 μs。這支持「同一次合成測試中的局部差異」，不能解讀為實際遊戲 FPS 提升 3%。第 102 輪 p99.9 反而略差，亦不能說所有尾端指標全面改善。

六份 CSV 的 `PresentMode` 全為 `Composed: Copy with GPU GDI`；`Dropped=1` 約占 87.92–87.94%。此欄代表未顯示的幀，不是 ETW 遺失。高速提交可自然造成許多幀不被顯示，不能據此說資料損壞；但這強烈限制了「提交改善等於畫面更順」的推論。[PresentMon Dropped 定義](https://github.com/GameTechDev/PresentMon/blob/v1.9.2/README.md#csv-columns)。

所有確認樣本都沒有超過 4.167 ms 的 Present 間隔。這不表示顯示端沒有慢幀，而是目前分析的提交序列沒有量到這類事件。因此資料不能回答實際遊戲 4–16 ms 以上慢幀、shader hitch 或長停頓是否被改善。

**四、目前缺少的主要證據**

1. **沒有正式的 Windows 原始策略對最佳候選比較。** `baseline` 是回復快照；校準有在變更前 capture，但使用不同 cap、時長與時序，沒有進入相同條件的配對效益判定。主流程 Passed 只表示候選贏過亞軍。即使所有單 LP 都比預設差，也可能選出其中最好的候選。Equivalent 流程確有 reference 驗證，但 reference 必須能解析成既有單 LP；它不能取代一般 OS 預設策略的對照。[runner.rs](../../src-tauri/src/benchmark/runner.rs)、[manager.rs](../../src-tauri/src/benchmark/manager.rs)。

2. **測量條件與最後推薦不同。** `capture_step` 明確不設定 workload CPU affinity；`recommended_cores` 卻把遊戲 CPU 集合縮小，UI 匯入後使用 `Custom`。以八顆實體核心、最佳 GPU core 非 core 0 為例，遊戲可能只剩六顆。這是可用核心數減少 25%，不是已證明 FPS 會減少 25%；實際淨效益取決於引擎需求。六核心機仍會排掉最佳那顆而剩五顆，六核心門檻只限制額外排除 core 0。隔離收益必須勝過容量損失才成立。[recommend.rs](../../src-tauri/src/benchmark/recommend.rs)、[RuleEditor.svelte](../../src/components/RuleEditor.svelte)。

3. **合成 workload 的可轉移性未知。** 現行 workload 只有 Vulkan 與 D3D9，沒有真實遊戲 replay 或 D3D12 工作負載。D3D9 是 Clear＋Present；數千 FPS 的提交壓力、視窗合成、driver 開銷比例與一般遊戲不同。沒有 CPU busy／GPU busy／ready wait／driver DPC trace，就不能確定排名背後是哪種瓶頸。[D3D9 workload](../../src-tauri/d3d9-workload/src/main.rs)、[PresentMon 呼叫參數](../../src-tauri/src/benchmark/runner.rs)。

4. **多輪設計仍是小樣本啟發式。** 現行流程為一次短篩選全部 LP → Top 5 → Top 3 → Top 2 的 3–7 輪確認；亞軍反轉才觸發獨立反向驗證。篩選與確認隔離、交錯順序、逐輪一致性與護欄都能減少部分誤判，但篩選可能提早淘汰真正最佳者，少數配對不能代表跨日重現。bootstrap 穩定性區間不是統計信賴區間，程式註解本身也明說不宣稱覆蓋率。複合分數的 0.5 門檻不是 FPS 提升 0.5%，其權重與 clamp 亦未由實際遊戲體感驗證。[runner.rs](../../src-tauri/src/benchmark/runner.rs)、[metrics.rs](../../src-tauri/src/benchmark/metrics.rs)。

5. **策略回讀不等於機制驗證。** GPU 成功驗證主要比較 registry 值；沒有測量 driver ISR/DPC 實際 CPU、耗時及與慢幀的重疊。Driver 可以另設 DPC 目標；把 interrupt affinity 改成一顆 LP 不證明所有 GPU DPC 都被隔離。[gpu.rs](../../src-tauri/src/gpu.rs)、[Microsoft DPC queues](https://learn.microsoft.com/en-us/windows-hardware/drivers/kernel/organization-of-dpc-queues)。

6. **環境與歷史有效性有限。** AC、節能、CPU idle、5% 時間漂移與 capture 品質閘已有實作，但不是溫度／時脈／GPU 負載穩定的直接證據。硬體相容性主要驗 CPU 指紋與 GPU instance；沒有充分涵蓋 driver、OS、遊戲版本、顯示模式或背景負載的效能有效期限。[env.rs](../../src-tauri/src/benchmark/env.rs)、[manager.rs](../../src-tauri/src/benchmark/manager.rs)。

**五、直接影響可信度的實作細節**

- **D3D9 限幀不精確，且高於 1,000 FPS 完全不 sleep。** `frame_ms = 1000u64 / fps_cap`：240 得 4 ms，500 得 2 ms，1000 得 1 ms，2000／4000 得 0。還會額外加上 render 時間，並非精確 frame deadline。這使校準 tiers 不代表實際指定 cap；上述本機紀錄用 Vulkan，不能把此缺陷當成該紀錄無效的理由。[main.rs](../../src-tauri/d3d9-workload/src/main.rs)。
- **D3D9 固定使用 adapter 0。** `CreateDevice(0, ...)` 沒有映射 UI 所選 GPU 的 PnP ID。多 GPU 系統可能測量 A 卡 workload、修改 B 卡中斷策略，需以實際 adapter LUID／裝置對應驗證。單 GPU 是否受影響要依實際枚舉狀況判斷，不能由此否定全部結果。[main.rs](../../src-tauri/d3d9-workload/src/main.rs)。
- **Prefer 不持續追蹤新執行緒。** 程序初次發現即列舉當下 threads；成功之後 revalidation 只包含 Hard／CpuSets。啟動早期套用成功，不保證稍後產生的主力 worker 都取得相同偏好；這點對刻意提早開 handle 的設計尤其重要。[process.rs](../../src-tauri/src/process.rs)、[watcher.rs](../../src-tauri/src/watcher.rs)。
- **預設 High 應視為待驗證政策。** 整個遊戲升級可能讓與遊戲合作的音訊、擷取或其他支援程序取得較少 CPU 時間，尤其系統繁忙時。不能把新建規則當成「只監控」的對照。[model.rs](../../src-tauri/src/model.rs)、[Rules.svelte](../../src/pages/Rules.svelte)、[Microsoft SetPriorityClass](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-setpriorityclass)。
- **Low 定義需要保持透明。** 最慢 FPS 的算術平均不等於最慢 frametime 平均的倒數。10、100 ms 的兩個慢幀分別得到 55 與 18.18 FPS；不是單純 rounding 差。極低百分位也可能只剩一幀。應優先直接報告 ms 尾端分布，不與不同 Low 定義的外部工具直接比較。[metrics.rs](../../src-tauri/src/benchmark/metrics.rs)。
- **自身開銷尚未量測。** 100 ms discovery 與約每秒完整程序維護、Dashboard 取樣皆有成本。不能僅以輪詢頻率斷言有害，也不能斷言零開銷；需要把工具關閉、只監控與套用策略分開測試。[watcher.rs](../../src-tauri/src/watcher.rs)、[usage.rs](../../src-tauri/src/usage.rs)。
- **README 落後程式。** `Prefer` 現在是純 ideal、`All` 現在會還原設定；GPU 排程已有 refinement 與有條件 Equivalent 套用。評估不能沿用 README 的舊行為描述。

**六、哪些情境值得測，哪些不能期待它直接修好**

| 情境 | 判斷 |
| --- | --- |
| CPU 受限、高 FPS，且 trace 顯示主執行緒 ready wait 或局部 ISR/DPC 干擾 | 最符合方法的作用機制，值得做逐遊戲配對實驗。 |
| 背景工作與遊戲爭用 CPU | 排程優先序或互補核心配置可能有效；只限制遊戲並不能保證背景工作被移走。 |
| Hybrid／跨 CCD 且已證實執行位置不利 | 手動集合可能有用；現行自動推薦沒有足夠拓撲與工作語意支持通用最佳值。 |
| 工作執行緒多、實體核心少 | 排除 SMT 或整顆核心可能增加佇列，惡化尾端；必須保留全核心對照。 |
| 主要受限於 GPU render、VRAM／RAM、儲存、shader compilation、thermal throttling | 本專案沒有直接修復這些根因；不能預設 affinity 會解決。 |
| 引擎同步／present pacing 問題 | CPU 排程只是可能因素之一；工具沒有針對真實遊戲控制 frame pacing 的回饋迴路。 |

這些是機制推論，不是已完成的分硬體效能測試結果。

**七、優先做的驗證與改進**

第一優先是新增「原始策略是合法候選，而且沒有改善就不推薦」的真實遊戲驗證流程，重要性高於繼續增加候選排名演算法的複雜度。

| 實驗條件 | GPU interrupt policy | 遊戲 CPU affinity | Priority |
| --- | --- | --- | --- |
| A：原始基線 | 原始快照／OS 策略 | 原始設定 | 原始設定 |
| B：只調 GPU | 候選單 LP | 原始設定 | 原始設定 |
| C：只調遊戲 CPU | 原始策略 | 推薦集合 | 原始設定 |
| D：組合 | 候選單 LP | 推薦集合 | 原始設定 |
| E：優先序增量 | 由 A–D 中已驗證配置決定 | 同左 | 分開比較 Normal／AboveNormal／High |

先另外比較 A 下工具關閉與僅監控的差異。控制組不能只是關閉 PaceDock：程序設定可能留存到程序結束，GPU policy 也會持續；必須回復 GPU 原始快照並重新啟動遊戲，驗證實際狀態。

使用固定 replay／路線、先暖機，再做跨多次啟動與跨日的配對區塊；區塊內隨機順序或平衡 AB／BA。起始可規劃每條件 8–12 次、每次 60–120 秒，實際次數再依先導變異與預先訂定的最小效益調整；這是實驗規劃建議，不是統計保證。不能把同一 capture 的十萬幀當成十萬次獨立實驗。[NIST randomized blocks](https://www.itl.nist.gov/div898/handbook/pri/section3/pri332.htm)。

主要結果保留 p99／p99.9 presented 與 displayed ms、超過實際 frame budget 的比例、超過 2×budget 與 25／50 ms 的事件及時間占比、平均 FPS；可量測時加入輸入延遲。ETW 機制診斷另用對稱條件 capture，確認 GPU ISR/DPC 的 LP 與耗時、遊戲 critical threads 的 ready wait、CPU／GPU busy。記錄 driver、OS、遊戲、HAGS、VRR／VSync、cap、溫度／時脈與背景負載。

Passed 的產品意義應改為：「在這個遊戲與設定下，相對原始策略，主要尾端指標達到預先定義的實務改善，其他護欄無明顯退步，且獨立確認可重現」。只贏過另一顆核心，應標為合成測試排名。優先修正 D3D9 cap／adapter 對應，並把核心排除、priority、SMT 選擇各自納入實驗條件。

**八、驗證範圍與最終判斷**

執行 `cargo test --manifest-path src-tauri/Cargo.toml --quiet`：443 passed，0 failed。測試主要證明純邏輯、mock 流程與回復契約依預期運作；沒有量測實際遊戲改善。GitNexus 索引已更新，但本機 FTS extension 不可用，概念搜尋無結果後使用符號 context 與現行檔案交叉確認。沒有修改應用程式原始碼；只新增研究與評估文件。

因此，「核心方法一定沒用」不符合證據；「這個專案已證明能穩定改善遊戲 FPS」同樣超出證據。現階段可支持的定位是：**有可測試機制、具備部分品質控制的排程調校工具；實際遊戲效益及自動推薦的可靠性仍待原始策略對照與遊戲級驗證。**
