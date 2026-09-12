//! 資料模型（PLAN §5.2）。JSON 序列化：struct 欄位 camelCase、enum variant PascalCase。

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    pub version: u32,
    #[serde(default)]
    pub settings: Settings,
    #[serde(default)]
    pub rules: Vec<Rule>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: 1,
            settings: Settings::default(),
            rules: Vec::new(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub enum Theme {
    #[default]
    Dark,
    Light,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    #[serde(default = "default_language")]
    pub language: String,
    #[serde(default)]
    pub start_with_windows: bool,
    #[serde(default)]
    pub start_minimized: bool,
    #[serde(default = "default_true")]
    pub close_to_tray: bool,
    #[serde(default = "default_poll_interval")]
    pub poll_interval_ms: u64,
    #[serde(default)]
    pub show_advanced_priorities: bool,
    /// 高精度計時器：常駐請求 0.5 ms timer resolution（per-process 語意，見 timer.rs）
    #[serde(default)]
    pub high_precision_timer: bool,
    /// 高精度計時器：遊戲節流豁免名單（小寫 exe 檔名；執行中同名行程自動套用）
    #[serde(default)]
    pub timer_exempt_programs: Vec<String>,
    #[serde(default = "default_theme")]
    pub theme: Theme,
}

fn default_language() -> String {
    "zh-TW".to_string()
}
fn default_theme() -> Theme {
    Theme::Dark
}
fn default_true() -> bool {
    true
}
fn default_poll_interval() -> u64 {
    1000
}

/// 高精度計時器狀態（get_timer_status 回傳）
#[derive(Serialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct TimerStatus {
    pub enabled: bool,
    /// 實際生效解析（ms）；查詢失敗為 None
    pub current_resolution_ms: Option<f64>,
    /// 最細支援解析（ms，Clockres 的 "Minimum timer interval"，通常 0.5）
    #[serde(default)]
    pub min_interval_ms: Option<f64>,
    /// 最粗支援解析（ms，Clockres 的 "Maximum timer interval"，通常 15.625）
    #[serde(default)]
    pub max_interval_ms: Option<f64>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            language: default_language(),
            start_with_windows: false,
            // 預設不最小化啟動：主視窗以最大化顯示（tauri.conf.json maximized）
            start_minimized: false,
            close_to_tray: true,
            poll_interval_ms: default_poll_interval(),
            show_advanced_priorities: false,
            high_precision_timer: false,
            timer_exempt_programs: Vec::new(),
            theme: default_theme(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Rule {
    pub id: String,
    pub name: String,
    pub exe_path: String,
    #[serde(default)]
    pub match_by: MatchBy,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub affinity: AffinitySpec,
    #[serde(default)]
    pub priority: CpuPriority,
    #[serde(default)]
    pub advanced: AdvancedSpec,
    /// GPU 基準測試推薦的套用元資料（可選；舊 config 無此欄可正常載入）
    #[serde(default)]
    pub recommendation: Recommendation,
}

impl Rule {
    pub fn new(exe_path: String, name: String) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            name,
            exe_path,
            match_by: MatchBy::FullPath,
            enabled: true,
            affinity: AffinitySpec::default(),
            priority: CpuPriority::High,
            advanced: AdvancedSpec::default(),
            recommendation: Recommendation::default(),
        }
    }
}

