//! 基準測試分析（Task 2）：PresentMon CSV 解析、frametime→FPS、
//! AutoGpuAffinity 相容的統計（Max/Avg/Min/STDEV + 1/0.1/0.01/0.005
//! frame-count percentile 與 Low）、dense rank、最佳 LP、嚴重 LP。
//!
//! Low/percentile 採 frame-count 演算法（最慢 N% 個 instantaneous FPS），
//! 不用 time-weighted，避免少量超長 frame 把 Low 壓成 Min。
//!
//! 全為純函式，fixture 測試不需真實 GPU/驅動。

use super::LpResult;

/// PresentMon CSV 中 frametime 欄位名（毫秒）
pub const COL_MS_BETWEEN_PRESENTS: &str = "msBetweenPresents";

/// 解析 PresentMon 1.x/2.x CSV，回傳 `MsBetweenPresents` 的 frametime 序列。
/// 欄名不分大小寫；2.x 的 `NA`/空值列會跳過。空 header、缺欄或沒有任何
/// 有效有限正數 → Err（該 CSV 視為無效）。
pub fn parse_presentmon_csv(text: &str) -> Result<Vec<f64>, String> {
    let mut header_idx: Option<usize> = None;
    let mut frames: Vec<f64> = Vec::new();
    let mut saw_data = false;

    for (lineno, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let fields = split_csv_line(line);
        if header_idx.is_none() {
            if let Some(i) = fields
                .iter()
                .position(|f| f.trim().eq_ignore_ascii_case(COL_MS_BETWEEN_PRESENTS))
            {
                header_idx = Some(i);
            }
            continue; // header 行本身不是資料
        }
        let idx = header_idx.unwrap();
        if idx >= fields.len() {
            continue; // 該列缺欄 → 跳過
        }
        let value = fields[idx].trim();
        if value.is_empty() || value.eq_ignore_ascii_case("NA") {
            continue;
        }
        let v: f64 = value.parse().map_err(|_| {
            format!(
                "CSV 第 {} 行 msBetweenPresents 非數值: {:?}",
                lineno + 1,
                fields[idx]
            )
        })?;
        if v.is_finite() && v > 0.0 {
            frames.push(v);
            saw_data = true;
        }
    }

    match header_idx {
        None => Err("CSV 缺 msBetweenPresents 欄位".to_string()),
        Some(_) if !saw_data => Err("CSV 沒有有效 frametime 資料".to_string()),
        Some(_) => Ok(frames),
    }
}

/// 完整 capture 解析結果：capture 時間（供 duration/monotonic 驗證）。
/// frametime 序列在解析期即驗證（finite positive、非空），不在此回傳。
#[derive(Debug, Clone, Default)]
pub struct CsvCapture {
    /// 最早 capture 時間（秒）；僅當 CSV 含 `TimeInSeconds` 欄位時才有值。
    pub first_time_secs: Option<f64>,
    /// 最晚 capture 時間（秒）。
    pub last_time_secs: Option<f64>,
    /// capture 時間序列是否單調非遞減（有時間欄位才判定；無欄位 = true）。
    pub monotonic: bool,
}

impl CsvCapture {
    /// 觀測到的 capture 時長（秒）：`last - first`；無秒數欄位或非有限 → None。
    pub fn observed_duration_secs(&self) -> Option<f64> {
        let (a, b) = (self.first_time_secs?, self.last_time_secs?);
        if !a.is_finite() || !b.is_finite() || b < a {
            return None;
        }
        Some(b - a)
    }
}

/// PresentMon CSV 的 capture 時間欄位名（秒，浮點；PresentMon 2.x 輸出）。
pub const COL_TIME_IN_SECONDS: &str = "TimeInSeconds";
/// PresentMon CSV 的 QPC 刻度時間欄位名（單調、非秒）。
pub const COL_QPC_TIME: &str = "QpcTime";
/// PresentMon CSV 的顯示變更間隔欄位名（毫秒；實際螢幕更新節奏）。
pub const COL_MS_BETWEEN_DISPLAY_CHANGE: &str = "msBetweenDisplayChange";
/// PresentMon CSV 的 present→顯示延遲欄位名（毫秒；未顯示的幀為 NA）。
pub const COL_MS_UNTIL_DISPLAYED: &str = "msUntilDisplayed";
/// PresentMon CSV 的丟幀旗標欄位名（1 = 該 present 未顯示）。
pub const COL_DROPPED: &str = "Dropped";

/// 顯示端序列：display-change 間隔、present→display 延遲與丟幀計數。
/// 序列皆已過濾（NA/非有限剔除）；`presents` 為有效 frametime 列數
/// （丟幀分母）；`dropped_seen` 記錄 CSV 是否含 `Dropped` 欄。
#[derive(Debug, Clone, Default)]
pub struct DisplaySeries {
    pub intervals: Vec<f64>,
    pub latencies: Vec<f64>,
    pub dropped_count: u32,
    pub dropped_seen: bool,
    pub presents: u32,
}

/// 單次解析的完整序列：frametime + capture 時間 + 顯示端序列。
#[derive(Debug, Clone, Default)]
pub struct FullSeries {
    pub frames: Vec<f64>,
    pub capture: CsvCapture,
    pub display: DisplaySeries,
}

/// 解析 PresentMon CSV，同時擷取 capture 時間（供完整性驗證）。
/// 與 [`parse_presentmon_csv`] 相同的 frametime 語意（跳過 NA/非有限/非正值）；
/// 額外追蹤 `TimeInSeconds`（秒，供 `observed_duration_secs`）與時間單調性
/// （優先 `TimeInSeconds`、其次 `QpcTime`）。時間欄位缺失 → 對應欄位為 None、
/// `monotonic=true`（無資料不視為不單調）。
pub fn parse_presentmon_csv_full(text: &str) -> Result<CsvCapture, String> {
    let mut header_idx: Option<usize> = None;
    let mut seconds_idx: Option<usize> = None;
    let mut qpc_idx: Option<usize> = None;
    let mut saw_data = false;

    let mut first_secs: Option<f64> = None;
    let mut last_secs: Option<f64> = None;
    let mut prev_time: Option<f64> = None;
    let mut monotonic = true;

    for (lineno, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let fields = split_csv_line(line);
        if header_idx.is_none() {
            if let Some(i) = fields
                .iter()
                .position(|f| f.trim().eq_ignore_ascii_case(COL_MS_BETWEEN_PRESENTS))
            {
                header_idx = Some(i);
            }
            if let Some(i) = fields
                .iter()
                .position(|f| f.trim().eq_ignore_ascii_case(COL_TIME_IN_SECONDS))
            {
                seconds_idx = Some(i);
            }
            if let Some(i) = fields
                .iter()
                .position(|f| f.trim().eq_ignore_ascii_case(COL_QPC_TIME))
            {
                qpc_idx = Some(i);
            }
            continue;
        }
        let idx = header_idx.unwrap();
        // 時間欄位（優先秒數、其次 QPC）的單調性與範圍。
        let time_val: Option<f64> = seconds_idx
            .and_then(|i| fields.get(i))
            .and_then(|v| v.trim().parse::<f64>().ok())
            .or_else(|| {
                qpc_idx
                    .and_then(|i| fields.get(i))
                    .and_then(|v| v.trim().parse::<f64>().ok())
            });
        if let Some(t) = time_val {
            if let Some(p) = prev_time {
                if t < p {
                    monotonic = false;
                }
            }
            prev_time = Some(t);
        }
        if seconds_idx.is_some() {
            if let Some(s) = seconds_idx.and_then(|i| fields.get(i)) {
                if let Ok(v) = s.trim().parse::<f64>() {
                    if v.is_finite() {
                        if first_secs.is_none() {
                            first_secs = Some(v);
                        }
                        last_secs = Some(v);
                    }
                }
            }
        }
        if idx >= fields.len() {
            continue;
        }
        let value = fields[idx].trim();
        if value.is_empty() || value.eq_ignore_ascii_case("NA") {
            continue;
        }
        let v: f64 = value.parse().map_err(|_| {
            format!(
                "CSV 第 {} 行 msBetweenPresents 非數值: {:?}",
                lineno + 1,
                fields[idx]
            )
        })?;
        if v.is_finite() && v > 0.0 {
            saw_data = true;
        }
    }

    match header_idx {
        None => Err("CSV 缺 msBetweenPresents 欄位".to_string()),
        Some(_) if !saw_data => Err("CSV 沒有有效 frametime 資料".to_string()),
        Some(_) => Ok(CsvCapture {
            first_time_secs: first_secs,
            last_time_secs: last_secs,
            monotonic,
        }),
    }
}

