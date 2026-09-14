// 從 canonical source PNG 產生 PaceDock 圖示：icon.png (256) / 128x128.png / 32x32.png / icon.ico
// 無第三方依賴，純 JS PNG decoder + bilinear resize + PNG encoder + ICO 容器。
import { deflateSync, inflateSync } from 'node:zlib';
import { readFileSync, writeFileSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const REPO = join(dirname(fileURLToPath(import.meta.url)), '..');
const outDir = join(REPO, 'src-tauri', 'icons');
const sourcePath = join(outDir, 'PaceDock-icon.png');
mkdirSync(outDir, { recursive: true });

// ---- CRC32 ----
const crcTable = new Uint32Array(256).map((_, n) => {
  let c = n;
  for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
  return c >>> 0;
});
const crc32 = (buf) => {
  let c = 0xffffffff;
  for (const b of buf) c = crcTable[(c ^ b) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
};

function pngChunk(type, data) {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length);
  const body = Buffer.concat([Buffer.from(type, 'ascii'), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(body));
  return Buffer.concat([len, body, crc]);
}

// ---- PNG encoder (reused from prior version) ----
function encodePng(size, rgba) {
  const sig = Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]);
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(size, 0);
  ihdr.writeUInt32BE(size, 4);
  ihdr[8] = 8;  // bit depth
  ihdr[9] = 6;  // RGBA
  const raw = Buffer.alloc(size * (size * 4 + 1));
  for (let y = 0; y < size; y++) {
    raw[y * (size * 4 + 1)] = 0; // filter: none
    rgba.copy(raw, y * (size * 4 + 1) + 1, y * size * 4, (y + 1) * size * 4);
  }
  return Buffer.concat([sig, pngChunk('IHDR', ihdr), pngChunk('IDAT', deflateSync(raw, { level: 9 })), pngChunk('IEND', Buffer.alloc(0))]);
}

// ---- PNG decoder ----
function decodePng(buf) {
  // 驗證簽名
  const sig = Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]);
  if (Buffer.compare(buf.subarray(0, 8), sig) !== 0) throw new Error('Not a PNG file');

  let width, height, bitDepth, colorType;
  const idatChunks = [];
  let palette = null;   // [r,g,b, ...] per entry
  let transparency = null; // alpha per palette index

  let offset = 8;
  while (offset < buf.length) {
    const len = buf.readUInt32BE(offset);
    const type = buf.subarray(offset + 4, offset + 8).toString('ascii');
    const data = buf.subarray(offset + 8, offset + 8 + len);
    offset += 12 + len;

    if (type === 'IHDR') {
      width = data.readUInt32BE(0);
      height = data.readUInt32BE(4);
      bitDepth = data[8];
      colorType = data[9];
    } else if (type === 'PLTE') {
      palette = data;
    } else if (type === 'tRNS') {
      transparency = data;
    } else if (type === 'IDAT') {
      idatChunks.push(data);
    } else if (type === 'IEND') {
      break;
    }
  }

  if (bitDepth !== 8) throw new Error(`Unsupported bit depth: ${bitDepth}`);

  const compressed = Buffer.concat(idatChunks);
  const raw = inflateSync(compressed);

  // ---- 解濾波器（針對原始 sample 寬度運作） ----
  const paeth = (a, b, c) => {
    const p = a + b - c;
    const pa = Math.abs(p - a), pb = Math.abs(p - b), pc = Math.abs(p - c);
    return pa <= pb && pa <= pc ? a : pb <= pc ? b : c;
  };

  let bpp, samples;
  if (colorType === 6) {
    bpp = 4; // RGBA
    samples = Buffer.alloc(width * height * bpp);
  } else if (colorType === 3) {
    bpp = 1; // indexed
    samples = Buffer.alloc(width * height);
  } else {
    throw new Error(`Unsupported color type: ${colorType}`);
  }

  const stride = width * bpp;
  let srcOff = 0;
  for (let y = 0; y < height; y++) {
    const filter = raw[srcOff++];
    const rowStart = y * stride;
    for (let x = 0; x < stride; x++) {
      const a = x >= bpp ? samples[rowStart + x - bpp] : 0;
      const b = y > 0 ? samples[rowStart + x - stride] : 0;
      const c = y > 0 && x >= bpp ? samples[rowStart + x - stride - bpp] : 0;
      const v = raw[srcOff++];
      switch (filter) {
        case 0: samples[rowStart + x] = v; break;
        case 1: samples[rowStart + x] = (v + a) & 0xff; break;
        case 2: samples[rowStart + x] = (v + b) & 0xff; break;
        case 3: samples[rowStart + x] = (v + ((a + b) >>> 1)) & 0xff; break;
        case 4: samples[rowStart + x] = (v + paeth(a, b, c)) & 0xff; break;
        default: throw new Error(`Unknown filter: ${filter}`);
      }
    }
  }

  // ---- 若為索引色，展開為 RGBA ----
  if (colorType === 3) {
    if (!palette) throw new Error('Indexed PNG missing PLTE chunk');
    const rgba = Buffer.alloc(width * height * 4);
    for (let i = 0; i < width * height; i++) {
      const idx = samples[i];
      const po = idx * 3;
      rgba[i * 4]     = palette[po];
      rgba[i * 4 + 1] = palette[po + 1];
      rgba[i * 4 + 2] = palette[po + 2];
      // tRNS 提供各索引的 alpha；未覆蓋者為 255
      rgba[i * 4 + 3] = transparency && idx < transparency.length ? transparency[idx] : 255;
    }
    return { width, height, pixels: rgba };
  }

  return { width, height, pixels: samples };
}

