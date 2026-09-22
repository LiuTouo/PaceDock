<script lang="ts">
  import { t } from "svelte-i18n";
  import type {
    CoreCapture,
    CoreTarget,
    QuickResult,
    SessionDetail,
    SessionSummary,
  } from "../../lib/types";

  // 歷史/結果區塊(自 GpuTest.svelte 拆出):detail/history 由頁面載入,
  // chosen 為 $bindable(無線電選擇),套用/刪除仍經頁面 ConfirmDialog。
  let {
    locked = false,
    eligible = false,
    quick,
    history,
    detail,
    chosen = $bindable(null),
    label,
    onSelect,
    onApply,
    onDelete,
  }: {
    locked?: boolean;
    eligible?: boolean;
    quick?: QuickResult | null;
    history: SessionSummary[];
    detail: SessionDetail | null;
    chosen?: number | null;
    label: (target: CoreTarget) => string;
    onSelect: (id: string) => void;
    onApply: () => void;
    onDelete: () => void;
  } = $props();

  function number(value: number | null | undefined) {
    return value == null ? "—" : value.toFixed(3);
  }
</script>

<section class="panel">
  <label class="field"
    >{$t("quick.history")}<select
      disabled={locked}
      value={detail?.summary.id ?? ""}
      onchange={(e) => onSelect(e.currentTarget.value)}
      ><option value="" disabled>{$t("quick.chooseHistory")}</option
      >{#each history as session}<option value={session.id}
          >{session.startedAt} — {session.gpuName} — {$t(
            `quick.session.${session.status}`,
          )}</option
        >{/each}</select
    ></label
  >
  {#if detail}
    <h2>{detail.summary.gpuName}</h2>
    <p>{$t(`quick.session.${detail.summary.status}`)}</p>
    {#if detail.summary.error}<p class="error" role="alert">
        {$t(`errors.${detail.summary.error}`, {
          default: detail.summary.error,
        })}
      </p>{/if}
    {#if quick}
      <h2>{$t(`quick.ranking.${quick.status}`)}</h2>
      <p>
        {$t("quick.gapHint")}{#if quick.relativeGapPct != null}
          ({number(quick.relativeGapPct)}%){/if}
      </p>
      {#if quick.status === "Close" || quick.status === "Reversed"}<p>
          {$t("quick.manualChoice")}
        </p>{/if}
      <h3>{$t("quick.retestResults")}</h3>
      {@render resultTable(quick.retest, true)}
      {#if quick.baseline}
        <section class="baseline-banner">
          <p>
            <strong>{$t(`quick.baseline.${quick.baseline.verdict}`)}</strong
            >{#if quick.baseline.p1ImprovementPct != null}
              · {quick.baseline.p1ImprovementPct >= 0
                ? "+"
                : ""}{quick.baseline.p1ImprovementPct.toFixed(1)}% 1% low{/if}{#if quick.baseline.avgImprovementPct != null}
              · {quick.baseline.avgImprovementPct >= 0
                ? "+"
                : ""}{quick.baseline.avgImprovementPct.toFixed(1)}% Avg FPS{/if}
          </p>
          <p class="hint">{$t("quick.baseline.hint")}</p>
        </section>
      {:else}
        <p class="hint">{$t("quick.baseline.none")}</p>
      {/if}
      <button
        class="primary"
        disabled={locked || !eligible || chosen === null}
        onclick={onApply}>{$t("quick.applySelected")}</button
      >
      <details>
        <summary>{$t("quick.screenResults")}</summary>{@render resultTable(
          quick.screening,
          false,
        )}
      </details>
      <details>
        <summary>{$t("quick.order")}</summary>
        <p>Seed: {quick.seed}</p>
        <p>
          {$t("quick.screenResults")}: {quick.screeningOrder
            .map((id) => quick.candidates.find((c) => c.coreId === id))
            .filter((c): c is CoreTarget => !!c)
            .map(label)
            .join(" → ")}
        </p>
        <p>
          {$t("quick.retestResults")}: {quick.retestOrder
            .map((id) => quick.candidates.find((c) => c.coreId === id))
            .filter((c): c is CoreTarget => !!c)
            .map(label)
            .join(" → ")}
        </p>
      </details>
    {:else}
      <p>{$t("quick.legacy")}</p>
      <div class="table-wrap">
        <table>
          <thead><tr><th>LP</th><th>Avg FPS</th><th>1% low</th></tr></thead
          ><tbody
            >{#each detail.results as row}<tr
                ><td>LP {row.lp}</td><td>{number(row.avgFps)}</td><td
                  >{number(row.p1Low)}</td
                ></tr
              >{/each}</tbody
          >
        </table>
      </div>
    {/if}
    <button class="danger" disabled={locked} onclick={onDelete}
      >{$t("quick.delete")}</button
    >
  {:else}<p>{$t("quick.chooseHistory")}</p>{/if}
</section>

{#snippet resultTable(rows: CoreCapture[], selectable: boolean)}
  <div class="table-wrap">
    <table>
      <thead
        ><tr
          ><th>{$t("quick.core")}</th><th>{$t("quick.score")}</th><th
            >Avg FPS</th
          ><th>1% low</th><th>0.1% low</th><th>MAD %</th><th>Spike %</th><th
            >{$t("quick.colDisplayP99")}</th
          ><th>{$t("quick.colDropped")}</th></tr
        ></thead
      ><tbody
        >{#each rows as row}<tr
            ><td
              >{#if selectable}<label
                  ><input
                    type="radio"
                    name="result-core"
                    value={row.target.coreId}
                    bind:group={chosen}
                    disabled={locked || !eligible}
                  />{label(row.target)}</label
                >{:else}{label(row.target)}{/if}</td
            ><td>{number(row.score)}</td><td>{number(row.metrics.avgFps)}</td
            ><td>{number(row.metrics.p1Low)}</td><td
              >{number(row.metrics.p01Low)}</td
            ><td>{number(row.metrics.frametimeMadPct)}</td><td
              >{number(row.metrics.spikeRatePct)}</td
            ><td>{number(row.metrics.displayLatencyP99Ms)}</td><td
              >{number(row.metrics.droppedPct)}</td
            ></tr
          >{/each}</tbody
      >
    </table>
  </div>
{/snippet}

<style>
  h2 {
    font-size: 17px;
    margin: 12px 0;
  }
  h3 {
    font-size: 15px;
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
