import type { CoreTarget } from "./types";

export function coreLabel(target: CoreTarget, english: boolean): string {
  return english
    ? `Physical core ${target.coreId} (LP ${target.lpIndices.join(", ")})`
    : `實體核心 ${target.coreId}（LP ${target.lpIndices.join("、")}）`;
}

/** 逐 byte 解碼，不把 64-bit mask 轉成 JavaScript Number。 */
export function policyIndices(bytes: number[] | null | undefined): number[] {
  return (bytes ?? []).flatMap((byte, offset) =>
    Array.from({ length: 8 }, (_, bit) => offset * 8 + bit).filter(
      (lp) => (byte & (1 << (lp % 8))) !== 0,
    ),
  );
}