/// 解析 PresentMon CSV 的完整序列：frametime、capture 時間、顯示端序列
/// （display-change 間隔、present→display 延遲、丟幀）。
/// frametime/時間語意與 [`parse_presentmon_csv_full`] 完全相同（含相同錯誤條件）；
/// 顯示端欄位缺失時，`DisplaySeries` 對應序列為空、`dropped_seen=false`
/// （不影響主體有效性）。延遲取 finite ≥0；display-change 間隔取 finite >0。
pub fn parse_presentmon_series(text: &str) -> Result<FullSeries, String> {
    let mut idxs = ColumnIndexes::default();
    let mut out = FullSeries::default();
    out.capture.monotonic = true; // 無時間欄位/無資料 = 不視為不單調（與既有語意一致）
    let mut saw_data = false;
    let mut prev_time: Option<f64> = None;

    for (lineno, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let fields = split_csv_line(line);
        if !idxs.found_header {
            idxs.locate(&fields); // header 行本身不是資料
            continue;
        }
        // 時間欄位（優先秒數、其次 QPC）的單調性與範圍。
        let time_val: Option<f64> = idxs
            .seconds
            .and_then(|i| fields.get(i))
            .and_then(|v| v.trim().parse::<f64>().ok())
            .or_else(|| {
                idxs.qpc
                    .and_then(|i| fields.get(i))
                    .and_then(|v| v.trim().parse::<f64>().ok())
            });
        if let Some(t) = time_val {
            if let Some(p) = prev_time {
                if t < p {
                    out.capture.monotonic = false;
                }
            }
            prev_time = Some(t);
        }
        if idxs.seconds.is_some() {
            if let Some(s) = idxs.seconds.and_then(|i| fields.get(i)) {
                if let Ok(v) = s.trim().parse::<f64>() {
                    if v.is_finite() {
                        if out.capture.first_time_secs.is_none() {
                            out.capture.first_time_secs = Some(v);
                        }
                        out.capture.last_time_secs = Some(v);
                    }
                }
            }
        }
        if idxs.frames >= fields.len() {
            continue;
        }
        let value = fields[idxs.frames].trim();
        if value.is_empty() || value.eq_ignore_ascii_case("NA") {
            continue;
        }
        let v: f64 = value.parse().map_err(|_| {
            format!(
                "CSV 第 {} 行 msBetweenPresents 非數值: {:?}",
                lineno + 1,
                fields[idxs.frames]
            )
        })?;
        if !(v.is_finite() && v > 0.0) {
            continue;
        }
        saw_data = true;
        out.frames.push(v);
        out.display.presents += 1;
        // 顯示端欄位（同列；缺欄/NA/非有限一律跳過，不影響 frametime 有效性）
        if let Some(i) = idxs.display_change {
            if let Some(f) = fields.get(i) {
                if let Ok(d) = f.trim().parse::<f64>() {
                    if d.is_finite() && d > 0.0 {
                        out.display.intervals.push(d);
                    }
                }
            }
        }
        if let Some(i) = idxs.until_displayed {
            if let Some(f) = fields.get(i) {
                if let Ok(l) = f.trim().parse::<f64>() {
                    if l.is_finite() && l >= 0.0 {
                        out.display.latencies.push(l);
                    }
                }
            }
        }
        if let Some(i) = idxs.dropped {
            out.display.dropped_seen = true;
            if let Some(f) = fields.get(i) {
                if let Ok(d) = f.trim().parse::<f64>() {
                    if d.is_finite() && d >= 0.5 {
                        out.display.dropped_count += 1;
                    }
                }
            }
        }
    }

    match idxs.frames_found {
        false => Err("CSV 缺 msBetweenPresents 欄位".to_string()),
        true if !saw_data => Err("CSV 沒有有效 frametime 資料".to_string()),
        true => Ok(out),
    }
}

/// 各欄位的 header 位置（含是否已找到 header）。
#[derive(Default)]
struct ColumnIndexes {
    found_header: bool,
    frames_found: bool,
    frames: usize,
    seconds: Option<usize>,
    qpc: Option<usize>,
    display_change: Option<usize>,
    until_displayed: Option<usize>,
    dropped: Option<usize>,
}

impl ColumnIndexes {
    fn locate(&mut self, fields: &[String]) {
        for (i, f) in fields.iter().enumerate() {
            let name = f.trim();
            if name.eq_ignore_ascii_case(COL_MS_BETWEEN_PRESENTS) {
                self.frames = i;
                self.frames_found = true;
            } else if name.eq_ignore_ascii_case(COL_TIME_IN_SECONDS) {
                self.seconds = Some(i);
            } else if name.eq_ignore_ascii_case(COL_QPC_TIME) {
                self.qpc = Some(i);
            } else if name.eq_ignore_ascii_case(COL_MS_BETWEEN_DISPLAY_CHANGE) {
                self.display_change = Some(i);
            } else if name.eq_ignore_ascii_case(COL_MS_UNTIL_DISPLAYED) {
                self.until_displayed = Some(i);
            } else if name.eq_ignore_ascii_case(COL_DROPPED) {
                self.dropped = Some(i);
            }
        }
        self.found_header = self.frames_found;
    }
}

/// 對含引號欄位的 CSV 列做正確的欄位切割（PresentMon 的 Application 帶引號）
fn split_csv_line(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                if in_quotes && chars.peek() == Some(&'"') {
                    cur.push('"');
                    chars.next();
                } else {
                    in_quotes = !in_quotes;
                }
            }
            ',' if !in_quotes => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

/// frametime(ms) → 即時 FPS（1000/ms）。輸入皆 > 0。
pub fn frame_times_to_fps(frames: &[f64]) -> Vec<f64> {
    frames.iter().map(|&ms| 1000.0 / ms).collect()
}

/// Bessel 校正（n-1）的母體標準差
fn stdev_bessel(values: &[f64]) -> f64 {
    let n = values.len();
    if n < 2 {
        return 0.0;
    }
    let mean = values.iter().sum::<f64>() / n as f64;
    let var = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n as f64 - 1.0);
    var.sqrt()
}

/// percentile FPS：instantaneous FPS 升冪排序後取 index = floor((n-1)*q)。
/// `q` ∈ (0,1]（例：1% percentile → 0.01）。這是「最慢 N% 的分位數」，不是平均。
/// 空序列 → None。
pub fn percentile_fps(fps: &[f64], q: f64) -> Option<f64> {
    if fps.is_empty() {
        return None;
    }
    let mut sorted = fps.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = (((sorted.len() - 1) as f64) * q).floor() as usize;
    Some(sorted[idx.min(sorted.len() - 1)])
}

/// N% Low：最慢 max(1, ceil(n*q)) 個 instantaneous FPS 的算術平均。
/// `q` ∈ (0,1]（例：1% low → 0.01）。慢 = FPS 低，故取升冪排序的前 count 個。
/// 空序列 → None。frame-count 演算法不會被少量超長 frame 壓成 Min。
pub fn n_pct_low_fps(fps: &[f64], q: f64) -> Option<f64> {
    if fps.is_empty() {
        return None;
    }
    let mut sorted = fps.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let count = (((sorted.len() as f64) * q).ceil() as usize)
        .max(1)
        .min(sorted.len());
    let sum: f64 = sorted[..count].iter().sum();
    Some(sum / count as f64)
}

