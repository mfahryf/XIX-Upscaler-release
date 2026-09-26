(function (root, factory) {
  // Assign the global unconditionally: bundlers that inject a CommonJS shim
  // (Vite/Rollup) make `typeof module === "object"` true, so an else-only
  // assignment would leave `window.XixLicensingUI` undefined in the bundle.
  const api = factory();
  if (typeof module === "object" && module.exports) {
    module.exports = api;
  }
  root.XixLicensingUI = api;
})(typeof globalThis === "object" ? globalThis : this, function () {
  const ENGINE_IDS = ["upscale-v1", "upscale-esrgan", "video-colab"];
  const TRIAL_TOTAL_LIMIT = 10;

  function formatLicenseExpiry(value) {
    const timestamp = Number(value);
    if (!Number.isFinite(timestamp) || timestamp <= 0) return null;
    const date = new Date(timestamp * 1000);
    if (Number.isNaN(date.getTime())) return null;
    return new Intl.DateTimeFormat("id-ID", {
      day: "numeric",
      month: "long",
      year: "numeric",
      timeZone: "UTC",
    }).format(date);
  }

  function deriveLicenseView(status) {
    const state = status && status.license_state ? status.license_state : "unavailable";
    const remaining = (status && status.trial_remaining_by_engine) || {};
    const reportedTotal = Number(status && status.trial_remaining);
    const trialRemaining = Number.isFinite(reportedTotal)
      ? Math.max(0, Math.min(TRIAL_TOTAL_LIMIT, reportedTotal))
      : Math.max(
          0,
          Math.min(
            TRIAL_TOTAL_LIMIT,
            Object.values(remaining).reduce((total, value) => total + (Number(value) || 0), 0),
          ),
        );
    const engineCounters = ENGINE_IDS.map((id) => ({
      id,
      remaining: Number.isFinite(Number(remaining[id])) ? Number(remaining[id]) : 0,
    }));
    const hasTrial = trialRemaining > 0;
    const licensed = state === "licensed" || state === "licensed-offline";
    const canProcess = licensed || (state === "trial" && hasTrial);
    const canStart = canProcess || state === "unactivated";
    let badge = "LOCKED";
    let message = "Hubungkan internet untuk memvalidasi lisensi.";
    if (state === "unactivated") {
      badge = "NOT ACTIVATED";
      message = "Process the first file to activate your online trial.";
    } else if (state === "trial") {
      badge = "TRIAL";
      message = hasTrial
        ? `Trial: ${trialRemaining} successful files total.`
        : "Trial exhausted. Activate a license to continue.";
    } else if (state === "licensed") {
      badge = "LICENSED";
      message = "Lisensi aktif.";
    } else if (state === "licensed-offline") {
      badge = "OFFLINE LICENSE";
      message = "Lisensi aktif sementara tanpa koneksi.";
    } else if (state === "subscription-expired" || state === "subscription_expired") {
      badge = "EXPIRED";
      message = "Langganan berakhir. Perbarui lisensi untuk melanjutkan.";
    } else if (state === "provider_inactive" || state === "provider-inactive") {
      badge = "MAYAR INACTIVE";
      message = "Kode lisensi Mayar tidak aktif. Hubungi XIXLabs untuk bantuan.";
    } else if (state === "revoked") {
      badge = "REVOKED";
      message = "Lisensi dicabut. Hubungi admin untuk bantuan.";
    } else if (state === "device-conflict") {
      badge = "DEVICE CONFLICT";
      message = "Lisensi terikat ke perangkat lain. Hubungi admin untuk reset perangkat.";
    } else if (state === "expired-offline") {
      badge = "RECONNECT";
    } else if (state === "device-identity-lost") {
      badge = "RECOVERY";
      const code = status && status.recovery_request_code;
      const contact = status && status.recovery_contact;
      message = code
        ? `Identitas hilang. Kode pemulihan: ${code}. ${contact || "Hubungi admin."}`
        : "Identitas perangkat hilang. Hubungi admin untuk pemulihan.";
    } else if (state === "clock-rollback") {
      badge = "CLOCK CHECK";
      message = "Waktu perangkat mundur. Periksa jam lalu validasi lisensi.";
    } else if (state === "unavailable") {
      badge = "UNAVAILABLE";
      message = "Status lisensi sementara tidak tersedia.";
    }
    const helpMessage = licensed ? "" : message;
    return {
      canProcess,
      canStart,
      showActivation: state !== "unavailable" && !licensed,
      badge,
      message,
      helpMessage,
      trialRemaining,
      expiryText: formatLicenseExpiry(status && status.subscription_expires_at),
      engineCounters,
      deviceState: status && status.device_state ? status.device_state : "unknown",
      recoveryRequestCode: status && status.recovery_request_code
        ? status.recovery_request_code
        : null,
      recoveryContact: status && status.recovery_contact ? status.recovery_contact : null,
      offlineDaysRemaining: status && status.offline_days_remaining != null
        ? status.offline_days_remaining
        : null,
    };
  }

  return { deriveLicenseView, ENGINE_IDS };
});

