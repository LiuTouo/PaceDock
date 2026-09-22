import fs from 'fs';
import path from 'path';

const en = JSON.parse(fs.readFileSync('src/i18n/en.json', 'utf8'));
const zh = JSON.parse(fs.readFileSync('src/i18n/zh-TW.json', 'utf8'));

function flat(o, p = '') {
  return Object.entries(o).flatMap(([k, v]) =>
    (v !== null && typeof v === 'object') ? flat(v, p + k + '.') : [p + k]
  );
}
const ek = new Set(flat(en)), zk = new Set(flat(zh));

const staticKeys = new Map();
const dynKeys = new Map();
const RE = /\$t\(\s*(['"`])([^'"`]+)\1/g;
function walk(d) {
  for (const f of fs.readdirSync(d)) {
    const fp = path.join(d, f);
    if (fs.statSync(fp).isDirectory()) walk(fp);
    else if (/\.(svelte|ts|js|mjs)$/.test(f)) {
      const src = fs.readFileSync(fp, 'utf8');
      for (const m of src.matchAll(RE)) {
        const k = m[2];
        const target = k.includes('${') ? dynKeys : staticKeys;
        if (!target.has(k)) target.set(k, fp);
      }
    }
  }
}
walk('src');

let miss = 0;
for (const [k, loc] of staticKeys) {
  const inE = ek.has(k), inZ = zk.has(k);
  if (!inE || !inZ) {
    miss++;
    console.log((!inE && !inZ ? 'BOTH  ' : !inE ? 'EN-miss' : 'ZH-miss') + ' ' + k + '  @' + loc);
  }
}
console.log('--- static used:', staticKeys.size, '| missing:', miss);
console.log('--- dynamic patterns:');
for (const k of dynKeys.keys()) console.log('   ' + k);

for (const k of ek) if (!zk.has(k)) { miss++; console.log('in EN not ZH:', k); }
for (const k of zk) if (!ek.has(k)) { miss++; console.log('in ZH not EN:', k); }

// ── 後端交叉驗證：locale 必須涵蓋後端實際會 emit 的每個值 ──
// 來源：error.rs 的錯誤碼常數、types.ts 的 union 型別、runner/manager emit 的字串。
// 新增後端 enum 值或 stage 字串時，這裡自動或手動同步（清單者需同步）。

// 1) 錯誤碼：src-tauri/src/error.rs 每個 pub const NAME → errors.NAME
const errorRs = fs.readFileSync('src-tauri/src/error.rs', 'utf8');
for (const m of errorRs.matchAll(/pub const ([A-Z0-9_]+):/g)) {
  const k = 'errors.' + m[1];
  if (!ek.has(k) || !zk.has(k)) { miss++; console.log('backend error code missing:', k); }
}

// 2) phase/session：解析 types.ts 的 union 型別（後端 serde 值的鏡像）
const typesTs = fs.readFileSync('src/lib/types.ts', 'utf8');
function unionMembers(typeName) {
  const i = typesTs.indexOf(`type ${typeName} =`);
  if (i < 0) throw new Error(`types.ts 找不到 ${typeName}`);
  const seg = typesTs.slice(i, typesTs.indexOf(';', i));
  return [...seg.matchAll(/"([^"]+)"/g)].map((m) => m[1]);
}
for (const v of unionMembers('BenchmarkPhase')) {
  const k = 'quick.phase.' + v;
  if (!ek.has(k) || !zk.has(k)) { miss++; console.log('backend phase missing:', k); }
}
for (const v of unionMembers('SessionStatus')) {
  const k = 'quick.session.' + v;
  if (!ek.has(k) || !zk.has(k)) { miss++; console.log('backend session status missing:', k); }
}

// 3) 後端以字串 emit、無型別可解析的值（顯式清單，新增時同步）
const backendEmitted = {
  // runner.rs emit()/emit_cancel() 與 manager.rs stage 字串；starting 為前端 fallback 預設
  'quick.stage': ['starting', 'applying', 'launching', 'collecting', 'collected', 'finalizing', 'restarting'],
  'quick.cancelStage': ['requested', 'stopping', 'restoring', 'finalizing'],
  'quick.ranking': ['Consistent', 'Close', 'Reversed', 'SingleCandidate', 'Insufficient'],
  'quick.baseline': ['BeatsDefault', 'WithinThreshold', 'Worse', 'Inconclusive'],
};
for (const [prefix, values] of Object.entries(backendEmitted)) {
  for (const v of values) {
    const k = prefix + '.' + v;
    if (!ek.has(k) || !zk.has(k)) { miss++; console.log('backend emitted value missing:', k); }
  }
}

console.log('--- dynamic group coverage:');
for (const prefix of Object.keys(backendEmitted)) {
  const eKeys = [...ek].filter((x) => x.startsWith(prefix + '.'));
  const zKeys = [...zk].filter((x) => x.startsWith(prefix + '.'));
  const diff = eKeys.filter((x) => !zKeys.includes(x)).concat(zKeys.filter((x) => !eKeys.includes(x)));
  console.log(`   ${prefix} en=${eKeys.length} zh=${zKeys.length} diff=${diff.length ? diff.join(',') : 'none'}`);
}

console.log(miss === 0 ? 'i18n check: OK' : `i18n check: ${miss} missing`);
process.exit(miss === 0 ? 0 : 1);