/// 從合併後的 frametime 序列計算單一 LP 的完整指標。
/// 空序列 → Err（該 LP 無效）。
pub fn compute_lp_result(lp: u32, frames: &[f64]) -> Result<LpResult, String> {
    if frames.is_empty() {
        return Err(format!("LP {lp} 無 frametime 資料"));
    }
    let fps = frame_times_to_fps(frames);
    let n = fps.len() as u32;
    let avg_ft = frames.iter().sum::<f64>() / frames.len() as f64;
    // AutoGpuAffinity 相容的 Avg FPS：1000 / mean(frametime_ms)，
    // 不是 mean(1000/frametime)（對非均勻 frametime 兩者不同）
    let avg = 1000.0 / avg_ft;
    // Max/Min 與 STDEV 仍是針對 instantaneous FPS
    let max = fps.iter().cloned().fold(f64::MIN, f64::max);
    let min = fps.iter().cloned().fold(f64::MAX, f64::min);
    Ok(LpResult {
        lp,
        avg_fps: Some(avg),
        max_fps: Some(max),
        min_fps: Some(min),
        stdev_fps: Some(stdev_bessel(&fps)),
        p1_low: n_pct_low_fps(&fps, 0.01),
        p01_low: n_pct_low_fps(&fps, 0.001),
        p001_low: n_pct_low_fps(&fps, 0.0001),
        p0005_low: n_pct_low_fps(&fps, 0.00005),
        p1_percentile: percentile_fps(&fps, 0.01),
        p01_percentile: percentile_fps(&fps, 0.001),
        p001_percentile: percentile_fps(&fps, 0.0001),
        p0005_percentile: percentile_fps(&fps, 0.00005),
        avg_frame_time_ms: Some(avg_ft),
        frametime_mad_pct: frametime_mad_pct(frames),
        spike_rate_pct: spike_rate_pct(frames),
        sample_count: n,
        // 顯示端指標只由 attach_display_metrics 填入
        displayed_avg_fps: None,
        displayed_p1_low: None,
        display_latency_avg_ms: None,
        display_latency_p99_ms: None,
        dropped_pct: None,
        completed: true,
        error: None,
    })
}

/// 顯示端指標（皆 Option：欄位缺失或無有效樣本 → None）。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DisplayMetrics {
    /// 顯示端平均 FPS：1000 / mean(display-change 間隔)
    pub displayed_avg_fps: Option<f64>,
    /// 顯示端 1% low（frame-count，作用於 display-change 間隔）
    pub displayed_p1_low: Option<f64>,
    /// present→顯示延遲平均（毫秒）
    pub display_latency_avg_ms: Option<f64>,
    /// present→顯示延遲 p99（毫秒；10-20s capture 樣本數有限，不做 p99.9）
    pub display_latency_p99_ms: Option<f64>,
    /// 丟幀佔比（%；CSV 無 Dropped 欄 → None）
    pub dropped_pct: Option<f64>,
}

/// 由顯示端序列計算顯示端指標（純函式）。
pub fn compute_display_metrics(series: &DisplaySeries) -> DisplayMetrics {
    let fps = frame_times_to_fps(&series.intervals);
    DisplayMetrics {
        displayed_avg_fps: (!series.intervals.is_empty())
            .then(|| 1000.0 / (series.intervals.iter().sum::<f64>() / series.intervals.len() as f64))
            .filter(|v| v.is_finite()),
        displayed_p1_low: n_pct_low_fps(&fps, 0.01),
        display_latency_avg_ms: (!series.latencies.is_empty()).then(|| {
            series.latencies.iter().sum::<f64>() / series.latencies.len() as f64
        }),
        display_latency_p99_ms: percentile_fps(&series.latencies, 0.99),
        dropped_pct: (series.dropped_seen && series.presents > 0).then(|| {
            series.dropped_count as f64 / series.presents as f64 * 100.0
        }),
    }
}

/// 把顯示端指標併入既有 LpResult（就地覆寫 5 個 display 欄位）。
pub fn attach_display_metrics(r: &mut LpResult, series: &DisplaySeries) {
    let dm = compute_display_metrics(series);
    r.displayed_avg_fps = dm.displayed_avg_fps;
    r.displayed_p1_low = dm.displayed_p1_low;
    r.display_latency_avg_ms = dm.display_latency_avg_ms;
    r.display_latency_p99_ms = dm.display_latency_p99_ms;
    r.dropped_pct = dm.dropped_pct;
}

/// 把各 round 的 frametime 合併成單一序列（round 順序不重要，統計用）
pub fn merge_rounds(per_round: &[Vec<f64>]) -> Vec<f64> {
    per_round.iter().flatten().copied().collect()
}

/// 合併各 round 的顯示端序列（round 順序不重要；計數相加、旗標 OR）
pub fn merge_display_rounds(per_round: &[DisplaySeries]) -> DisplaySeries {
    let mut out = DisplaySeries::default();
    for s in per_round {
        out.intervals.extend_from_slice(&s.intervals);
        out.latencies.extend_from_slice(&s.latencies);
        out.dropped_count = out.dropped_count.saturating_add(s.dropped_count);
        out.dropped_seen |= s.dropped_seen;
        out.presents = out.presents.saturating_add(s.presents);
    }
    out
}

// ── frametime 穩健性指標（與 workload 無關） ─────────────────────────────

/// frametime MAD（中位數絕對差），正規化為 frametime 中位數的百分比。
/// 空序列、中位數非有限或 ≤0 → None（守門，避免除零）。
pub fn frametime_mad_pct(frames: &[f64]) -> Option<f64> {
    if frames.is_empty() {
        return None;
    }
    let med = median(frames);
    if !med.is_finite() || med <= 0.0 {
        return None;
    }
    let devs: Vec<f64> = frames.iter().map(|f| (f - med).abs()).collect();
    let mad = median(&devs);
    if !mad.is_finite() {
        return None;
    }
    Some(mad / med * 100.0)
}

/// 慢幀 spike rate：frametime 超過 2×中位數的幀佔比（百分比）。
/// 空序列、中位數非有限或 ≤0 → None。
pub fn spike_rate_pct(frames: &[f64]) -> Option<f64> {
    if frames.is_empty() {
        return None;
    }
    let med = median(frames);
    if !med.is_finite() || med <= 0.0 {
        return None;
    }
    let threshold = 2.0 * med;
    let n = frames.len();
    let spikes = frames.iter().filter(|&&f| f > threshold).count();
    Some(spikes as f64 / n as f64 * 100.0)
}

// ── 穩健推薦分數（逐 round 競爭分數 + 跨 round 候選） ────────────────────

/// 穩健推薦分數權重（總和 1.0）：tail latency / stability 主導，
/// 平均吞吐量只佔 10%。
pub const W_P1_LOW: f64 = 0.30;
pub const W_P01_LOW: f64 = 0.15;
pub const W_MAD_INV: f64 = 0.25;
pub const W_SPIKE_INV: f64 = 0.20;
pub const W_AVG_FPS: f64 = 0.10;

/// 正規化比值：`value` 相對同 round 合格 LP 的中位數。
/// - `higher_is_better=true`（1% low / 0.1% low / Avg FPS）→ `value / median`。
/// - `false`（frametime MAD / spike rate）→ 對稱比值 `2*median/(median+value)`，
///   值域 (0,2]：value=0（完美）→ 2、value=median → 1、value→∞ → 0。
///
/// 守門：median 或 value 非有限 → 中性 1.0；higher-is-better 且 median≤0 或
/// value≤0 → 中性 1.0；lower-is-better 且 median<0 或 value<0 → 中性 1.0。
/// lower-is-better 且 median==0：value==0 → 1.0（中性）、value>0 → 0.0（懲罰），
/// 使跨 LP 中位數為 0 時仍能區分零值與正值。
pub fn normalized_ratio(value: f64, median: f64, higher_is_better: bool) -> f64 {
    if !value.is_finite() || !median.is_finite() {
        return 1.0;
    }
    if higher_is_better {
        if median <= 0.0 || value <= 0.0 {
            return 1.0;
        }
        return value / median;
    }
    if median < 0.0 || value < 0.0 {
        return 1.0;
    }
    if median == 0.0 {
        return if value == 0.0 { 1.0 } else { 0.0 };
    }
    let r = 2.0 * median / (median + value);
    if !r.is_finite() {
        1.0
    } else {
        r
    }
}

/// 是否可參與競爭分數：completed 且 Avg/1% low/0.1% low/MAD/spike 五項皆有限。
pub fn is_competitive_eligible(r: &LpResult) -> bool {
    let finite = |o: Option<f64>| o.is_some_and(|v| v.is_finite());
    r.completed
        && finite(r.avg_fps)
        && finite(r.p1_low)
        && finite(r.p01_low)
        && finite(r.frametime_mad_pct)
        && finite(r.spike_rate_pct)
}

/// 單一 round 內合格 LP 的五項指標中位數。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RoundMedians {
    pub p1_low: f64,
    pub p01_low: f64,
    pub mad: f64,
    pub spike: f64,
    pub avg: f64,
}

