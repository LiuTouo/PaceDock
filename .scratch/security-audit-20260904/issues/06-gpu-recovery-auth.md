# 06 — 可偽造 recovery/restore/session JSON 驅動 elevated HKLM GPU mutation

- 來源 findings:#16、#24、#26、#30、#32(CWE-15/345,severity medium)
- 位置:`benchmark/recovery.rs`、`benchmark/manager.rs`(restore/journal/apply_best)、`benchmark/storage.rs`、`gpu.rs`
- **狀態:done 2026-09-04**

## 實作(四層防護)

1. **HMAC 認證(核心)** — 新模組 `state_auth.rs`:HMAC-SHA256 認證特權狀態檔
   (recovery journal、restore record、session.json)。key(32 bytes,兩組 UUID v4)
   存於 `%PROGRAMDATA%\PaceDock\state.key`,目錄以 `D:P(A;;FA;;;BA)(A;;FA;;;SY)`
   保護(同帳戶 medium-integrity 程序不可讀,無法重算 MAC)。寫入以暫存 + 原子
   rename,平行建立競態安全;MAC 旁檔缺失/不符一律 Err(fail closed)。
   - `recovery::save_at/load_from`、`write_restore_record/load_restore_record`、
     `storage::save_session_at` 全面走 `auth_write/auth_read`。
   - `apply_best_affinity` 改用 `storage::get_at_verified`(UI 顯示用的 `get_at`
     維持寬鬆,只讀不特權)。
2. **語意驗證(任意 bytes 進不了 HKLM)** — `gpu::validate_policy_snapshot`:
   DevicePolicy 存在時必為 REG_DWORD(4 bytes)、AssignmentSetOverride 必為
   REG_BINARY ≤ 8 bytes、instance_id 非空且有長度上限。
   `RealGpuBackend::write_affinity_policy` 開頭強制呼叫 — 所有寫入路徑
   (apply/restore/recovery)單點覆蓋。
3. **write 前 present-adapter 檢查** — `gpu::restore_snapshot` 在任何 registry
   write 前驗證 target 為目前 present 的 display adapter;未知 adapter →
   `GPU_NOT_FOUND`,不再「先寫後失敗」。
4. **大小上限** — `state_auth::MAX_AUTHENTICATED_SIZE`(1 MiB)於讀取前檢查。

## 行為變化

- 未認證的舊 journal / restore record / session.json:載入即 Err → startup
  recovery 呈 RecoveryRequired(封鎖新測試)、舊 session 無法 apply。屬刻意
  fail closed;攻擊者可藉此造成 DoS 但無法驅動任何特權寫入。
- 非 DWORD 的 DevicePolicy 快照不再「無損還原」(原測試
  `restore_preserves_arbitrary_non_dword_types` 改為驗證拒絕)— 真實驅動語意
  該 slot 即 DWORD,任意型別正是本 finding 要關的門。

## 測試

- `state_auth`:roundtrip、竄改拒絕、缺旁檔拒絕。
- recovery/restore record:未認證直接寫入 → Err。
- `apply_best`:未認證 session 拒絕(`get_at_verified`)。
- restore:非 DWORD 快照在 write 前被擋(裝置狀態不變)。
- 全套 443 tests 通過。
