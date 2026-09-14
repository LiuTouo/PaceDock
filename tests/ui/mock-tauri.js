// Browser-only IPC fixture: never calls native commands or modifies system settings.
export function mockTauri({ theme, language, compact = false }) {
  const callbacks = new Map();
  const listeners = new Map();
  let nextId = 1;
  const target = { coreId: 0, lpIndices: [0, 1] };
  const metrics = { avgFps: 120, p1Low: 100, p01Low: 90, frametimeMadPct: 2,
    spikeRatePct: 1, sampleCount: 3600 };
  const capture = { id: 'capture-1', startedAt: '2026-09-12', gameTitle: 'Example Game',
    durationSecs: 30, metrics };
  const summary = { id: 'session-1', gpuInstanceId: 'gpu-1', gpuName: 'Example GPU',
    startedAt: '2026-09-12', status: 'Completed', captureQuality: { integrityPassed: true },
    quick: { status: 'Consistent', methodVersion: 3, retest: [{ target, metrics, score: 1 }],
      screening: [], seed: 1, candidates: [target], screeningOrder: [0], retestOrder: [0] } };
  const state = { status: compact ? 'Running' : 'Pending', windowLayout: compact ? 'CompactProgress' : 'Normal',
    gpuBusy: false, recoveryRequired: false, sessionId: null, progressPct: 50,
    currentTarget: target, cancelRequested: false, currentPhase: 'Screening', cancelStage: 'requested' };
  const settings = { theme, language, startWithWindows: false, startMinimized: false,
    closeToTray: true, highPrecisionTimer: false, timerExemptPrograms: [], pollIntervalMs: 1000 };
  const emit = (event, payload) => {
    for (const [id, listener] of listeners) {
      if (listener.event === event) callbacks.get(listener.handler)?.({ event, id, payload });
    }
  };
  window.__uiMock = { state, emit, calls: [], blocked: [], release: {} };
  window.__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener: () => {} };
  window.__TAURI_INTERNALS__ = {
    transformCallback(callback) { const id = nextId++; callbacks.set(id, callback); return id; },
    async invoke(cmd, args = {}) {
      window.__uiMock.calls.push(cmd);
      if (window.__uiMock.blocked.includes(cmd)) {
        await new Promise(resolve => { window.__uiMock.release[cmd] = resolve; });
      }
      switch (cmd) {
        case 'plugin:event|listen': { const id = nextId++; listeners.set(id, args); return id; }
        case 'plugin:event|unlisten': listeners.delete(args.eventId); return;
        case 'get_settings': return settings;
        case 'save_settings': Object.assign(settings, args.settings); return;
        case 'get_topology': return { physicalCores: [{ id: 0, lpIndices: [0, 1] }], logicalProcessors: [], totalLp: 2 };
        case 'enumerate_gpus': return [{ instanceId: 'gpu-1', friendlyName: 'Example GPU' }];
        case 'get_core_candidates': return [target];
        case 'get_benchmark_state': return { ...state };
        case 'get_gpu_affinity_policy': return { devicePolicy: { bytes: [0] }, assignmentSetOverride: { present: false } };
        case 'get_quick_schedule': return { estimatedMinSecs: 60, estimatedMaxSecs: 120, candidateCaptures: 3 };
        case 'list_benchmark_sessions': return [summary];
        case 'get_benchmark_session': return { summary };
        case 'list_game_windows': return [{ pid: 123, title: 'Example Game', exeName: 'example.exe' }];
        case 'list_game_captures': return [capture, { ...capture, id: 'capture-2' }];
        case 'list_timer_exempts': return [{ exeName: 'example.exe', pids: [123] }];
        case 'get_timer_global_enabled': return false;
        case 'get_timer_status': return { currentResolutionMs: 1, minIntervalMs: .5, maxIntervalMs: 15.625 };
        case 'get_update_info': return { portable: true, version: '0.3.0' };
        case 'check_portable_update':
          emit('update-state', { status: 'Available', currentVersion: '0.3.0', latestVersion: '0.4.0', progress: null, error: null }); return;
        case 'sample_gpu_interrupts': return { instanceId: 'gpu-1', driver: 'example.sys', cpus: [], graphicsKernelCpus: [], sampleSecs: 3, eventsLost: 0 };
        case 'cancel_benchmark': state.cancelRequested = true; return;
        case 'open_data_folder': return;
        default: throw new Error(`Unexpected UI test IPC: ${cmd}`);
      }
    },
  };
}