/// 由一組同 round 的 LpResult 計算五項中位數（僅含 competitive-eligible 者；
/// 空集合 → 全 0）。
pub fn round_medians(round_results: &[LpResult]) -> RoundMedians {
    let eligible: Vec<&LpResult> = round_results
        .iter()
        .filter(|r| is_competitive_eligible(r))
        .collect();
    RoundMedians {
        p1_low: median(&eligible.iter().filter_map(|r| r.p1_low).collect::<Vec<_>>()),
        p01_low: median(
            &eligible
                .iter()
                .filter_map(|r| r.p01_low)
                .collect::<Vec<_>>(),
        ),
        mad: median(
            &eligible
                .iter()
                .filter_map(|r| r.frametime_mad_pct)
                .collect::<Vec<_>>(),
        ),
        spike: median(
            &eligible
                .iter()
                .filter_map(|r| r.spike_rate_pct)
                .collect::<Vec<_>>(),
        ),
        avg: median(
            &eligible
                .iter()
                .filter_map(|r| r.avg_fps)
                .collect::<Vec<_>>(),
        ),
    }
}

/// 單一 LP 在某 round 的競爭分數（相對於該 round 合格 LP 的中位數）。
/// 五項加權：30% 1% low、15% 0.1% low、25% 反比 MAD、20% 反比 spike、10% Avg FPS。
/// 任一必要指標缺失 → None。
pub fn competitive_score(r: &LpResult, med: &RoundMedians) -> Option<f64> {
    if !is_competitive_eligible(r) {
        return None;
    }
    let r_p1 = normalized_ratio(r.p1_low?, med.p1_low, true);
    let r_p01 = normalized_ratio(r.p01_low?, med.p01_low, true);
    let r_mad = normalized_ratio(r.frametime_mad_pct?, med.mad, false);
    let r_spike = normalized_ratio(r.spike_rate_pct?, med.spike, false);
    let r_avg = normalized_ratio(r.avg_fps?, med.avg, true);
    Some(
        W_P1_LOW * r_p1
            + W_P01_LOW * r_p01
            + W_MAD_INV * r_mad
            + W_SPIKE_INV * r_spike
            + W_AVG_FPS * r_avg,
    )
}

/// 跨 round 穩健候選：`scores_by_lp` 為每 LP 的逐 round 競爭分數。
/// 依跨 round 分數中位數降序；平手取 worst-round 分數較高；再取較小 LP。
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg(test)]
pub struct RobustCandidate {
    pub lp: u32,
    pub median_score: f64,
    pub worst_round_score: f64,
}

#[cfg(test)]
pub fn robust_candidates(scores_by_lp: &[(u32, Vec<f64>)]) -> Vec<RobustCandidate> {
    let mut out: Vec<RobustCandidate> = scores_by_lp
        .iter()
        .filter_map(|(lp, scores)| {
            if scores.is_empty() {
                return None;
            }
            let med = median(scores);
            let worst = scores.iter().cloned().fold(f64::INFINITY, f64::min);
            Some(RobustCandidate {
                lp: *lp,
                median_score: med,
                worst_round_score: worst,
            })
        })
        .collect();
    out.sort_by(|a, b| {
        b.median_score
            .partial_cmp(&a.median_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                b.worst_round_score
                    .partial_cmp(&a.worst_round_score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.lp.cmp(&b.lp))
    });
    out
}

// ── 確認專用分數（有界 log-ratio；與 screening/refinement 的 competitive_score 分離）──

/// 確認分數權重（總和 1.0）：1% low 主導、0.1% low 次之、MAD 與 avg 為輔。
/// spike 不進主分數，只當 guardrail。
#[cfg(test)]
pub const CONFIRM_W_P1_LOW: f64 = 0.40;
#[cfg(test)]
pub const CONFIRM_W_P01_LOW: f64 = 0.20;
#[cfg(test)]
pub const CONFIRM_W_MAD: f64 = 0.25;
#[cfg(test)]
pub const CONFIRM_W_AVG_FPS: f64 = 0.15;

/// 有界 log-ratio 效應（每項 clamp 到 [-5, +5]）：
/// - `higher_is_better=true` → `100 * ln(candidate / runner)`。
/// - `higher_is_better=false`（MAD 越低越好）→ `100 * ln(runner / candidate)`。
///
/// 守門：任一值非有限或 ≤0 → None（fail closed，不給中性分）。
#[cfg(test)]
pub fn confirmation_log_ratio(candidate: f64, runner: f64, higher_is_better: bool) -> Option<f64> {
    if !candidate.is_finite() || !runner.is_finite() || candidate <= 0.0 || runner <= 0.0 {
        return None;
    }
    let ratio = if higher_is_better {
        candidate / runner
    } else {
        runner / candidate
    };
    let effect = 100.0 * ratio.ln();
    if effect.is_finite() {
        Some(effect.clamp(-5.0, 5.0))
    } else {
        None
    }
}

/// 單一 (candidate vs runner) 配對的確認複合分數：加權有界 log-ratio。
/// 任一必要指標（avg / 1% low / 0.1% low / MAD）非有限或 ≤0 → None（fail closed）。
/// spike 不進主分數（只當 guardrail）。
#[cfg(test)]
pub fn confirmation_effect(candidate: &LpResult, runner: &LpResult) -> Option<f64> {
    let avg = confirmation_log_ratio(candidate.avg_fps?, runner.avg_fps?, true)?;
    let p1 = confirmation_log_ratio(candidate.p1_low?, runner.p1_low?, true)?;
    let p01 = confirmation_log_ratio(candidate.p01_low?, runner.p01_low?, true)?;
    let mad = confirmation_log_ratio(
        candidate.frametime_mad_pct?,
        runner.frametime_mad_pct?,
        false,
    )?;
    Some(
        CONFIRM_W_P1_LOW * p1
            + CONFIRM_W_P01_LOW * p01
            + CONFIRM_W_MAD * mad
            + CONFIRM_W_AVG_FPS * avg,
    )
}

// ── 排序與選擇 ──────────────────────────────────────────────────────────

/// 單一欄位的 dense rank（1 起跳、同值同 rank、無空位）。
/// `descending=true`：值越大 rank 越小（越好）。
/// 同時保留第 1、2 個不同值（顯示表用，避免外觀上的 tie）。
#[allow(dead_code)]
pub struct ColumnRank {
    pub ranks: Vec<usize>,
    // Task 3 顯示表用：保留第 1、2 個不同值，避免外觀 tie
    #[allow(dead_code)]
    pub first: Option<f64>,
    #[allow(dead_code)]
    pub second: Option<f64>,
}

#[allow(dead_code)]
pub fn rank_column(values: &[f64], descending: bool) -> ColumnRank {
    let mut order: Vec<usize> = (0..values.len()).collect();
    order.sort_by(|&a, &b| {
        let ord = values[a]
            .partial_cmp(&values[b])
            .unwrap_or(std::cmp::Ordering::Equal);
        if descending {
            ord.reverse()
        } else {
            ord
        }
    });
    let mut ranks = vec![0usize; values.len()];
    let mut distinct: Vec<f64> = Vec::new();
    let mut r = 0usize;
    for &i in &order {
        if distinct.last().map(|&d| d != values[i]).unwrap_or(true) {
            r += 1;
            distinct.push(values[i]);
        }
        ranks[i] = r;
    }
    ColumnRank {
        ranks,
        first: distinct.first().copied(),
        second: distinct.get(1).copied(),
    }
}

pub fn median(values: &[f64]) -> f64 {
    let mut v = values.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len();
    if n == 0 {
        return 0.0;
    }
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

/// 完成且四項排序指標皆有的結果（缺任一 → 無法參與排名）
fn complete_rows(results: &[LpResult]) -> Vec<&LpResult> {
    results
        .iter()
        .filter(|r| {
            r.completed
                && r.avg_fps.is_some()
                && r.p1_low.is_some()
                && r.p01_low.is_some()
                && r.stdev_fps.is_some()
        })
        .collect()
}

/// 依四欄 dense-rank 總和排序，回傳 LP 清單（最好在前）。
/// 排序鍵：總和（Avg desc、1% Low desc、0.1% Low desc、STDEV asc）越小越好；
/// 平手依 0.1% Low、1% Low、Avg（皆越高越好）、LP 越小越好。
/// 供 [`best_lp`]（候選）與可靠性亞軍共用同一套聚合排名。
#[allow(dead_code)]
pub fn ranked_lps(results: &[LpResult]) -> Vec<u32> {
    let rows = complete_rows(results);
    if rows.is_empty() {
        return Vec::new();
    }
    let avg_r = rank_column(
        &rows.iter().map(|r| r.avg_fps.unwrap()).collect::<Vec<_>>(),
        true,
    );
    let p1_r = rank_column(
        &rows.iter().map(|r| r.p1_low.unwrap()).collect::<Vec<_>>(),
        true,
    );
    let p01_r = rank_column(
        &rows.iter().map(|r| r.p01_low.unwrap()).collect::<Vec<_>>(),
        true,
    );
    let stdev_r = rank_column(
        &rows
            .iter()
            .map(|r| r.stdev_fps.unwrap())
            .collect::<Vec<_>>(),
        false,
    );

    let mut scored: Vec<(usize, f64, f64, f64, u32)> = rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let sum = avg_r.ranks[i] + p1_r.ranks[i] + p01_r.ranks[i] + stdev_r.ranks[i];
            (
                sum,
                row.p01_low.unwrap(),
                row.p1_low.unwrap(),
                row.avg_fps.unwrap(),
                row.lp,
            )
        })
        .collect();
    scored.sort_by(|a, b| {
        if a.0 != b.0 {
            return a.0.cmp(&b.0);
        }
        if a.1 != b.1 {
            return b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal);
        }
        if a.2 != b.2 {
            return b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal);
        }
        if a.3 != b.3 {
            return b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal);
        }
        a.4.cmp(&b.4)
    });
    scored.into_iter().map(|t| t.4).collect()
}

