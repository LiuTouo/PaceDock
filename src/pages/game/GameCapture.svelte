<script lang="ts">
  import { onMount } from "svelte";
  import { t } from "svelte-i18n";
  import type {
    GameCaptureProgress,
    GameCaptureRecord,
    GameWindow,
  } from "../../lib/types";
  import { listen } from "@tauri-apps/api/event";
  import * as ipc from "../../lib/ipc";

  // 遊戲量測區塊(自 GpuTest.svelte 拆出):自身擁有量測狀態,
  // 破壞性刪除仍經頁面統一 ConfirmDialog(requestDelete → captureDeleted)。
  let {
    locked = false,
    recovery = false,
    gpu = "",
    onError,
    onNotice,
    requestDelete,
  }: {
    locked?: boolean;
    recovery?: boolean;
    gpu?: string;
    onError: (message: string) => void;
    onNotice: (message: string) => void;
    requestDelete: (captureId: string) => void;
  } = $props();

  let games = $state<GameWindow[]>([]);
  let captureGame = $state("");
  let captureDuration = $state(30);
  let capturing = $state(false);
  let capturePct = $state(0);
  let captures = $state<GameCaptureRecord[]>([]);
  let pickA = $state("");
  let pickB = $state("");
  // 首次清單載入完成前不渲染 <details>:`open={!captures.length}` 需在
  // 資料備妥後評估,避免載入後屬性回寫與使用者點擊互相覆蓋(原版為應用
  // 啟動時預載,時序上無此競態)。
  let ready = $state(false);

  export async function refreshGames() {
    try {
      games = await ipc.listGameWindows();
      if (!games.some((g) => String(g.pid) === captureGame))
        captureGame = games[0] ? String(games[0].pid) : "";
    } catch (e) {
      onError(String(e));
    }
  }
  export async function refreshCaptures() {
    try {
      captures = await ipc.listGameCaptures();
    } catch (e) {
      onError(String(e));
    } finally {
      ready = true;
    }
  }
  /** 頁面刪除成功後回呼:清掉指向已刪除紀錄的選擇並重讀清單。 */
  export async function captureDeleted(deletedId: string) {
    if (pickB === deletedId) pickB = "";
    if (pickA === deletedId) pickA = "";
    await refreshCaptures();
  }

  async function startCapture() {
    const game = games.find((g) => String(g.pid) === captureGame);
    if (!game || capturing || locked) return;
    capturing = true;
    capturePct = 0;
    try {
      const record = await ipc.startGameCapture(
        game.pid,
        game.title,
        captureDuration,
        gpu || null,
      );
      await refreshCaptures();
      pickA = record.id;
      if (!pickB || pickB === record.id)
        pickB = captures.find((c) => c.id !== record.id)?.id ?? "";
      onNotice($t("quick.done"));
    } catch (e) {
      if (String(e) !== "cancelled") onError(String(e));
    } finally {
      capturing = false;
    }
  }

  type MetricRow = {
    label: string;
    a: number | null;
    b: number | null;
    relative: boolean;
  };
  const metricRows = $derived.by<MetricRow[]>(() => {
    const a = captures.find((c) => c.id === pickA);
    const b = captures.find((c) => c.id === pickB);
    if (!a || !b || a.id === b.id) return [];
    return [
      {
        label: "Avg FPS",
        a: a.metrics.avgFps,
        b: b.metrics.avgFps,
        relative: true,
      },
      {
        label: "1% low",
        a: a.metrics.p1Low,
        b: b.metrics.p1Low,
        relative: true,
      },
      {
        label: "0.1% low",
        a: a.metrics.p01Low,
        b: b.metrics.p01Low,
        relative: true,
      },
      {
        label: "MAD %",
        a: a.metrics.frametimeMadPct,
        b: b.metrics.frametimeMadPct,
        relative: false,
      },
      {
        label: "Spike %",
        a: a.metrics.spikeRatePct,
        b: b.metrics.spikeRatePct,
        relative: false,
      },
      {
        label: $t("measure.rowDisplayP99"),
        a: a.metrics.displayLatencyP99Ms,
        b: b.metrics.displayLatencyP99Ms,
        relative: false,
      },
      {
        label: $t("measure.rowDropped"),
        a: a.metrics.droppedPct,
        b: b.metrics.droppedPct,
        relative: false,
      },
      {
        label: "Frames",
        a: a.metrics.sampleCount,
        b: b.metrics.sampleCount,
        relative: false,
      },
    ];
  });
  function fmtVal(v: number | null) {
    return v == null ? "—" : Number.isInteger(v) ? String(v) : v.toFixed(1);
  }
  function fmtDelta(row: MetricRow) {
    if (row.a == null || row.b == null || row.b === 0) return "—";
    return row.relative
      ? `${(((row.a - row.b) / row.b) * 100).toFixed(1)}%`
      : `${(row.a - row.b).toFixed(1)}`;
  }

  onMount(() => {
    void refreshGames();
    void refreshCaptures();
    let unlisten: (() => void) | null = null;
    void listen<GameCaptureProgress>("game-capture-progress", (e) => {
      if (e.payload.stage === "done") {
        capturing = false;
        void refreshCaptures();
      } else if (e.payload.stage === "cancelled") capturing = false;
      else capturePct = e.payload.percentage;
    }).then((fn) => {
      unlisten = fn;
    });
    return () => {
      unlisten?.();
    };
  });
