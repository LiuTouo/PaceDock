//! CPU 拓撲列舉（PLAN §7.1）：GetLogicalProcessorInformationEx(RelationProcessorCore)
//! 取得實體核心 → 邏輯處理器映射、SMT sibling、EfficiencyClass（P/E core）。

use serde::Serialize;
use windows::Win32::System::SystemInformation::{
    GetLogicalProcessorInformationEx, RelationProcessorCore, PROCESSOR_RELATIONSHIP,
    SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
};

/// LTP_PC_SMT：PROCESSOR_RELATIONSHIP.Flags 的 SMT 旗標（windows crate 未匯出此常數）
const LTP_PC_SMT: u8 = 0x1;

use crate::error::TopologyError;
#[cfg(test)]
use crate::model::{AffinityMode, AffinitySpec};

/// CPU 拓撲（PLAN §5.3）
#[derive(Serialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct Topology {
    pub logical_processors: Vec<LogicalProcessor>, // 依 LP index 排序
    pub physical_cores: Vec<PhysicalCore>,         // 依 core id 排序
    pub has_smt: bool,                             // 有任何核心 >1 LP
    pub has_hybrid: bool,                          // EfficiencyClass 不全相同
    pub total_lp: u32,
    /// 偵測到的處理器群組數。>1 = 多群組系統，拓撲僅含 group 0（PLAN §15.1）。
    pub processor_groups: u32,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct LogicalProcessor {
    pub index: u32,           // LP index（group 0 內）
    pub core_id: u32,         // 所屬實體核心 id
    pub is_smt_sibling: bool, // true = 此核心第二條 HT 執行緒（UI 標「HT」）
    pub efficiency_class: u8, // 0 = E-core；較大 = P-core（均質 CPU 全相同）
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PhysicalCore {
    pub id: u32,
    pub lp_indices: Vec<u32>, // 1 個 = 無 SMT；2 個 = 有 HT
    pub efficiency_class: u8,
    pub is_p_core: bool, // efficiency_class == 全系統最大值
}

/// 列舉 CPU 拓撲。失敗回傳 Err；呼叫端決定 fallback。
pub fn enumerate_topology() -> Result<Topology, TopologyError> {
    // 1) 取得所需 buffer 長度（預期回傳失敗，needed 被填上）
    let mut needed: u32 = 0;
    let _ = unsafe { GetLogicalProcessorInformationEx(RelationProcessorCore, None, &mut needed) };
    if needed == 0 {
        return Err(TopologyError::QueryFailed);
    }

    // 2) 配置 buffer 正式取資料
    let mut buf = vec![0u8; needed as usize];
    unsafe {
        GetLogicalProcessorInformationEx(
            RelationProcessorCore,
            Some(buf.as_mut_ptr() as *mut SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX),
            &mut needed,
        )
        .map_err(|_| TopologyError::QueryFailed)?;
    }

    // 3) 走訪可變長結構鏈
    let mut raw_cores: Vec<(Vec<u32>, u8, bool)> = Vec::new(); // (lp_indices, efficiency, is_smt)
                                                               // 偵測到的群組編號。僅 group 0 進拓撲：group 1 的「群組內 LP index」與 group 0
                                                               // 重疊，混入會讓 mask 靜默指向錯誤核心（PLAN §15.1：多群組 → 警告，不支援）。
    let mut groups: std::collections::BTreeSet<u16> = std::collections::BTreeSet::new();
    let mut offset = 0usize;
    while offset < needed as usize {
        let header = unsafe {
            &*(buf.as_ptr().add(offset) as *const SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX)
        };
        if header.Relationship == RelationProcessorCore {
            let proc_rel: &PROCESSOR_RELATIONSHIP = unsafe { &header.Anonymous.Processor };
            let masks_offset = proc_rel.GroupMask.as_ptr() as usize - header as *const _ as usize;
            let masks_bytes =
                proc_rel.GroupCount as usize * std::mem::size_of_val(&proc_rel.GroupMask[0]);
            if masks_offset + masks_bytes > header.Size as usize
                || offset + header.Size as usize > needed as usize
            {
                return Err(TopologyError::QueryFailed);
            }
            let masks = unsafe {
                std::slice::from_raw_parts(
                    proc_rel.GroupMask.as_ptr(),
                    proc_rel.GroupCount as usize,
                )
            };
            for ga in masks {
                groups.insert(ga.Group);
                if ga.Group == 0 {
                    let lp_indices = mask_to_indices(ga.Mask as u64);
                    if !lp_indices.is_empty() {
                        raw_cores.push((
                            lp_indices,
                            proc_rel.EfficiencyClass,
                            proc_rel.Flags == LTP_PC_SMT,
                        ));
                    }
                }
            }
        }
        if header.Size == 0 {
            break; // 防呆：避免無限迴圈
        }
        offset += header.Size as usize;
    }

    let mut topo = build_topology(raw_cores);
    topo.processor_groups = groups.len() as u32;
    Ok(topo)
}

/// mask 的 set bits → LP indices
pub fn mask_to_indices(mask: u64) -> Vec<u32> {
    (0..64).filter(|i| mask & (1u64 << i) != 0).collect()
}

/// 由核心清單組裝 Topology（獨立函式方便測試）
pub fn build_topology(mut raw_cores: Vec<(Vec<u32>, u8, bool)>) -> Topology {
    raw_cores.sort_by_key(|(lps, _, _)| lps[0]);

    let max_eff = raw_cores.iter().map(|(_, e, _)| *e).max().unwrap_or(0);
    let has_hybrid = raw_cores.iter().any(|(_, e, _)| *e != max_eff)
        && raw_cores.iter().any(|(_, e, _)| *e == max_eff)
        && !raw_cores.is_empty()
        && raw_cores
            .iter()
            .map(|(_, e, _)| e)
            .collect::<std::collections::HashSet<_>>()
            .len()
            > 1;
    let has_smt = raw_cores.iter().any(|(_, _, smt)| *smt);

    let mut physical_cores = Vec::new();
    let mut logical_processors = Vec::new();

    for (core_id, (lp_indices, eff, _smt)) in raw_cores.iter().enumerate() {
        let is_p_core = *eff == max_eff;
        physical_cores.push(PhysicalCore {
            id: core_id as u32,
            lp_indices: lp_indices.clone(),
            efficiency_class: *eff,
            is_p_core,
        });
        for (i, &lp) in lp_indices.iter().enumerate() {
            logical_processors.push(LogicalProcessor {
                index: lp,
                core_id: core_id as u32,
                // 同一實體核心內，mask 最低位元 LP 視為實體執行緒，其餘為 HT 虛擬核心
                is_smt_sibling: i > 0,
                efficiency_class: *eff,
            });
        }
    }

    let total_lp = logical_processors.len() as u32;
    Topology {
        logical_processors,
        physical_cores,
        has_smt,
        has_hybrid,
        total_lp,
        processor_groups: 1,
    }
}

/// affinity 模式 → mask（PLAN §7.1）。解析結果為 0 時 fallback 全部核心。
#[cfg(test)]
pub fn resolve_mask(spec: &AffinitySpec, topo: &Topology) -> u64 {
    let all = if topo.total_lp >= 64 {
        u64::MAX
    } else {
        (1u64 << topo.total_lp) - 1
    };
    let mask = match spec.mode {
        AffinityMode::All => all,
        AffinityMode::NoSmtSibling => topo
            .logical_processors
            .iter()
            .filter(|lp| !lp.is_smt_sibling)
            .fold(0u64, |m, lp| m | (1u64 << lp.index)),
        AffinityMode::PCoresOnly => topo
            .logical_processors
            .iter()
            .filter(|lp| {
                topo.physical_cores
                    .get(lp.core_id as usize)
                    .map(|c| c.is_p_core)
                    .unwrap_or(true)
            })
            .fold(0u64, |m, lp| m | (1u64 << lp.index)),
        AffinityMode::Custom => spec
            .cores
            .iter()
            .filter(|&&i| i < 64)
            .fold(0u64, |m, &i| m | (1u64 << i)),
        // 軟綁定不走 mask（執行緒層級 ideal processor，watcher 分開處理）
        AffinityMode::Prefer => 0,
    };
    // 防呆：0 mask 會讓 SetProcessAffinityMask 失敗
    let mask = if mask == 0 { all } else { mask };
    // 只保留系統實際存在的 LP（custom cores 可能來自過期拓撲）
    mask & all
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 假拓撲：無 SMT 均質 8C
    fn topo_8c() -> Topology {
        build_topology((0..8).map(|c| (vec![c], 0, false)).collect())
    }

    /// 假拓撲：SMT 8C16T（每核心 2 LP）
    fn topo_8c16t() -> Topology {
        build_topology(
            (0..8u32)
                .map(|c| (vec![c * 2, c * 2 + 1], 0, true))
                .collect(),
        )
    }

    /// 假拓撲：混合 8P(有HT)+8E = 24 LP（模擬 i9 類型）
    fn topo_hybrid() -> Topology {
        let mut cores: Vec<(Vec<u32>, u8, bool)> = Vec::new();
        let mut lp = 0u32;
        for _ in 0..8 {
            cores.push((vec![lp, lp + 1], 1, true)); // P-core, HT
            lp += 2;
        }
        for _ in 0..8 {
            cores.push((vec![lp], 0, false)); // E-core
            lp += 1;
        }
        build_topology(cores)
    }

    #[test]
    fn all_mode_covers_everything() {
        let t = topo_8c16t();
        let spec = AffinitySpec {
            mode: AffinityMode::All,
            cores: vec![],
        };
        assert_eq!(resolve_mask(&spec, &t), 0xFFFF);
    }

    #[test]
    fn no_smt_sibling_picks_first_thread_per_core() {
        let t = topo_8c16t();
        let spec = AffinitySpec {
            mode: AffinityMode::NoSmtSibling,
            cores: vec![],
        };
        // 每核心第一條：LP 0,2,4,...,14
        assert_eq!(resolve_mask(&spec, &t), 0b0101_0101_0101_0101);
    }

    #[test]
    fn no_smt_on_uniform_cpu_equals_all() {
        let t = topo_8c();
        assert!(!t.has_smt);
        let spec = AffinitySpec {
            mode: AffinityMode::NoSmtSibling,
            cores: vec![],
        };
        assert_eq!(resolve_mask(&spec, &t), 0xFF);
    }

    #[test]
    fn p_cores_only_on_hybrid() {
        let t = topo_hybrid();
        assert!(t.has_hybrid);
        assert_eq!(t.total_lp, 24);
        let spec = AffinitySpec {
            mode: AffinityMode::PCoresOnly,
            cores: vec![],
        };
        // P-core LP 0..16
        assert_eq!(resolve_mask(&spec, &t), 0xFFFF);
    }

    #[test]
    fn hybrid_marks_smt_siblings() {
        let t = topo_hybrid();
        let lp0 = &t.logical_processors[0];
        let lp1 = &t.logical_processors[1];
        assert!(!lp0.is_smt_sibling);
        assert!(lp1.is_smt_sibling);
        // E-core 無 sibling
        let e_core = t.physical_cores.iter().find(|c| !c.is_p_core).unwrap();
        assert_eq!(e_core.lp_indices.len(), 1);
    }

    #[test]
    fn p_cores_only_on_uniform_equals_all() {
        let t = topo_8c();
        assert!(!t.has_hybrid);
        let spec = AffinitySpec {
            mode: AffinityMode::PCoresOnly,
            cores: vec![],
        };
        assert_eq!(resolve_mask(&spec, &t), 0xFF);
    }

    #[test]
    fn custom_uses_given_cores() {
        let t = topo_8c16t();
        let spec = AffinitySpec {
            mode: AffinityMode::Custom,
            cores: vec![0, 2, 4, 6],
        };
        assert_eq!(resolve_mask(&spec, &t), 0b0101_0101);
    }

    #[test]
    fn empty_custom_falls_back_to_all() {
        let t = topo_8c();
        let spec = AffinitySpec {
            mode: AffinityMode::Custom,
            cores: vec![],
        };
        assert_eq!(resolve_mask(&spec, &t), 0xFF);
    }

    #[test]
    fn custom_cores_clamped_to_topology() {
        let t = topo_8c();
        let spec = AffinitySpec {
            mode: AffinityMode::Custom,
            cores: vec![0, 1, 63],
        };
        assert_eq!(resolve_mask(&spec, &t), 0b11);
    }

    #[test]
    fn mask_to_indices_works() {
        assert_eq!(mask_to_indices(0b1011), vec![0, 1, 3]);
        assert_eq!(mask_to_indices(0), Vec::<u32>::new());
    }
}