/// 最佳 LP：四欄 dense-rank 總和（Avg desc、1% Low desc、0.1% Low desc、
/// STDEV asc），平手依 0.1% Low、1% Low、Avg（皆越高越好）、LP 越小越好。
#[allow(dead_code)]
pub fn best_lp(results: &[LpResult]) -> Option<u32> {
    ranked_lps(results).into_iter().next()
}

/// 嚴重 LP：Avg、1% Low、0.1% Low 任一低於該指標中位數的 85%，
/// 或 STDEV 高於中位數的 150%。中位數 STDEV 為 0 時停用 STDEV 條件。
#[cfg(test)]
pub fn severe_lps(results: &[LpResult]) -> Vec<u32> {
    let rows = complete_rows(results);
    if rows.is_empty() {
        return Vec::new();
    }
    let med_avg = median(&rows.iter().map(|r| r.avg_fps.unwrap()).collect::<Vec<_>>());
    let med_p1 = median(&rows.iter().map(|r| r.p1_low.unwrap()).collect::<Vec<_>>());
    let med_p01 = median(&rows.iter().map(|r| r.p01_low.unwrap()).collect::<Vec<_>>());
    let med_stdev = median(
        &rows
            .iter()
            .map(|r| r.stdev_fps.unwrap())
            .collect::<Vec<_>>(),
    );
    let stdev_active = med_stdev > 0.0;

    rows.iter()
        .filter(|r| {
            let avg_sev = r.avg_fps.unwrap() < 0.85 * med_avg;
            let p1_sev = r.p1_low.unwrap() < 0.85 * med_p1;
            let p01_sev = r.p01_low.unwrap() < 0.85 * med_p01;
            let stdev_sev = stdev_active && r.stdev_fps.unwrap() > 1.5 * med_stdev;
            avg_sev || p1_sev || p01_sev || stdev_sev
        })
        .map(|r| r.lp)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 產生 N 個 frametime，圍繞 base 上下 jitter
    fn frames_n(base: f64, n: usize, jitter: f64) -> Vec<f64> {
        (0..n)
            .map(|i| {
                let j = if i % 2 == 0 { jitter } else { -jitter };
                (base + j).max(0.5)
            })
            .collect()
    }

    #[test]
    fn csv_parses_ms_between_presents_column() {
        let csv = "\
Application,ProcessID,SwapChainAddress,SyncInterval,msBetweenPresents,msInPresentAPI
\"game.exe (1234)\",1234,0x1234,1,16.7,0.5
\"game.exe (1234)\",1234,0x1234,1,8.3,0.4
";
        let frames = parse_presentmon_csv(csv).unwrap();
        assert_eq!(frames, vec![16.7, 8.3]);
    }

    #[test]
    fn csv_v2_header_is_case_insensitive_and_skips_na() {
        let csv = "Application,ProcessID,MsBetweenPresents\ngame.exe,42,NA\ngame.exe,42,0.125\n";
        assert_eq!(parse_presentmon_csv(csv).unwrap(), vec![0.125]);
    }

    #[test]
    fn csv_skips_non_finite_frame_times() {
        let csv = "Application,MsBetweenPresents\ngame.exe,NaN\ngame.exe,inf\ngame.exe,1.5\n";
        assert_eq!(parse_presentmon_csv(csv).unwrap(), vec![1.5]);
    }

    #[test]
    fn csv_with_quoted_comma_field_keeps_alignment() {
        let csv = "\
Application,ProcessID,msBetweenPresents
\"proc, with comma (7)\",7,10.0
\"proc2\",8,20.0
";
        let frames = parse_presentmon_csv(csv).unwrap();
        assert_eq!(frames, vec![10.0, 20.0]);
    }

    #[test]
    fn csv_missing_column_errors() {
        assert!(parse_presentmon_csv("Application,ProcessID\n1,2\n").is_err());
    }

    #[test]
    fn csv_empty_data_errors() {
        let csv = "Application,msBetweenPresents\n";
        assert!(parse_presentmon_csv(csv).is_err());
        // 全 0 / 負 frametime → 視為無資料
        let csv = "Application,msBetweenPresents\n0,0\n-1\n";
        assert!(parse_presentmon_csv(csv).is_err());
    }

    #[test]
    fn csv_invalid_numeric_row_errors() {
        let csv = "Application,msBetweenPresents\nabc,xyz\n";
        assert!(parse_presentmon_csv(csv).is_err());
    }

    #[test]
    fn fps_conversion_and_basic_stats() {
        let frames = frames_n(10.0, 4, 2.0); // 12,8,12,8
        let fps = frame_times_to_fps(&frames);
        let m = compute_lp_result(3, &frames).unwrap();
        assert_eq!(m.lp, 3);
        assert_eq!(m.sample_count, 4);
        // avg = 1000 / mean(frametime) = 1000 / 10 = 100（非 mean(instant fps) ≈ 104.17）
        assert_eq!(m.avg_fps.unwrap(), 100.0);
        assert_eq!(m.max_fps.unwrap(), 125.0);
        assert!((m.min_fps.unwrap() - 83.33333).abs() < 0.01);
        let _ = fps;
    }

    /// 非均勻 frametime：明確區分 1000/mean(frametime) 與 mean(1000/frametime)
    #[test]
    fn avg_fps_is_1000_over_mean_frame_time() {
        // frames [10, 20]ms：mean(instant FPS) = (100+50)/2 = 75；
        // AutoGpuAffinity 定義 avg = 1000 / 15 ≈ 66.67
        let m = compute_lp_result(0, &[10.0, 20.0]).unwrap();
        let expected = 1000.0 / 15.0;
        assert!(
            (m.avg_fps.unwrap() - expected).abs() < 1e-9,
            "avg={}",
            m.avg_fps.unwrap()
        );
        assert!(
            (m.avg_fps.unwrap() - 75.0).abs() > 1e-6,
            "不得是 mean(instant fps)"
        );
        assert_eq!(m.avg_frame_time_ms.unwrap(), 15.0);
        // max/min 仍是 instantaneous FPS
        assert_eq!(m.max_fps.unwrap(), 100.0);
        assert_eq!(m.min_fps.unwrap(), 50.0);
    }

    #[test]
    fn stdev_uses_bessel_correction() {
        // 兩樣本 [100, 110]：mean=105, 樣本變異=((5)^2+(-5)^2)/(2-1)=50, stdev≈7.071
        let frames = vec![10.0, 1000.0 / 110.0]; // fps=100,110
        let m = compute_lp_result(0, &frames).unwrap();
        let expected = (50.0_f64).sqrt();
        assert!((m.stdev_fps.unwrap() - expected).abs() < 1e-9);
        // 單一樣本 stdev = 0（Bessel n-1 = 0）
        let m1 = compute_lp_result(1, &[16.7]).unwrap();
        assert_eq!(m1.stdev_fps.unwrap(), 0.0);
    }

    #[test]
    fn percentile_fps_uses_sorted_index() {
        // FPS = [10, 20, 30, 40, 50]（升冪排序後）
        let fps = vec![30.0, 10.0, 50.0, 20.0, 40.0];
        // n=5：q=0.5 → idx = floor(4*0.5)=2 → 30
        assert_eq!(percentile_fps(&fps, 0.5), Some(30.0));
        // q=1.0 → idx = floor(4)=4 → 50（最大值）
        assert_eq!(percentile_fps(&fps, 1.0), Some(50.0));
        // q 極小 → idx=0 → 最小值
        assert_eq!(percentile_fps(&fps, 0.01), Some(10.0));
        assert_eq!(percentile_fps(&[], 0.01), None);
    }

    #[test]
    fn n_pct_low_averages_slowest_count() {
        // FPS = [10, 20, 30, 40, 50]，n=5
        let fps = vec![50.0, 30.0, 10.0, 40.0, 20.0];
        // q=0.4 → count = ceil(2)=2 → avg(10,20) = 15
        assert!((n_pct_low_fps(&fps, 0.4).unwrap() - 15.0).abs() < 1e-9);
        // q=0.01 → count = max(1,ceil(0.05))=1 → 10（單一最慢）
        assert_eq!(n_pct_low_fps(&fps, 0.01), Some(10.0));
        assert_eq!(n_pct_low_fps(&[], 0.01), None);
    }

    #[test]
    fn tiny_q_takes_at_least_one_frame() {
        let fps = vec![100.0, 200.0, 50.0];
        // q 極小（0.00001）：count = ceil(0.00003)=1 → 仍取最慢 1 個
        assert_eq!(n_pct_low_fps(&fps, 0.00001), Some(50.0));
        assert_eq!(percentile_fps(&fps, 0.00001), Some(50.0));
    }

    #[test]
    fn few_outliers_do_not_drag_p1_low_to_min() {
        // 999 個 100 FPS + 1 個 1 FPS（極端 outlier）。
        // 1% Low = ceil(1000*0.01)=10 個最慢的平均 = (1 + 9*100)/10 = 90.1，遠高於 Min(1)。
        let mut fps = vec![100.0; 999];
        fps.push(1.0);
        let low = n_pct_low_fps(&fps, 0.01).unwrap();
        assert!((low - 90.1).abs() < 1e-9, "low={low}");
        assert!(low > 1.0, "1% Low 不應等於 Min");
    }

    #[test]
    fn compute_lp_result_empty_errors() {
        assert!(compute_lp_result(0, &[]).is_err());
    }

    #[test]
    fn merge_rounds_concatenates() {
        let merged = merge_rounds(&[vec![1.0, 2.0], vec![3.0], vec![]]);
        assert_eq!(merged, vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn dense_rank_descending_with_ties() {
        let vals = [100.0, 200.0, 200.0, 50.0];
        let c = rank_column(&vals, true);
        // 降序：200,200 → rank1；100 → rank2；50 → rank3
        assert_eq!(c.ranks, vec![2, 1, 1, 3]);
        assert_eq!(c.first, Some(200.0));
        assert_eq!(c.second, Some(100.0));
    }

    #[test]
    fn dense_rank_ascending() {
        let vals = [10.0, 5.0, 5.0, 20.0];
        let c = rank_column(&vals, false);
        assert_eq!(c.ranks, vec![2, 1, 1, 3]);
    }

    fn lp(avg: f64, p1: f64, p01: f64, stdev: f64) -> LpResult {
        LpResult {
            lp: 0,
            avg_fps: Some(avg),
            p1_low: Some(p1),
            p01_low: Some(p01),
            stdev_fps: Some(stdev),
            completed: true,
            ..Default::default()
        }
    }

    #[test]
    fn best_lp_picks_sum_of_dense_ranks() {
        // LP 由 avg 欄位帶出
        let mut a = lp(100.0, 90.0, 80.0, 5.0);
        a.lp = 1;
        let mut b = lp(90.0, 85.0, 75.0, 2.0);
        b.lp = 2;
        let mut c = lp(95.0, 92.0, 78.0, 8.0);
        c.lp = 3;
        let r = best_lp(&[a, b, c]);
        assert_eq!(r, Some(1)); // LP1 avg/p1/p01 最好，總和最小
    }

    #[test]
    fn best_lp_tie_break_by_p01() {
        // 四欄 dense rank 總和相同（6=6），由 0.1% Low 較高者勝
        let mut a = lp(100.0, 80.0, 95.0, 10.0);
        a.lp = 4;
        let mut b = lp(80.0, 100.0, 80.0, 5.0);
        b.lp = 7;
        assert_eq!(best_lp(&[a, b]), Some(4));
    }

    #[test]
    fn best_lp_tie_break_lower_lp_when_all_equal() {
        let mut a = lp(100.0, 90.0, 80.0, 5.0);
        a.lp = 3;
        let mut b = lp(100.0, 90.0, 80.0, 5.0);
        b.lp = 6;
        assert_eq!(best_lp(&[a, b]), Some(3));
    }

    #[test]
    fn best_lp_skips_incomplete_rows() {
        let mut a = lp(100.0, 90.0, 80.0, 5.0);
        a.lp = 1;
        let b = LpResult {
            lp: 2, // completed=false
            ..Default::default()
        };
        let r = best_lp(&[a, b]);
        assert_eq!(r, Some(1));
    }

    #[test]
    fn severe_flags_below_85_percent_of_median() {
        // 中位數：avg=100,p1=90,p01=80,stdev=5
        let mut a = lp(100.0, 90.0, 80.0, 5.0);
        a.lp = 1;
        let mut bad = lp(80.0, 90.0, 80.0, 5.0); // avg 80 < 85
        bad.lp = 2;
        let mut stdev_bad = lp(100.0, 90.0, 80.0, 9.0); // stdev 9 > 1.5*5=7.5
        stdev_bad.lp = 3;
        let sev = severe_lps(&[a, bad, stdev_bad]);
        assert_eq!(sev, vec![2, 3]);
    }

    #[test]
    fn severe_disables_stdev_condition_when_median_zero() {
        // 全部 stdev = 0 → 中位數 0 → STDEV 條件停用
        let mut a = lp(100.0, 90.0, 80.0, 0.0);
        a.lp = 1;
        let mut b = lp(100.0, 90.0, 80.0, 0.0);
        b.lp = 2;
        let mut c = lp(60.0, 90.0, 80.0, 0.0); // avg 60 < 85 → 仍嚴重
        c.lp = 3;
        let sev = severe_lps(&[a, b, c]);
        assert_eq!(sev, vec![3]);
    }

    #[test]
    fn frametime_mad_pct_normalizes_by_median() {
        // frames [10, 12, 14]：median=12，MAD=2 → 2/12*100 ≈ 16.667%
        let m = frametime_mad_pct(&[10.0, 12.0, 14.0]).unwrap();
        assert!((m - 16.6667).abs() < 1e-3, "mad_pct={m}");
        // 完全一致 → MAD=0
        assert_eq!(frametime_mad_pct(&[10.0, 10.0, 10.0]).unwrap(), 0.0);
        assert_eq!(frametime_mad_pct(&[]), None);
    }

    #[test]
    fn spike_rate_counts_frames_over_2x_median() {
        // frames [10,10,10,30]：median=10，threshold=20 → 1/4 = 25%
        assert_eq!(spike_rate_pct(&[10.0, 10.0, 10.0, 30.0]).unwrap(), 25.0);
        // 零 spike
        assert_eq!(spike_rate_pct(&[10.0, 10.0, 10.0]).unwrap(), 0.0);
        assert_eq!(spike_rate_pct(&[]), None);
    }

    #[test]
    fn compute_lp_result_includes_robustness_metrics() {
        // 有 spike 的序列，mad/spike 皆應被填入
        let m = compute_lp_result(7, &[10.0, 10.0, 10.0, 30.0]).unwrap();
        assert!(m.frametime_mad_pct.is_some());
        assert_eq!(m.spike_rate_pct, Some(25.0));
    }

    #[test]
    fn normalized_ratio_guards_zero_and_non_finite() {
        // median ≤0 且 higher-is-better → 中性 1.0
        assert_eq!(normalized_ratio(100.0, 0.0, true), 1.0);
        // value 非有限 → 中性 1.0
        assert_eq!(normalized_ratio(f64::NAN, 50.0, true), 1.0);
        // 反比 value=0（完美）→ 對稱比值上限 2.0（不再 3x 獎勵）
        assert_eq!(normalized_ratio(0.0, 10.0, false), 2.0);
        // 反比正常：2*median/(median+value) = 60/50 = 1.2
        assert!((normalized_ratio(20.0, 30.0, false) - 1.2).abs() < 1e-12);
        // 高者愈好正常：value/median
        assert!((normalized_ratio(90.0, 80.0, true) - 1.125).abs() < 1e-12);
    }

    #[test]
    fn normalized_ratio_zero_median_distinguishes_zero_from_positive() {
        // median==0：value==0 → 中性 1.0，value>0 → 懲罰 0.0
        assert_eq!(normalized_ratio(0.0, 0.0, false), 1.0);
        assert_eq!(normalized_ratio(5.0, 0.0, false), 0.0);
        assert_eq!(normalized_ratio(100.0, 0.0, false), 0.0);
    }

    #[test]
    fn normalized_ratio_lower_is_better_monotonic() {
        // median=10，value 越大 ratio 越小（懲罰越重）
        let ratios: Vec<f64> = [0.0, 5.0, 10.0, 20.0, 100.0]
            .iter()
            .map(|&v| normalized_ratio(v, 10.0, false))
            .collect();
        for w in ratios.windows(2) {
            assert!(
                w[0] > w[1],
                "lower-is-better 應隨 value 單調遞減: {ratios:?}"
            );
        }
        // 端點：value=0 → 2、value=median → 1、value→∞ 趨近 0
        assert!((ratios[0] - 2.0).abs() < 1e-12);
        assert!((ratios[2] - 1.0).abs() < 1e-12);
        assert!(ratios[4] < 0.2);
    }

    #[test]
    fn normalized_ratio_non_finite_safe() {
        assert_eq!(normalized_ratio(f64::NAN, 10.0, false), 1.0);
        assert_eq!(normalized_ratio(10.0, f64::NAN, false), 1.0);
        assert_eq!(normalized_ratio(f64::INFINITY, 10.0, false), 1.0);
        assert_eq!(normalized_ratio(10.0, f64::INFINITY, true), 1.0);
        assert_eq!(normalized_ratio(f64::NEG_INFINITY, 10.0, true), 1.0);
    }

    fn lp_comp(lp: u32, avg: f64, p1: f64, p01: f64, mad: f64, spike: f64) -> LpResult {
        LpResult {
            lp,
            avg_fps: Some(avg),
            p1_low: Some(p1),
            p01_low: Some(p01),
            frametime_mad_pct: Some(mad),
            spike_rate_pct: Some(spike),
            completed: true,
            ..Default::default()
        }
    }

    #[test]
    fn competitive_score_favors_better_lp_across_all_axes() {
        let a = lp_comp(0, 100.0, 90.0, 80.0, 10.0, 10.0);
        let b = lp_comp(1, 80.0, 70.0, 60.0, 20.0, 20.0);
        let med = round_medians(&[a.clone(), b.clone()]);
        assert_eq!(med.avg, 90.0);
        assert_eq!(med.p1_low, 80.0);
        assert_eq!(med.p01_low, 70.0);
        assert_eq!(med.mad, 15.0);
        assert_eq!(med.spike, 15.0);
        let sa = competitive_score(&a, &med).unwrap();
        let sb = competitive_score(&b, &med).unwrap();
        assert!(sa > sb, "LP0 應在所有軸上都較佳");
        assert!((sa - 1.16004).abs() < 1e-3, "sa={sa}");
        assert!((sb - 0.86567).abs() < 1e-3, "sb={sb}");
    }

    #[test]
    fn zero_valued_lower_metric_cannot_dominate() {
        // LP0：MAD/spike 完美（0），但 FPS 極差；LP1：MAD/spike 略高於中位數，FPS 極佳。
        // 對稱比值上限 2.0 使完美 lower metric 無法蓋過 55% 的 FPS 權重 → LP1 勝。
        let a = lp_comp(0, 10.0, 10.0, 9.0, 0.0, 0.0);
        let b = lp_comp(1, 100.0, 100.0, 90.0, 5.0, 5.0);
        let med = round_medians(&[a.clone(), b.clone()]);
        let sa = competitive_score(&a, &med).unwrap();
        let sb = competitive_score(&b, &med).unwrap();
        assert!(sb > sa, "零 MAD/spike 不應主導分數：sa={sa}, sb={sb}");
    }

    #[test]
    fn competitive_score_ineligible_returns_none() {
        let a = lp_comp(0, 100.0, 90.0, 80.0, 10.0, 10.0);
        // 缺 spike → 不合格
        let mut bad = a.clone();
        bad.spike_rate_pct = None;
        let med = round_medians(&[a.clone(), bad.clone()]);
        assert_eq!(competitive_score(&bad, &med), None);
        assert!(competitive_score(&a, &med).is_some());
    }

    #[test]
    fn robust_candidates_median_then_worst_round_then_lower_lp() {
        // 中位數相同（1.0），LP5 worst=1.0 > LP2 worst=0.9 → LP5 勝
        let cands = robust_candidates(&[(5, vec![1.0, 1.0, 1.0]), (2, vec![1.1, 0.9, 1.0])]);
        assert_eq!(cands[0].lp, 5);
        assert_eq!(cands[1].lp, 2);
        assert!((cands[0].median_score - 1.0).abs() < 1e-12);
        // 完全平手 → 較小 LP 勝
        let tie = robust_candidates(&[(9, vec![1.0]), (3, vec![1.0])]);
        assert_eq!(tie[0].lp, 3);
        assert_eq!(tie[1].lp, 9);
        // 中位數較高者優先
        let hi = robust_candidates(&[(0, vec![1.0]), (1, vec![1.5, 1.5])]);
        assert_eq!(hi[0].lp, 1);
    }

    #[test]
    fn robust_candidates_skips_empty_scores() {
        let cands = robust_candidates(&[(0, vec![]), (1, vec![1.0])]);
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].lp, 1);
    }

    // ── 確認分數（有界 log-ratio）──

    #[test]
    fn confirmation_log_ratio_higher_and_lower_better() {
        // higher-is-better：candidate 快 5% → 100*ln(1.05) ≈ 4.879（clamp 內）
        let hi = confirmation_log_ratio(105.0, 100.0, true).unwrap();
        assert!((hi - 4.8790).abs() < 1e-3, "hi={hi}");
        // lower-is-better：candidate 較低較好（97 vs 100）→ 100*ln(100/97) ≈ 3.046
        let lo = confirmation_log_ratio(97.0, 100.0, false).unwrap();
        assert!((lo - 3.0460).abs() < 1e-3, "lo={lo}");
        // 完全相等 → 0
        assert_eq!(confirmation_log_ratio(50.0, 50.0, true), Some(0.0));
    }

    #[test]
    fn confirmation_log_ratio_clamps_to_five() {
        // candidate 極快（1000x）→ 100*ln(1000) ≈ 690 → clamp +5
        assert_eq!(confirmation_log_ratio(1000.0, 1.0, true), Some(5.0));
        // candidate 極慢 → clamp -5
        assert_eq!(confirmation_log_ratio(1.0, 1000.0, true), Some(-5.0));
        // lower-is-better 反向亦然
        assert_eq!(confirmation_log_ratio(1000.0, 1.0, false), Some(-5.0));
    }

    #[test]
    fn confirmation_log_ratio_fails_closed_on_invalid() {
        // 非有限或 ≤0 一律 None（不得給中性分）
        assert_eq!(confirmation_log_ratio(f64::NAN, 10.0, true), None);
        assert_eq!(confirmation_log_ratio(10.0, f64::INFINITY, true), None);
        assert_eq!(confirmation_log_ratio(0.0, 10.0, true), None);
        assert_eq!(confirmation_log_ratio(10.0, 0.0, false), None);
        assert_eq!(confirmation_log_ratio(-1.0, 10.0, true), None);
    }

    #[test]
    fn confirmation_effect_spike_does_not_affect_score() {
        let a = lp_comp(0, 100.0, 90.0, 80.0, 10.0, 0.0);
        let b = lp_comp(1, 90.0, 80.0, 70.0, 12.0, 0.0);
        let base = confirmation_effect(&a, &b).unwrap();
        // spike 只當 guardrail，不進主分數：改動 spike 不影響分數
        let mut a_spiky = a.clone();
        a_spiky.spike_rate_pct = Some(80.0);
        let mut b_spiky = b.clone();
        b_spiky.spike_rate_pct = Some(0.1);
        let spiky = confirmation_effect(&a_spiky, &b_spiky).unwrap();
        assert_eq!(base, spiky, "spike 變化不得影響確認主分數");
    }

    #[test]
    fn confirmation_effect_fails_closed_on_missing_or_zero_mad() {
        let a = lp_comp(0, 100.0, 90.0, 80.0, 10.0, 0.0);
        let b = lp_comp(1, 90.0, 80.0, 70.0, 12.0, 0.0);
        // MAD = 0（完美穩定）→ fail closed（log-ratio 無法定義）
        let mut z = a.clone();
        z.frametime_mad_pct = Some(0.0);
        assert_eq!(confirmation_effect(&z, &b), None);
        // 缺 MAD → None
        let mut miss = a.clone();
        miss.frametime_mad_pct = None;
        assert_eq!(confirmation_effect(&miss, &b), None);
        assert!(confirmation_effect(&a, &b).is_some());
    }

    #[test]
    fn confirmation_effect_is_symmetric_in_direction() {
        // candidate 全面較佳 → 正向為正、反向為負
        let a = lp_comp(0, 100.0, 90.0, 80.0, 10.0, 0.0);
        let b = lp_comp(1, 90.0, 80.0, 70.0, 12.0, 0.0);
        let fwd = confirmation_effect(&a, &b).unwrap();
        let rev = confirmation_effect(&b, &a).unwrap();
        assert!(fwd > 0.0, "fwd={fwd}");
        assert!(rev < 0.0, "rev={rev}");
        assert!(
            (fwd + rev).abs() < 1e-9,
            "反向應完全對稱: fwd={fwd} rev={rev}"
        );
    }

    // ── 顯示端序列（present→display 延遲 / display-change 間隔 / 丟幀）──

    /// PresentMon 2.5.1 `--v1_metrics` 實際表頭（含 BOM，取自真實 session CSV）
    const V1_HEADER: &str = "\
\u{feff}Application,ProcessID,SwapChainAddress,Runtime,SyncInterval,PresentFlags,Dropped,TimeInSeconds,msInPresentAPI,msBetweenPresents,AllowsTearing,PresentMode,msUntilRenderComplete,msUntilDisplayed,msBetweenDisplayChange,msFlipDelay";

    #[test]
    fn series_parses_display_columns_from_v1_header() {
        let csv = format!(
            "{V1_HEADER}\n\"g (1)\",1,0x0,D3D11,1,0,1,0.001,0.1,16.0,0,\"Composed\",5.0,8.0,16.6,0.0\n\
             \"g (1)\",1,0x0,D3D11,1,0,0,0.017,0.1,16.2,0,\"Composed\",5.0,NA,16.4,0.0\n"
        );
        let s = parse_presentmon_series(&csv).unwrap();
        assert_eq!(s.frames, vec![16.0, 16.2]);
        assert_eq!(s.display.presents, 2);
        // 間隔欄位有效就收（NA 只影響同列的延遲欄位）
        assert_eq!(s.display.intervals, vec![16.6, 16.4]);
        assert_eq!(s.display.latencies, vec![8.0]); // NA 延遲剔除
        assert!(s.display.dropped_seen);
        assert_eq!(s.display.dropped_count, 1);
        // 時間欄位照常解析
        assert_eq!(s.capture.first_time_secs, Some(0.001));
        assert_eq!(s.capture.last_time_secs, Some(0.017));
        assert!(s.capture.monotonic);
    }

    #[test]
    fn series_without_display_columns_still_parses_frames() {
        let csv = "Application,msBetweenPresents\ngame.exe,16.7\ngame.exe,8.3\n";
        let s = parse_presentmon_series(csv).unwrap();
        assert_eq!(s.frames, vec![16.7, 8.3]);
        assert!(s.display.intervals.is_empty());
        assert!(s.display.latencies.is_empty());
        assert!(!s.display.dropped_seen);
        assert_eq!(s.display.dropped_count, 0);
        assert_eq!(s.display.presents, 2);
    }

    #[test]
    fn series_errors_match_legacy_semantics() {
        // 缺主欄 → Err；無有效 frametime → Err（與 parse_presentmon_csv 相同）
        assert!(parse_presentmon_series("Application,ProcessID\n1,2\n").is_err());
        assert!(parse_presentmon_series("Application,msBetweenPresents\n").is_err());
        // 非單調時間仍偵測（不 Err，與 CsvCapture.monotonic 語意一致）
        let csv = format!(
            "{V1_HEADER}\n\"g\",1,0,1,0,1,0,0.02,0,16.0,0,\"C\",0,NA,0,0\n\
             \"g\",1,0,1,0,1,0,0.01,0,16.0,0,\"C\",0,NA,0,0\n"
        );
        let s = parse_presentmon_series(&csv).unwrap();
        assert!(!s.capture.monotonic);
    }

    #[test]
    fn display_metrics_math_and_none_guards() {
        // 間隔 [10, 10, 10, 30] ms → 顯示端 avg = 1000/15 ≈ 66.67、
        // instant FPS = [100,100,100,33.33] → 1% low = 單一最慢 = 33.33
        let series = DisplaySeries {
            intervals: vec![10.0, 10.0, 10.0, 30.0],
            latencies: vec![2.0, 4.0, 6.0, 8.0, 100.0],
            dropped_count: 1,
            dropped_seen: true,
            presents: 4,
        };
        let m = compute_display_metrics(&series);
        let expected_avg = 1000.0 / 15.0;
        assert!((m.displayed_avg_fps.unwrap() - expected_avg).abs() < 1e-9);
        assert!((m.displayed_p1_low.unwrap() - 100.0 / 3.0).abs() < 1e-9);
        assert!((m.display_latency_avg_ms.unwrap() - 24.0).abs() < 1e-9);
        // p99 採 floor((n-1)q) 索引（5 樣本 → sorted[3] = 8.0，與既有 percentile 慣例一致）
        assert!((m.display_latency_p99_ms.unwrap() - 8.0).abs() < 1e-9);
        assert!((m.dropped_pct.unwrap() - 25.0).abs() < 1e-9);
        // 無 dropped 欄 → None；無樣本 → 全 None
        let bare = DisplaySeries {
            dropped_seen: false,
            ..Default::default()
        };
        let m2 = compute_display_metrics(&bare);
        assert_eq!(m2.displayed_avg_fps, None);
        assert_eq!(m2.dropped_pct, None);
    }

    #[test]
    fn merge_display_rounds_sums_counts_and_flags() {
        let a = DisplaySeries {
            intervals: vec![10.0],
            latencies: vec![1.0],
            dropped_count: 1,
            dropped_seen: true,
            presents: 2,
        };
        let b = DisplaySeries {
            intervals: vec![20.0, 30.0],
            latencies: vec![],
            dropped_count: 0,
            dropped_seen: false,
            presents: 3,
        };
        let m = merge_display_rounds(&[a, b]);
        assert_eq!(m.intervals, vec![10.0, 20.0, 30.0]);
        assert_eq!(m.latencies, vec![1.0]);
        assert_eq!(m.dropped_count, 1);
        assert!(m.dropped_seen);
        assert_eq!(m.presents, 5);
    }

    #[test]
    fn attach_display_metrics_fills_lp_result() {
        let mut r = compute_lp_result(1, &[16.0]).unwrap();
        assert_eq!(r.displayed_avg_fps, None);
        let series = DisplaySeries {
            intervals: vec![8.0],
            latencies: vec![3.0],
            dropped_count: 0,
            dropped_seen: true,
            presents: 1,
        };
        attach_display_metrics(&mut r, &series);
        assert_eq!(r.displayed_avg_fps, Some(125.0));
        assert_eq!(r.display_latency_avg_ms, Some(3.0));
        assert_eq!(r.dropped_pct, Some(0.0));
    }
}