// ---- Bilinear resize ----
function resize(src, srcW, srcH, dstW, dstH) {
  const dst = Buffer.alloc(dstW * dstH * 4);
  const xRatio = srcW / dstW;
  const yRatio = srcH / dstH;
  // 預取 pixel fn 加速
  const g = (x, y, ch) => src[(Math.min(y, srcH - 1) * srcW + Math.min(x, srcW - 1)) * 4 + ch];

  for (let dy = 0; dy < dstH; dy++) {
    for (let dx = 0; dx < dstW; dx++) {
      const sx = (dx + 0.5) * xRatio - 0.5;
      const sy = (dy + 0.5) * yRatio - 0.5;
      const x0 = Math.max(0, Math.floor(sx));
      const y0 = Math.max(0, Math.floor(sy));
      const x1 = Math.min(srcW - 1, x0 + 1);
      const y1 = Math.min(srcH - 1, y0 + 1);
      const fx = Math.max(0, Math.min(1, sx - x0));
      const fy = Math.max(0, Math.min(1, sy - y0));

      for (let ch = 0; ch < 4; ch++) {
        const v00 = g(x0, y0, ch), v10 = g(x1, y0, ch);
        const v01 = g(x0, y1, ch), v11 = g(x1, y1, ch);
        const v = (v00 * (1 - fx) + v10 * fx) * (1 - fy) + (v01 * (1 - fx) + v11 * fx) * fy;
        dst[(dy * dstW + dx) * 4 + ch] = Math.round(v);
      }
    }
  }
  return dst;
}

// ---- ICO encoder ----
function encodeIco(pngBuf, size) {
  const header = Buffer.alloc(6);
  header.writeUInt16LE(0, 0); header.writeUInt16LE(1, 2); header.writeUInt16LE(1, 4);
  const entry = Buffer.alloc(16);
  entry[0] = size >= 256 ? 0 : size;
  entry[1] = size >= 256 ? 0 : size;
  entry.writeUInt16LE(1, 4);   // planes
  entry.writeUInt16LE(32, 6);  // bpp
  entry.writeUInt32LE(pngBuf.length, 8);
  entry.writeUInt32LE(22, 12);
  return Buffer.concat([header, entry, pngBuf]);
}

// ---- Main ----
const source = decodePng(readFileSync(sourcePath));
console.log(`Source: ${source.width}x${source.height} RGBA`);

for (const size of [256, 128, 32]) {
  const rgba = resize(source.pixels, source.width, source.height, size, size);
  const png = encodePng(size, rgba);
  const name = size === 256 ? 'icon.png' : `${size}x${size}.png`;
  writeFileSync(join(outDir, name), png);
  if (size === 256) writeFileSync(join(outDir, 'icon.ico'), encodeIco(png, size));
}
console.log('icons written to', outDir);
