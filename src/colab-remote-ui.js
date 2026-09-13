(function (root, factory) {
  const api = factory();
  if (typeof module === "object" && module.exports) module.exports = api;
  root.XIXColabRemoteUI = api;
})(typeof globalThis !== "undefined" ? globalThis : this, function () {
  const PHASE_LABELS = {
    queued: "MENUNGGU ANTREAN", validating: "MEMERIKSA VIDEO",
    authenticating: "HUBUNGKAN GOOGLE DRIVE", uploading: "MENGUNGGAH VIDEO",
    "waiting-for-colab": "JALANKAN RUN ALL DI COLAB", preparing: "MENYIAPKAN GPU DAN MODEL",
    processing: "MEMPROSES VIDEO", paused: "DIJEDA SETELAH CHECKPOINT",
    "runtime-disconnected": "RUNTIME COLAB TERPUTUS — RUN ALL KEMBALI",
    downloading: "MENGUNDUH HASIL", verifying: "MEMVERIFIKASI HASIL",
    completed: "SELESAI", cancelled: "DIBATALKAN", failed: "GAGAL",
  };
  const terminal = (phase) => ["completed", "cancelled", "failed"].includes(phase);

  function normalizePath(value) {
    const path = String(value || "").replace(/\\/g, "/")
      .replace(/^\/\/\?\/UNC\//i, "//").replace(/^\/\/\?\//, "");
    return /^[a-z]:\//i.test(path) || path.startsWith("//") ? path.toLowerCase() : path;
  }

  function reduceColabEvent(uiState, event) {
    if (!event || !Object.hasOwn(PHASE_LABELS, event.phase)) return uiState;
    const rows = (uiState.rows || []).slice();
    const index = rows.findIndex((row) =>
      (event.job_id && row.job_id === event.job_id) ||
      (!row.job_id && normalizePath(row.file) === normalizePath(event.file)));
    const percent = Number.isFinite(event.percent) ? Math.max(0, Math.min(100, event.percent)) : 0;
    const message = event.message || PHASE_LABELS[event.phase];
    if (index >= 0) rows[index] = { ...rows[index], ...event, percent, message };
    const active = rows.filter((row) => !terminal(row.phase));
    return {
      ...uiState, rows, lcd: message,
      percent: rows.length ? rows.reduce((sum, row) => sum + (terminal(row.phase) ? 100 : Math.min(99, row.percent || 0)), 0) / rows.length : percent,
      paused: active.length > 0 && active.every((row) => row.phase === "paused"),
      done: rows.filter((row) => terminal(row.phase)).length,
      failed: rows.filter((row) => row.phase === "failed").length,
    };
  }

  function resolveDriveControl(engineId, connected) {
    const visible = engineId === "video-colab";
    return { visible, connected: visible && !!connected };
  }

  function createDriveControl({ button, menu, invoke, onError = () => {}, onConnected = async () => {} }) {
    const account = menu.querySelector("#drive-account");
    const disconnectButton = menu.querySelector("#drive-disconnect");
    const switchButton = menu.querySelector("#drive-switch");
    let engineId = null;
    let authPending = false;
    let running = false;
    let authGeneration = 0;
    let auth = { connected: false, masked_account: null, busy: false };

    function close() {
      menu.classList.add("hidden");
      button.setAttribute("aria-expanded", "false");
    }
    function sync() {
      const view = resolveDriveControl(engineId, auth.connected);
      button.classList.toggle("hidden", !view.visible);
      button.classList.toggle("on", view.connected);
      button.setAttribute("aria-pressed", String(view.connected));
      account.textContent = view.connected ? auth.masked_account || "AKUN TERHUBUNG" : "";
      for (const control of [button, disconnectButton, switchButton]) {
        control.disabled = running || authPending || !!auth.busy;
        control.setAttribute("aria-disabled", String(control.disabled));
      }
      if (!view.visible || !view.connected || button.disabled) close();
    }
    async function changeAccount(action) {
      if (button.disabled || button.classList.contains("hidden")) return;
      const generation = ++authGeneration;
      authPending = true;
      sync();
      let connectedNow = false;
      try {
        if (action !== "connect") {
          auth = { connected: false, masked_account: null, busy: false };
          sync();
          await invoke("colab_auth_disconnect", {});
        }
        if (action !== "disconnect") {
          const connected = await invoke("colab_auth_connect", {});
          if (generation === authGeneration) {
            auth = connected;
            connectedNow = auth.connected;
          }
        }
      } catch (error) {
        if (generation === authGeneration) onError(error);
      } finally {
        if (generation === authGeneration) authPending = false;
        sync();
      }
      if (connectedNow) await onConnected();
    }
    button.onclick = async () => {
      if (button.disabled || button.classList.contains("hidden")) return;
      if (!auth.connected) return changeAccount("connect");
      const opening = menu.classList.contains("hidden");
      menu.classList.toggle("hidden", !opening);
      button.setAttribute("aria-expanded", String(opening));
    };
    disconnectButton.onclick = () => changeAccount("disconnect");
    switchButton.onclick = () => changeAccount("switch");
    return {
      close,
      isBusy: () => authPending || !!auth.busy,
      setRunning(value) { running = value; sync(); },
      setEngine(value) { engineId = value; sync(); },
      async refresh() {
        if (authPending) return auth;
        const generation = ++authGeneration;
        try {
          const refreshed = await invoke("colab_auth_status", {});
          if (generation === authGeneration) auth = refreshed;
        } catch (error) {
          if (generation === authGeneration) onError(error);
        }
        sync();
        return auth;
      },
    };
  }

  return { normalizePath, reduceColabEvent, resolveDriveControl, createDriveControl };
});
