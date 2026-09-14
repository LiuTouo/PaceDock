// 重新取得基準測試內建資源並更新 SHA256SUMS：
//   - PresentMon-2.5.1-x64.exe（GameTechDev/PresentMon，MIT）
//   - lava-triangle.exe + LICENSE（valleyofdoom/AutoGpuAffinity 1.0.0 內附的
//     liblava Vulkan workload，MIT）
// 下載後以 mt.exe 嵌入 lava-triangle.manifest（宣告 PerMonitorV2 DPI 感知），
// 再計算 hash，確保 vendor 的二進位檔含 DPI 清單。
// 資產已 vendor 在 git 內；此 script 供重新取得/校驗 manifest。
import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, readdirSync, readFileSync, writeFileSync, rmSync } from 'node:fs';
import path from 'node:path';
import { spawnSync } from 'node:child_process';

const MANIFEST = 'scripts/lava-triangle.manifest';

/** 在 Windows Kits 10 安裝中尋找 x64 mt.exe，取最高 SDK 版本。 */
function findMtExe() {
  const base = 'C:\\Program Files (x86)\\Windows Kits\\10\\bin';
  if (!existsSync(base)) return null;
  const dirs = readdirSync(base, { withFileTypes: true })
    .filter(d => d.isDirectory() && /^\d+\.\d+\.\d+\.\d+$/.test(d.name))
    .map(d => d.name)
    .sort()
    .reverse();
  for (const ver of dirs) {
    const mt = path.join(base, ver, 'x64', 'mt.exe');
    if (existsSync(mt)) return mt;
  }
  return null;
}

/** 將 manifest 嵌入 PE 的 RT_MANIFEST 資源（#1），覆寫原有 manifest。 */
function embedManifest(exe) {
  const mt = findMtExe();
  if (!mt) {
    throw new Error(
      '找不到 Windows SDK mt.exe。請安裝 Windows 10 SDK（含 Windows Kits 10\\bin\\<ver>\\x64\\mt.exe）。',
    );
  }
  console.log(`嵌入 manifest: ${MANIFEST} → ${exe}（mt=${mt}）`);
  const res = spawnSync(mt, ['-manifest', MANIFEST, `-outputresource:${exe};#1`], { stdio: 'inherit' });
  if (res.status !== 0) throw new Error(`mt.exe 嵌入 manifest 失敗，exit=${res.status}`);
}

const DIR = 'src-tauri/resources/benchmark';
const TMP = 'scripts/.assets-tmp';
mkdirSync(TMP, { recursive: true });

// known-good trust root：下載（含 manifest 嵌入後）的最终產物必須與此處
// 完全一致，script 才會覆寫 vendored 檔案與 SHA256SUMS。此處的 digest 變更
// 屬於信任根更新，必須獨立 review，不得由下載流程自動產生。
// 升級上游版本時：人工確認新版本 → 同一 PR 內同時更新此處與重新 vendor。
const KNOWN_GOOD = {
  'PresentMon-2.5.1-x64.exe': '9bec3083069f58f911e6a512f4806db51a27bd096103087bc1d05ef54c80a191',
  'lava-triangle.exe': 'c4beae5889d99682f80d79d143d81123ab5a5e045568c76ad1936194c66ae547',
};

const PRESENTMON_URL =
  'https://github.com/GameTechDev/PresentMon/releases/download/v2.5.1/PresentMon-2.5.1-x64.exe';
const PRESENTMON_LICENSE_URL =
  'https://raw.githubusercontent.com/GameTechDev/PresentMon/v2.5.1/LICENSE.txt';
const AGA_ZIP_URL =
  'https://github.com/valleyofdoom/AutoGpuAffinity/releases/download/1.0.0/AutoGpuAffinity.zip';

function sha256(file) {
  return createHash('sha256').update(readFileSync(file)).digest('hex');
}

