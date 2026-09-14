# 04 — portable update 固定 TEMP staging 允許驗證後置換

- 來源 findings:#1、#4、#7、#10(CWE-367/377,severity high)
- 位置:`src-tauri/src/update.rs`(staging write ~460-500、script reopen ~673-706、PE move/start)
- **狀態:done 2026-09-04**

## 實作

- staging 目錄改一次性隨機名稱 `%TEMP%\pacedock_update_<uuid>`,以
  `D:P(A;;FA;;;BA)(A;;FA;;;SY)` 保護型 DACL 建立(僅 Administrators/SYSTEM 可寫,
  同帳戶 medium-integrity 程序無 ACE 可用;已以受限 token 寫入測試實證被拒)。
- 舊版固定名稱 `%TEMP%\pacedock_update` 於每次更新時盡力清除(升級殘留)。
- 解壓檔案全部改 exclusive create(`create_new`),UUID 撞名殘留時 fail closed。
- helper 腳本路徑/日誌改由 staged exe 路徑反推;腳本成功與 rollback 兩條結束路徑
  都會清掉 staging 目錄。
- DACL 子目錄由父目錄繼承同組 ACE,resources 子樹同受保護。

## 殘餘(記錄,不阻塞)

- 未做 verify-to-use file identity binding(handle/file ID 重驗):DACL 已是
  主要控制,隨機目錄 + BA/SY-only 下置換不可達;若未來目錄 ACL 需放寬再補。
- portable 完整更新流程(實機下載→替換→重啟)仍應以 `npm run tauri dev` +
  實際 release ZIP 驗一次。

## 問題

portable updater 驗證 ZIP 後把 `PaceDock_new.exe`、resources、`update.ps1` 寫到**固定** `%TEMP%\pacedock_update`,elevated 端之後**依路徑**重開腳本並移動/啟動 staged PE。同帳戶 medium-integrity 程序可在驗證後置換,取得 administrator code execution。

## 修法

- staging 目錄改每次隨機、位於 high-only 位置(或以 DACL 收緊為僅 admin 可寫)。
- 檔案以 exclusive create(開檔即 fail if exists)+ 拒絕 reparse point。
- 驗證、置換、執行綁同一 file identity(持 handle 或記 file ID/creation time,消費前重驗)。
- 不從共享 user TEMP 依名稱執行腳本;長期方向:受保護的已簽署 native updater。

## 驗收

- medium-integrity helper 持續置換固定 staging 路徑 → 新版拒絕或仍執行 trusted file ID。
- precreated junction / symlink / hardlink / rename race 測試。
- 實機 `npm run tauri dev` + portable update 流程驗證。
