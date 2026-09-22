// IPC 契約 parity 測試:Rust generate_handler 註冊清單 ↔ 前端 invoke 呼叫 ↔ UI 測試 mock
// 三處人工同步,新增 IPC 忘記任一處時在此提前失敗,而不是到 UI 測試或執行期才炸。
import { readFileSync, readdirSync, statSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { test } from 'node:test';
import assert from 'node:assert/strict';

const root = join(dirname(fileURLToPath(import.meta.url)), '..', '..');

function read(p) {
  return readFileSync(join(root, p), 'utf8');
}

// 1) main.rs generate_handler![...] 註冊清單(去掉 module 路徑前綴)
function registeredCommands() {
  const main = read('src-tauri/src/main.rs');
  const m = main.match(/generate_handler!\[([\s\S]*?)\]/);
  assert.ok(m, 'main.rs 找不到 generate_handler! 註冊區塊');
  return new Set(
    m[1]
      .split(/[\s,]+/)
      .filter((s) => s && !s.startsWith('//'))
      .map((s) => s.split('::').pop()),
  );
}

// 2) 前端所有 invoke('cmd') / invoke<T>('cmd') 呼叫(排除 plugin: 內部命令)
function frontendCommands() {
  const cmds = new Set();
  const walk = (dir) => {
    for (const name of readdirSync(dir)) {
      const p = join(dir, name);
      if (statSync(p).isDirectory()) walk(p);
      else if (/\.(ts|svelte)$/.test(name)) {
        const src = readFileSync(p, 'utf8');
        for (const match of src.matchAll(/invoke(?:<[^>]*>)?\(\s*['"]([^'"]+)['"]/g)) {
          if (!match[1].startsWith('plugin:')) cmds.add(match[1]);
        }
      }
    }
  };
  walk(join(root, 'src'));
  return cmds;
}

// 3) UI 測試 mock 的命令(排除 plugin: 內部命令)
function mockedCommands() {
  const mock = read('tests/ui/mock-tauri.js');
  return new Set(
    [...mock.matchAll(/case\s+['"]([^'"]+)['"]/g)].map((m) => m[1]).filter((c) => !c.startsWith('plugin:')),
  );
}

// UI 測試未涵蓋、因此未 mock 的命令(新增時請思考能否補進 UI 測試)
const UNMOCKED_ALLOWED = new Set([
  'apply_gpu_core',
  'start_gpu_benchmark',
  'begin_update',
  'end_update',
  'delete_benchmark_session',
  'delete_game_capture',
  'get_benchmark_storage_info',
  'restore_previous_gpu_affinity',
  'start_game_capture',
  'cancel_game_capture',
  'perform_portable_update',
  'set_timer_exempt',
  'set_timer_global_enabled',
  'scan_dpc_offenders',
  'verify_interrupt_affinity',
  'apply_msi',
  'restore_msi',
  'apply_power_tweak',
  'restore_power_tweak',
]);

test('前端每個 invoke 命令都已註冊於 main.rs generate_handler', () => {
  const registered = registeredCommands();
  const missing = [...frontendCommands()].filter((c) => !registered.has(c));
  assert.deepEqual(missing, [], `未註冊的前端 IPC 命令: ${missing.join(', ')}`);
});

test('UI 測試 mock 的命令都真實存在(防止 mock 與後端 drift)', () => {
  const registered = registeredCommands();
  const ghost = [...mockedCommands()].filter((c) => !registered.has(c));
  assert.deepEqual(ghost, [], 'mock 了不存在的命令(改名後殘留): ' + ghost.join(', '));
});

test('前端命令若未 mock 於 UI 測試,必須列於 UNMOCKED_ALLOWED', () => {
  const mocked = mockedCommands();
  const unmocked = [...frontendCommands()].filter((c) => !mocked.has(c));
  const illegal = unmocked.filter((c) => !UNMOCKED_ALLOWED.has(c));
  assert.deepEqual(illegal, [], `未 mock 且未登記 allowlist 的命令: ${illegal.join(', ')}`);
  // 反向:allowlist 裡的命令若已全被 mock,就該刪掉登記
  const stale = [...UNMOCKED_ALLOWED].filter((c) => mocked.has(c));
  assert.deepEqual(stale, [], 'allowlist 中已 mock 的過時項: ' + stale.join(', '));
});
