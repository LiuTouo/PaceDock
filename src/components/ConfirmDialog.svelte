<script lang="ts">
  import { tick } from "svelte";

  let {
    open = $bindable(false),
    title,
    message,
    detail = null,
    confirmLabel,
    cancelLabel,
    danger = false,
    busy = false,
    onconfirm,
    oncancel,
  }: {
    open?: boolean;
    title: string;
    message: string;
    detail?: string | null;
    confirmLabel: string;
    cancelLabel: string;
    danger?: boolean;
    busy?: boolean;
    onconfirm: () => void | Promise<void>;
    oncancel?: () => void;
  } = $props();

  let dialogEl = $state<HTMLDivElement>();
  let previousFocus = $state<HTMLElement | null>(null);
  let cancelBtn = $state<HTMLButtonElement>();
  let confirmBtn = $state<HTMLButtonElement>();
  const dialogId = `confirm-dialog-${Math.random().toString(36).slice(2, 8)}`;
  const titleId = `${dialogId}-title`;
  const descId = `${dialogId}-desc`;

  function close() {
    if (busy) return;
    open = false;
    oncancel?.();
  }

  function onkeydown(event: KeyboardEvent) {
    if (!open) return;
    if (event.key === "Escape") {
      event.stopPropagation();
      close();
    }
    // 焦點陷阱：Tab 在確認/取消按鈕之間循環
    if (event.key === "Tab" && dialogEl) {
      const focusable = dialogEl.querySelectorAll<HTMLElement>(
        "button:not([disabled])",
      );
      if (focusable.length < 2) return;
      const first = focusable[0];
      const last = focusable[focusable.length - 1];
      if (event.shiftKey && document.activeElement === first) {
        event.preventDefault();
        last.focus();
      } else if (!event.shiftKey && document.activeElement === last) {
        event.preventDefault();
        first.focus();
      }
    }
  }

  // 開啟時：記錄焦點、聚焦對話框按鈕；關閉時：還原焦點
  $effect(() => {
    if (open) {
      previousFocus = document.activeElement as HTMLElement | null;
      tick().then(() => {
        // 一律優先聚焦取消按鈕（最安全選項），不因 danger 而預設聚焦破壞性操作
        if (cancelBtn) {
          cancelBtn.focus();
        } else {
          confirmBtn?.focus();
        }
      });
    } else if (previousFocus) {
      previousFocus.focus();
      previousFocus = null;
    }
  });
</script>

<svelte:window {onkeydown} />

{#if open}
  <!-- svelte-ignore a11y_click_events_have_key_events -->
  <div class="overlay" role="presentation" onclick={close}>
    <!-- svelte-ignore a11y_click_events_have_key_events -->
    <div
      class:danger
      class="dialog"
      role="alertdialog"
      tabindex="-1"
      aria-modal="true"
      aria-labelledby={titleId}
      aria-describedby={descId}
      bind:this={dialogEl}
      onclick={(event) => event.stopPropagation()}
    >
      <div class="icon" aria-hidden="true">
        {#if danger}
          <svg
            viewBox="0 0 24 24"
            width="18"
            height="18"
            fill="none"
            stroke="currentColor"
            stroke-width="2.5"
            stroke-linecap="round"
          >
            <path d="M12 9v4m0 4h.01" />
            <path
              d="M10.29 3.86L1.82 18a2 2 0 001.71 3h16.94a2 2 0 001.71-3L13.71 3.86a2 2 0 00-3.42 0z"
            />
          </svg>
        {:else}
          <svg
            viewBox="0 0 24 24"
            width="18"
            height="18"
            fill="none"
            stroke="currentColor"
            stroke-width="2.5"
            stroke-linecap="round"
          >
            <circle cx="12" cy="12" r="10" />
            <path d="M12 16v-4m0-4h.01" />
          </svg>
        {/if}
      </div>
      <div class="content">
        <h2 id={titleId}>{title}</h2>
        <p id={descId}>{message}</p>
        {#if detail}<p class="detail">{detail}</p>{/if}
        <div class="actions">
          <button bind:this={cancelBtn} onclick={close} disabled={busy}>
            {cancelLabel}
          </button>
          <button
            bind:this={confirmBtn}
            class="primary"
            class:danger
            onclick={onconfirm}
            disabled={busy}
          >
            {confirmLabel}
          </button>
        </div>
      </div>
    </div>
  </div>
{/if}

<style>
  .overlay {
    position: fixed;
    inset: 0;
    z-index: 200;
    display: flex;
    align-items: center;
    justify-content: center;
    padding: var(--space-5);
    background: var(--overlay);
    backdrop-filter: blur(4px);
  }

  .dialog {
    display: flex;
    gap: var(--space-4);
    width: min(440px, 100%);
    max-height: 100%;
    overflow-y: auto;
    padding: var(--space-6);
    background: var(--surface-1);
    border: 1px solid var(--border-default);
    border-radius: var(--radius-xl);
    box-shadow: var(--shadow-lg);
  }

  .dialog.danger {
    border-color: color-mix(in srgb, var(--danger) 40%, var(--border-default));
  }

  .icon {
    display: grid;
    place-items: center;
    flex: 0 0 32px;
    width: 32px;
    height: 32px;
    color: var(--accent);
    background: var(--accent-muted);
    border: 1px solid color-mix(in srgb, var(--accent) 35%, transparent);
    border-radius: 50%;
  }

  .danger .icon {
    color: var(--danger);
    background: var(--danger-muted);
    border-color: color-mix(in srgb, var(--danger) 35%, transparent);
  }

  .content {
    flex: 1;
    min-width: 0;
  }

  h2 {
    margin: 0 0 var(--space-2);
    font-size: 15px;
    font-weight: var(--font-weight-medium);
  }

  p {
    margin: 0;
    line-height: 1.55;
    white-space: pre-line;
    color: var(--text-primary);
  }

  .detail {
    margin-top: var(--space-2);
    color: var(--text-secondary);
    font-size: 12px;
  }

  .actions {
    display: flex;
    flex-wrap: wrap;
    justify-content: flex-end;
    gap: var(--space-3);
    margin-top: var(--space-6);
  }
</style>