(function () {
  function boot() {
    const tauri = window.__TAURI__;
    const $ = (id) => document.getElementById(id);
    if (!tauri?.core?.invoke || !$("license-modal-overlay") || !window.XixLicensingUI?.deriveLicenseView) return;
    const { invoke } = tauri.core;
    let pendingUpdate = null;
    let lastTrigger = null;

    function render(status) {
      const view = window.XixLicensingUI.deriveLicenseView(status);
      $("license-badge").textContent = view.badge;
      $("license-status").textContent = view.message;
      $("license-trial-total").textContent = `TOTAL: ${view.trialRemaining}/10`;
      $("license-expiry").textContent = view.expiryText ? `Expires: ${view.expiryText}` : "";
      $("license-expiry").classList.toggle("hidden", !view.expiryText);
      $("license-panel").classList.toggle("license-locked", !view.canProcess);
      $("license-help").textContent = view.canProcess
        ? (view.helpMessage || "One Mayar license is linked to this device.")
        : "Processing is locked. Activate a Mayar license to continue.";
      const start = $("btn-start");
      if (start && !document.body.classList.contains("running")) {
        start.classList.toggle("disabled", !view.canStart);
      }
    }

    async function loadStatus() {
      try { render(await invoke("license_status")); }
      catch { render({ license_state: "unavailable", trial_remaining: 0 }); }
    }
    async function refresh() {
      const button = $("license-refresh");
      button.disabled = true;
      try {
        render(await invoke("refresh_license"));
        $("license-status").textContent = "License status refreshed.";
      } catch { $("license-status").textContent = "Connect to the internet and try again."; }
      finally { button.disabled = false; }
    }
    async function activate() {
      const input = $("license-key");
      const key = input.value.trim();
      if (!key) { $("license-status").textContent = "Enter the license code from Mayar."; input.focus(); return; }
      const button = $("license-activate");
      button.disabled = true;
      try {
        render(await invoke("activate_license", { licenseKey: key }));
        input.value = "";
        $("license-status").textContent = "License activated.";
      } catch (error) { $("license-status").textContent = String(error || "License activation failed."); }
      finally { button.disabled = false; }
    }
    async function buy() {
      const button = $("license-buy");
      button.disabled = true;
      try {
        let url = window.XIX_LICENSE_PURCHASE_URL;
        try {
          const catalogUrl = await invoke("license_purchase_url");
          if (typeof catalogUrl === "string" && catalogUrl.startsWith("https://")) url = catalogUrl;
        } catch {}
        if (!url || !tauri.opener?.openUrl) throw new Error("Purchase link unavailable");
        await tauri.opener.openUrl(url);
      } catch { $("license-status").textContent = "Purchase page is not available right now."; }
      finally { button.disabled = false; }
    }
    async function checkUpdates() {
      try {
        const update = tauri.updater?.check
          ? await tauri.updater.check()
          : await invoke("plugin:updater|check");
        if (!update?.version || !update?.rid) return;
        pendingUpdate = update;
        $("update-message").textContent = `Version ${update.version} is available.`;
        $("update-modal-overlay").classList.remove("hidden");
        $("update-modal-overlay").setAttribute("aria-hidden", "false");
      } catch (error) { console.debug("Update check unavailable", error); }
    }
    function closeLicense() {
      $("license-modal-overlay").classList.add("hidden");
      $("license-modal-overlay").setAttribute("aria-hidden", "true");
      if (lastTrigger) lastTrigger.focus();
    }
    function closeUpdate() {
      pendingUpdate = null;
      $("update-modal-overlay").classList.add("hidden");
      $("update-modal-overlay").setAttribute("aria-hidden", "true");
    }
    async function installUpdate() {
      if (!pendingUpdate) return;
      const button = $("update-install");
      button.disabled = true;
      button.textContent = "Installing…";
      $("update-message").textContent = `Downloading version ${pendingUpdate.version}…`;
      try {
        if (typeof pendingUpdate.downloadAndInstall === "function") {
          await pendingUpdate.downloadAndInstall(undefined, { restartAfterInstall: true });
        } else {
          const channel = tauri.core.Channel ? new tauri.core.Channel() : null;
          if (!channel) throw new Error("Updater channel unavailable");
          await invoke("plugin:updater|download_and_install", {
            onEvent: channel, rid: pendingUpdate.rid, restartAfterInstall: true,
          });
        }
        if (tauri.process?.relaunch) await tauri.process.relaunch();
        else await invoke("plugin:process|restart");
      } catch {
        button.disabled = false;
        button.textContent = "Update now";
        $("update-message").textContent = "The update could not be installed. Try again later.";
      }
    }

    $("btn-license").addEventListener("click", () => {
      lastTrigger = $("btn-license");
      $("license-modal-overlay").classList.remove("hidden");
      $("license-modal-overlay").setAttribute("aria-hidden", "false");
      $("license-key").focus();
    });
    $("license-close").addEventListener("click", closeLicense);
    $("license-refresh").addEventListener("click", refresh);
    $("license-activate").addEventListener("click", activate);
    $("license-key").addEventListener("keydown", (event) => { if (event.key === "Enter") activate(); });
    $("license-buy").addEventListener("click", buy);
    $("license-modal-overlay").addEventListener("mousedown", (event) => {
      if (event.target === $("license-modal-overlay")) closeLicense();
    });
    $("update-later").addEventListener("click", closeUpdate);
    $("update-install").addEventListener("click", installUpdate);
    $("update-modal-overlay").addEventListener("mousedown", (event) => {
      if (event.target === $("update-modal-overlay")) closeUpdate();
    });
    document.addEventListener("keydown", (event) => {
      if (event.key !== "Escape") return;
      if (!$("update-modal-overlay").classList.contains("hidden")) closeUpdate();
      else if (!$("license-modal-overlay").classList.contains("hidden")) closeLicense();
    });
    void loadStatus();
    if (typeof window.setTimeout === "function") window.setTimeout(checkUpdates, 1500);
  }
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", boot, { once: true });
  else boot();
})();
