# FrameAnchor 核心方法與 FPS 穩定度：一手來源核對

查核日期：2026-09-09。範圍：Windows 排程 API、GPU interrupt affinity、PresentMon 2.5.1 指標，以及如何驗證實際遊戲效益。此文件是原理與程式碼的交叉評估，沒有執行硬體效能實驗，不能作為提升百分比的證據。

## 結論

這套方法具有可成立的作用機制：若慢幀主要來自 CPU 排程競爭、SMT 資源共享，或特定核心上的 ISR/DPC 干擾，調整執行位置可能有效。但 API 能成功設定，不等於遊戲效能改善；內建 workload 找到較佳 GPU interrupt LP，也不等於在真實遊戲套用 GPU affinity 加 CPU 核心排除後仍改善。目前應定位為「需逐遊戲驗證的調校工具」，不能推論為普遍有效的 FPS 穩定化方法。

上述效益是有條件的推論。Microsoft 與 Intel 的官方資料均保留 OS 排程彈性，沒有支持所有遊戲都應排除 CPU0、GPU interrupt 所在整顆核心，或固定使用 P-core only 的通用規則。[Microsoft：Multiple Processors](https://learn.microsoft.com/en-us/windows/win32/procthread/multiple-processors)、[Intel：Optimizing Threading for Gaming Performance](https://www.intel.com/content/www/us/en/developer/articles/technical/optimizing-threading-for-gaming-performance.html)

## API 實際保證與限制

| 方法 | 官方合約 | 對效益判斷的意義 |
| --- | --- | --- |
| `SetProcessAffinityMask` | 限制行程執行緒可執行的處理器集合；Microsoft 指出硬 affinity 通常應避免，因其可能妨礙有效排程、降低平行效能。 | 減少競爭與減少可用算力同時發生，需要實測淨效果。 |
| `SetThreadIdealProcessorEx` | 是偏好提示，沒有保證執行緒一定在該 LP 執行。 | 不能把成功回傳描述為獨占核心或完整隔離。 |
| `SetProcessDefaultCpuSets` | 只涵蓋沒有 thread-selected CPU Sets 的執行緒；hard affinity 優先於衝突的 CPU Sets。 | process-default 回讀正確，仍不證明每條遊戲執行緒實際只在該集合執行。 |
| `SetPriorityClass` | 改變 CPU 排程優先序；不能單靠 CPU priority 控制磁碟、記憶體等資源干擾。 | 對 runnable CPU 競爭才有直接作用，不能等同 GPU、儲存或記憶體效能優化。 |

來源：[Microsoft：Multiple Processors](https://learn.microsoft.com/en-us/windows/win32/procthread/multiple-processors)、[Microsoft：CPU Sets](https://learn.microsoft.com/en-us/windows/win32/procthread/cpu-sets)、[Microsoft：SetPriorityClass](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-setpriorityclass)。

Intel 的遊戲開發指引將 critical-path 與背景執行緒區分，建議少量關鍵工作才提高優先序，並要求在目標硬體驗證 affinity 假設。整個遊戲行程套一組固定核心，沒有遊戲引擎內部每條執行緒的工作語意，因此不能直接套用該指引所描述的細粒度最佳化成果。[Intel：Optimizing Threading for Gaming Performance](https://www.intel.com/content/www/us/en/developer/articles/technical/optimizing-threading-for-gaming-performance.html)

AMD 官方 chipset release notes 確認存在 `AMD 3D V-Cache Performance Optimizer Driver`。這只能證明平台已有相關軟體元件，不能證明 FrameAnchor 的手動 affinity 一定優於它。舊 AMD 社群安裝指引查核時已重新導向公告頁，未將搜尋摘要中的完整排程細節當成已驗證結論。[AMD：Chipset Driver 6.10.17.152 Release Notes](https://www.amd.com/en/resources/support-articles/release-notes/RN-RYZEN-CHIPSET-6-10-17-152.html)

## GPU interrupt affinity 的因果缺口

Microsoft 定義 interrupt affinity 為可以服務裝置中斷的處理器集合；`DevicePolicy` 配合 `AssignmentSetOverride` 是正式機制，但官方建議適用時維持預設策略。`DevicePolicy = 4` 的正式名稱是 `IrqPolicySpecifiedProcessors`，並非內建的「最佳單核心」模式；本專案透過單 bit mask 達成單 LP 選擇。[Microsoft：Interrupt affinity](https://learn.microsoft.com/en-us/windows-hardware/drivers/kernel/interrupt-affinity-and-priority)、[Microsoft：IRQ_DEVICE_POLICY](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/wdm/ne-wdm-_irq_device_policy)

事實：每顆處理器有自己的 DPC queue，driver 可以另外指定 DPC 的目標處理器。因此寫入 interrupt mask，不能單憑 registry 回讀推斷所有 GPU driver DPC 都已搬到該 LP。這是「設定狀態」與「實際執行路徑」的差別；應記錄 GPU driver ISR/DPC 的實際 LP、次數、時間，並與慢幀時間軸對齊。[Microsoft：Organization of DPC Queues](https://learn.microsoft.com/en-us/windows-hardware/drivers/kernel/organization-of-dpc-queues)、[Microsoft：Measuring DPC/ISR Time](https://learn.microsoft.com/en-us/windows-hardware/drivers/devtest/example-15--measuring-dpc-isr-time)

推論：若集中到一顆 LP 降低了對遊戲主執行緒的干擾，可能縮短慢幀；若因此形成中斷處理瓶頸、增加跨核心協作，或犧牲遊戲需要的 CPU 算力，也可能反向影響。這兩種方向都需實際 trace 與 A/B 數據區分，官方 API 文件沒有提供通用提升幅度。

## 專案量測與實際套用不是同一個處理條件

以下是直接讀取本機程式碼得到的事實：

- `src-tauri/src/benchmark/runner.rs` 的 `capture_step` 明確禁止把 workload process affinity 綁到受測 LP，目的是只改 GPU interrupt affinity。
- `src-tauri/src/benchmark/mod.rs` 的 workload 僅含內建 Vulkan 與 D3D9；預設為 `lava-triangle.exe`，預設取樣 30 秒。
- `src-tauri/src/benchmark/recommend.rs::recommended_cores` 永遠排除最佳 GPU LP 所屬的整顆實體核心，另在剩餘至少 6 顆實體核心時排除 core0。`severe_lps` 不參與排除。
- `src-tauri/src/benchmark/metrics.rs` 以 `MsBetweenPresents` 建立 FPS、低百分位、MAD 與相對慢幀率。

推論：GPU-only benchmark 沒有驗證之後新增的「遊戲 CPU 排除規則」。最佳 LP 排名、最佳 LP 對次佳 LP 的確認，不能代替「原始 OS/driver 策略對完整套用方案」的比較。即使 microbenchmark 排名可重現，跨 workload 的可轉移性仍未知；6 核心門檻與排除 core0 也仍屬啟發式。

## PresentMon 能測到什麼

PresentMon 2.5.1 官方文件將 `MsBetweenPresents` 定義為相鄰 `Present()` 呼叫間隔；`DisplayedTime` 是畫面在螢幕停留多久，未顯示的幀可為 `NA`；`MsBetweenDisplayChange` 是實際顯示變更間隔。這些是不同測量點。[PresentMon v2.5.1：Console CSV columns](https://github.com/GameTechDev/PresentMon/blob/v2.5.1/README-ConsoleApplication.md#csv-columns)

推論：只分析 `MsBetweenPresents`，能支持應用程式提交節奏的結論，但不足以直接證明玩家看到的畫面間隔更均勻或輸入延遲下降。驗證畫面平順度應同時保留 displayed timing、未顯示幀、PresentMode；若測試 frame generation，還應區分應用程式幀與產生幀，避免混用分母。[PresentMon v2.5.1：CSV 欄位與 capture options](https://github.com/GameTechDev/PresentMon/blob/v2.5.1/README-ConsoleApplication.md)

`--v1_metrics` 的 `Dropped = 1` 指該幀未顯示，`0` 指已顯示；這不是 ETW event loss。若高速 microbenchmark 大量 presents 未實際顯示，提交速率改善更不能直接等同顯示平順度改善。PresentMon 2.5.1 文件明確把 v1 欄位定義導向下列 1.x 文件。[PresentMon 1.9.2：CSV columns](https://github.com/GameTechDev/PresentMon/blob/v1.9.2/README.md#csv-columns)

本機指標還有兩個純數學限制：

1. `1% Low` 使用最慢幀的 instantaneous FPS 算術平均，不是最慢 frametime 平均後取倒數。兩者都可定義，但必須明示，不能與使用不同算法的軟體直接比較。例如慢幀為 10ms、100ms 時，前者是 55 FPS，後者約 18.18 FPS。
2. 相對慢幀門檻 `frametime > 2 × median` 會隨基準改變。整體都變慢卻同樣均勻的結果，其 MAD 百分比與相對 spike rate 仍可能好看。應加上絕對 ms 門檻及 throughput 不退步條件。

30 秒、240 FPS 約有 7,200 幀；0.1% 尾端平均僅約 8 幀，0.01% 與 0.005% 尾端平均各只剩 1 幀。這是由目前 `ceil(n*q)` 定義直接算出的樣本量，無法把極低百分位當成穩健長期統計。

## 建議的驗證實驗

下列為研究設計建議，並非已完成結果。

1. 使用同一遊戲版本、固定路線／replay／save、同一解析度與 frame cap，比較四組：原始設定、只改 GPU interrupt affinity、只改 CPU 集合、兩者合用。優先序變更另列實驗，才能辨識是哪個處理造成改善。
2. 每組跨多次獨立啟動、不同時段做交錯配對；固定或記錄暖機、溫度、時脈、driver、HAGS、VRR/VSync、Game Mode、背景負載。將時間漂移視為 block，block 內隨機順序。這符合 NIST 以 blocking 控制已知干擾、以 randomization 分散其餘干擾的設計原則。[NIST：Randomized block designs](https://www.itl.nist.gov/div898/handbook/pri/section3/pri332.htm)
3. 主要報告每次執行的 displayed frametime p99/p99.9、超過固定 frame budget 的幀數及時間占比，另報 average FPS、presented timing、輸入延遲（可量測時）與個別長停頓。不可只報加權總分。
4. 使用 ETW 驗證 GPU ISR/DPC 是否確實搬移、主／render 執行緒是否減少 ready wait 或被中斷時間。避免只因統計相關便宣稱機制已成立。[Microsoft：Measuring DPC/ISR Time](https://learn.microsoft.com/en-us/windows-hardware/drivers/devtest/example-15--measuring-dpc-isr-time)
5. 先固定最小實務改善門檻與樣本數規則；以獨立 run／block 作重抽樣單位，保留篩選外的確認資料，再做跨日 holdout。不要把同一段 capture 的數千相關幀誤當數千次獨立實驗。
6. 至少涵蓋 CPU 受限、多執行緒遊戲、GPU 受限三類場景，並分開報 Intel hybrid、AMD 單 CCD 與多 CCD 結果。沒有優勢時維持預設，不能讓排行榜被迫產生推薦。

## 查核方式與界線

依專案 Context7 規則先執行 `npx ctx7@latest library "Windows API" ...`，取得 `/websites/microsoft_github_io_windows-docs-rs_doc_windows`，再執行 `docs` 核對 Rust Windows bindings 的 `SetProcessAffinityMask` 與 `SetThreadIdealProcessorEx` 簽章。共 2 個 Context7 指令，無配額或網路錯誤；Context7 僅回傳部分函式簽章，所以 API 語意另外直接閱讀 Microsoft 官方文件。PresentMon 使用與專案內附版本一致的 `v2.5.1` 文件。沒有採用論壇效能案例作為普遍有效的證據，也未進行會變更 GPU 或行程排程的操作。