function download(url, dest) {
  console.log(`下載 ${url}`);
  const res = spawnSync('curl', ['-sL', '--max-time', '180', '-o', dest, url], { stdio: 'inherit' });
  if (res.status !== 0 || !existsSync(dest)) {
    throw new Error(`下載失敗: ${url}`);
  }
}

try {
  const pm = path.join(TMP, 'PresentMon-2.5.1-x64.exe');
  download(PRESENTMON_URL, pm);
  const pmLic = path.join(TMP, 'LICENSE-PresentMon.txt');
  download(PRESENTMON_LICENSE_URL, pmLic);

  const zip = path.join(TMP, 'aga.zip');
  download(AGA_ZIP_URL, zip);
  const ext = path.join(TMP, 'aga_ext');
  rmSync(ext, { recursive: true, force: true });
  mkdirSync(ext, { recursive: true });
  const unzip = spawnSync('powershell', ['-NoProfile', '-Command', `Expand-Archive -Path '${path.resolve(zip)}' -DestinationPath '${path.resolve(ext)}' -Force`], { stdio: 'inherit' });
  if (unzip.status !== 0) throw new Error('解壓 AutoGpuAffinity.zip 失敗');
  const lava = path.join(ext, 'AutoGpuAffinity/bin/liblava/lava-triangle.exe');
  if (!existsSync(lava)) throw new Error(`找不到 ${lava}`);
  const lavaLic = path.join(ext, 'AutoGpuAffinity/bin/liblava/LICENSE.txt');

  mkdirSync(DIR, { recursive: true });
  const copy = (src, dst) => {
    writeFileSync(dst, readFileSync(src));
    console.log(`[OK] ${dst}`);
  };
  copy(pm, path.join(DIR, 'PresentMon-2.5.1-x64.exe'));
  copy(lava, path.join(DIR, 'lava-triangle.exe'));
  if (existsSync(lavaLic)) copy(lavaLic, path.join(DIR, 'LICENSE-liblava.txt'));
  copy(pmLic, path.join(DIR, 'LICENSE-PresentMon.txt'));
  rmSync(path.join(DIR, 'PresentMon-1.10.0-x64.exe'), { force: true });

  // 嵌入 DPI 感知 manifest（PerMonitorV2），讓全螢幕覆蓋縮放 >100% 的整個桌面
  embedManifest(path.join(DIR, 'lava-triangle.exe'));

  // 驗證最終產物（嵌入 manifest 後）與 known-good 一致；不符即中止，不寫 SHA256SUMS
  for (const [file, want] of Object.entries(KNOWN_GOOD)) {
    const got = sha256(path.join(DIR, file));
    if (got !== want) {
      throw new Error(
        `下載的 ${file} 與 known-good digest 不符\n  期望: ${want}\n  實際: ${got}\n` +
        `上游內容可能已變更；若為刻意升級，請人工確認後在同一 PR 更新 KNOWN_GOOD。`,
      );
    }
    console.log(`[verified] ${file} = ${got}`);
  }

  const manifest = [
    '# PaceDock GPU 基準測試內建資源的固定 SHA-256。',
    '# 驗證：npm run verify:benchmark-assets；執行基準測試前也會驗證。',
    '# 來源：',
    '#   PresentMon-2.5.1-x64.exe — https://github.com/GameTechDev/PresentMon/releases/tag/v2.5.1（MIT）',
    '#   lava-triangle.exe        — valleyofdoom/AutoGpuAffinity 1.0.0 內附的 liblava Vulkan workload（MIT）',
    `${sha256(path.join(DIR, 'PresentMon-2.5.1-x64.exe'))}  PresentMon-2.5.1-x64.exe`,
    `${sha256(path.join(DIR, 'lava-triangle.exe'))}  lava-triangle.exe`,
    '',
  ].join('\n');
  writeFileSync(path.join(DIR, 'SHA256SUMS'), manifest);
  console.log('SHA256SUMS 已更新');
} catch (e) {
  console.error(`[FAIL] ${e.message}`);
  process.exit(1);
} finally {
  rmSync(TMP, { recursive: true, force: true });
}
