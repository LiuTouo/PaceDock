# 按鈕介面回歸測試

執行 `npm ci` 後，使用 `npm run test:ui`。Windows 使用已安裝的 Microsoft Edge；其他平台先執行 `npx playwright install chromium`。

測試會自動啟動獨立的 Vite 伺服器（127.0.0.1:1421），並在載入介面前模擬 Tauri IPC。未知 IPC 會報錯，不會執行真正的 GPU 測試、安裝更新或變更系統設定。

涵蓋 Dark／Light、繁中／英文、各頁按鈕及確認框的正常、懸停、按下、焦點、停用樣式，以及實際忙碌狀態、鍵盤操作、長標籤與 compact 視窗。使用瀏覽器計算後的色彩驗證文字對比至少 4.5:1、焦點至少 3:1，並檢查文字及按鈕沒有超出容器。

截圖保存在 `test-results/`；失敗時另保留 Playwright trace。這些檔案已排除版本控制。此測試驗證瀏覽器中的介面，不取代原生 WebView2、Windows DPI 與真實 IPC 整合測試。