</script>

{#if ready}
  <details open={!captures.length}>
    <summary>{$t("measure.tab")}</summary>
    <section class="panel">
      <p class="hint">{$t("measure.hint")}</p>
      <div class="form-grid">
        <label class="field"
          >{$t("measure.game")}
          <select bind:value={captureGame} disabled={locked || capturing}>
            <option value="" disabled hidden>{$t("measure.noGames")}</option>
            {#each games as game}<option value={String(game.pid)}
                >{game.title} — {game.exeName} (PID {game.pid})</option
              >{/each}
          </select>
        </label>
        <label class="field"
          >{$t("measure.duration")}
          <input
            type="number"
            min="5"
            max="600"
            bind:value={captureDuration}
            disabled={locked || capturing}
          />
        </label>
      </div>
      <div class="start-row">
        <button
          disabled={locked || capturing}
          onclick={() => void refreshGames()}>{$t("measure.refresh")}</button
        >
        <button
          disabled={capturing}
          onclick={() => void ipc.cancelGameCapture().catch(() => {})}
          >{$t("measure.cancel")}</button
        >
        <button
          class="primary"
          disabled={locked || recovery || capturing || !captureGame}
          onclick={startCapture}>{$t("measure.start")}</button
        >
      </div>
      {#if capturing}
        <p role="status">{$t("measure.running")}</p>
        <progress max="100" value={capturePct}></progress>
      {/if}
    </section>
    <section class="panel">
      <h2>{$t("measure.compare")}</h2>
      <p class="hint">{$t("measure.compareHint")}</p>
      {#if !captures.length}
        <p>{$t("measure.noCaptures")}</p>
      {:else}
        <div class="form-grid">
          <label class="field"
            >A
            <select bind:value={pickA} disabled={locked || capturing}>
              {#each captures as c}<option value={c.id}
                  >{c.startedAt} — {c.gameTitle} ({c.durationSecs}s){#if c.lockedLp != null}
                    · LP {c.lockedLp}{/if}</option
                >{/each}
            </select>
          </label>
          <label class="field"
            >B
            <select bind:value={pickB} disabled={locked || capturing}>
              <option value="" hidden>—</option>
              {#each captures as c}<option value={c.id}
                  >{c.startedAt} — {c.gameTitle} ({c.durationSecs}s){#if c.lockedLp != null}
                    · LP {c.lockedLp}{/if}</option
                >{/each}
            </select>
          </label>
        </div>
        {#if metricRows.length}
          <div class="table-wrap">
            <table>
              <thead><tr><th></th><th>A</th><th>B</th><th>Δ</th></tr></thead>
              <tbody
                >{#each metricRows as row}<tr
                    ><td>{row.label}</td><td>{fmtVal(row.a)}</td><td
                      >{fmtVal(row.b)}</td
                    ><td>{fmtDelta(row)}</td></tr
                  >{/each}</tbody
              >
            </table>
          </div>
        {/if}
        <button
          class="danger"
          disabled={locked || capturing || !pickA}
          onclick={() => requestDelete(pickA)}>{$t("measure.delete")}</button
        >
      {/if}
    </section>
  </details>
{/if}

<style>
  h2 {
    font-size: 17px;
    margin: 12px 0;
  }
  .table-wrap {
    overflow-x: auto;
    margin: 14px 0;
  }
  table {
    border-collapse: collapse;
    width: 100%;
    font-variant-numeric: tabular-nums;
  }
  th,
  td {
    padding: 10px;
    border-bottom: 1px solid var(--border-default);
    text-align: right;
    white-space: nowrap;
  }
  th:first-child,
  td:first-child {
    text-align: left;
  }
</style>