/// 基準測試推薦元資料：綁定在 Rule 上的「這個規則為何這樣設」的證據。
/// 全部欄位 serde default → 舊 config 載入不受影響，roundtrip 保留新欄位。
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct Recommendation {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub generated_at: Option<String>,
    #[serde(default)]
    pub cpu_fingerprint: Option<String>,
    #[serde(default)]
    pub gpu_instance_id: Option<String>,
    #[serde(default)]
    pub best_lp: Option<u32>,
    #[serde(default)]
    pub severe_lps: Vec<u32>,
    #[serde(default)]
    pub recommended_cores: Vec<u32>,
    #[serde(default)]
    pub adjusted: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub enum MatchBy {
    #[default]
    FullPath,
    FileName,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct AffinitySpec {
    #[serde(default)]
    pub mode: AffinityMode,
    #[serde(default)]
    pub cores: Vec<u32>,
}

impl Default for AffinitySpec {
    fn default() -> Self {
        Self {
            mode: AffinityMode::All,
            cores: Vec::new(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub enum AffinityMode {
    #[default]
    All,
    NoSmtSibling,
    PCoresOnly,
    Custom,
    /// 軟綁定：僅設定執行緒 ideal processor（偏好核心），不硬排除其他核心
    Prefer,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Default)]
pub enum CpuPriority {
    Idle,
    BelowNormal,
    #[default]
    Normal,
    AboveNormal,
    High,
}

impl CpuPriority {
    /// 顯示用字串（面板「實際狀態」欄）
    pub fn as_str(&self) -> &'static str {
        match self {
            CpuPriority::Idle => "Idle",
            CpuPriority::BelowNormal => "BelowNormal",
            CpuPriority::Normal => "Normal",
            CpuPriority::AboveNormal => "AboveNormal",
            CpuPriority::High => "High",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct AdvancedSpec {
    #[serde(default)]
    pub io_priority: Option<IoPriority>,
    #[serde(default)]
    pub memory_priority: Option<MemPriority>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub enum IoPriority {
    VeryLow,
    Low,
    Normal,
    High,
}

impl IoPriority {
    pub fn to_raw(self) -> u32 {
        match self {
            IoPriority::VeryLow => 0,
            IoPriority::Low => 1,
            IoPriority::Normal => 2,
            IoPriority::High => 3,
        }
    }

    pub fn from_raw(v: u32) -> Self {
        match v {
            0 => IoPriority::VeryLow,
            1 => IoPriority::Low,
            3 => IoPriority::High,
            _ => IoPriority::Normal,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub enum MemPriority {
    VeryLow,
    Low,
    Medium,
    BelowNormal,
    Normal,
}

impl MemPriority {
    pub fn to_raw(self) -> u32 {
        match self {
            MemPriority::VeryLow => 1,
            MemPriority::Low => 2,
            MemPriority::Medium => 3,
            MemPriority::BelowNormal => 4,
            MemPriority::Normal => 5,
        }
    }

    pub fn from_raw(v: u32) -> Self {
        match v {
            1 => MemPriority::VeryLow,
            2 => MemPriority::Low,
            3 => MemPriority::Medium,
            4 => MemPriority::BelowNormal,
            _ => MemPriority::Normal,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 舊 config 完全沒有 recommendation 欄位 → 正常載入，metadata 用預設值
    #[test]
    fn old_config_without_recommendation_loads() {
        let json = r#"{
            "version": 1,
            "rules": [
                {
                    "id": "r1",
                    "name": "Game",
                    "exePath": "C:\\Games\\game.exe",
                    "matchBy": "FullPath",
                    "enabled": true,
                    "affinity": { "mode": "All", "cores": [] },
                    "priority": "High",
                    "advanced": {}
                }
            ]
        }"#;
        let cfg: Config = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.rules.len(), 1);
        let rec = &cfg.rules[0].recommendation;
        assert!(rec.session_id.is_none());
        assert!(rec.severe_lps.is_empty());
        assert!(rec.recommended_cores.is_empty());
        assert!(!rec.adjusted);
    }

    /// roundtrip：填入推薦元資料後序列化/反序列化，欄位原樣保留
    #[test]
    fn recommendation_roundtrip_preserved() {
        let mut cfg = Config::default();
        let mut rule = Rule::new(r"C:\Games\game.exe".into(), "Game".into());
        rule.recommendation = Recommendation {
            session_id: Some("sess-1".into()),
            generated_at: Some("2026-08-11T00:00:00Z".into()),
            cpu_fingerprint: Some("fp-abc".into()),
            gpu_instance_id: Some(r"PCI\VEN_10DE&DEV_2684".into()),
            best_lp: Some(5),
            severe_lps: vec![3, 4],
            recommended_cores: vec![5, 6, 7],
            adjusted: true,
        };
        cfg.rules.push(rule);

        let json = serde_json::to_string(&cfg).unwrap();
        let back: Config = serde_json::from_str(&json).unwrap();
        let rec = &back.rules[0].recommendation;
        assert_eq!(rec.session_id.as_deref(), Some("sess-1"));
        assert_eq!(rec.best_lp, Some(5));
        assert_eq!(rec.severe_lps, vec![3, 4]);
        assert_eq!(rec.recommended_cores, vec![5, 6, 7]);
        assert!(rec.adjusted);
        assert_eq!(rec.cpu_fingerprint.as_deref(), Some("fp-abc"));
    }

    /// camelCase 序列化：Rule 序列化後欄位名為 camelCase、enum 為 PascalCase
    #[test]
    fn rule_serializes_camel_case() {
        let rule = Rule::new(r"C:\Games\game.exe".into(), "Game".into());
        let json = serde_json::to_string(&rule).unwrap();
        assert!(json.contains("\"exePath\""));
        assert!(json.contains("\"recommendation\""));
        assert!(json.contains("\"FullPath\""));
        assert!(json.contains("\"High\""));
    }

    /// 舊 config 沒有 timerExemptPrograms 欄位 → 空 vec;填入後 roundtrip 保留
    #[test]
    fn timer_exempt_programs_roundtrip() {
        let json = r#"{ "version": 1, "settings": {} }"#;
        let cfg: Config = serde_json::from_str(json).unwrap();
        assert!(cfg.settings.timer_exempt_programs.is_empty());

        let mut cfg = Config::default();
        cfg.settings.timer_exempt_programs = vec!["game.exe".into()];
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(json.contains("\"timerExemptPrograms\""));
        let back: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(back.settings.timer_exempt_programs, vec!["game.exe"]);
    }

    /// 舊 config 沒有 theme 欄位 → 預設為 Dark
    #[test]
    fn old_config_without_theme_defaults_to_dark() {
        let json = r#"{
            "version": 1,
            "settings": {
                "language": "en",
                "startWithWindows": false,
                "startMinimized": true,
                "closeToTray": true,
                "pollIntervalMs": 1000,
                "showAdvancedPriorities": false
            }
        }"#;
        let cfg: Config = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.settings.theme, Theme::Dark);
    }

    /// theme roundtrip：Dark ↔ Light 序列化/反序列化保留
    #[test]
    fn theme_roundtrip() {
        let mut cfg = Config::default();
        cfg.settings.theme = Theme::Light;
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(json.contains("\"theme\""));
        assert!(json.contains("\"Light\""));
        let back: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(back.settings.theme, Theme::Light);
    }
}
