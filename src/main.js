// XIX-Upscaler frontend — Winamp themed batch upscaler.
// Talks to the Rust core only through Tauri commands; no secrets live here.
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const { getCurrentWindow } = window.__TAURI__.window;
const { open } = window.__TAURI__.dialog;

const $ = (id) => document.getElementById(id);
const win = getCurrentWindow();

const APP_PALETTE = "neon";

const state = {
  engine: null,
  engines: [],
  files: [],
  running: false,
  paused: false,
  pauseRequested: null,
  stopping: false,
  recovering: false,
  colabResumeRequired: false,
  colabAuthChecked: false,
  proxy: { mode: "direct", list: [], key: "" },
  bgFx: "none",
  palette: APP_PALETTE,
  errCount: 0,
};

// ---------------- LCD / status helpers ----------------
function setLcd(text) {
  $("lcd-status").textContent = text;
}
function setStatus(text, isErr) {
  setLcd(text);
}
function setSeek(pct) {
  const p = Math.max(0, Math.min(100, Math.round(pct)));
  $("seek-fill").style.width = p + "%";
  $("seek-thumb").style.left = `calc(${p}% - 4px)`;
}
function setEqActive(active) {
  $("eq-vis").classList.toggle("idle", !active);
}

// ---------------- timer proses ----------------
// Berjalan selama proses (START), berhenti saat PAUSE, lanjut saat resume,
// membeku saat selesai/stop — reset ke 0:00 setiap START baru.
const timer = { secs: 0, interval: null };
function renderTimer() {
  const s = timer.secs;
  $("eq-timer").textContent = `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
}
function startTimer(reset) {
  if (reset !== false) {
    timer.secs = 0;
    renderTimer();
  }
  if (timer.interval) clearInterval(timer.interval);
  timer.interval = setInterval(() => {
    timer.secs++;
    renderTimer();
  }, 1000);
}
function pauseTimer() {
  if (timer.interval) {
    clearInterval(timer.interval);
    timer.interval = null;
  }
}

// ---------------- background effects + palettes ----------------
// Lapisan #bg-fx di belakang konten; tombol toolbar playlist men-siklus mode
// efek dengan palet warna tetap milik aplikasi.
const FX_MODES = [
  { id: "none", label: "OFF" },
  { id: "aurora", label: "AURORA" },
  { id: "bokeh", label: "BOKEH" },
  { id: "grain", label: "GRAIN" },
  { id: "star", label: "STARS" },
  { id: "embers", label: "EMBERS" },
  { id: "scanline", label: "CRT" },
  { id: "rain", label: "RAIN" },
  { id: "snow", label: "SNOW" },
  { id: "fireflies", label: "FLIES" },
  { id: "ripples", label: "RIPPLES" },
  { id: "grid", label: "GRID" },
  { id: "orbit", label: "ORBIT" },
];
// Palet: glow1/2/3 dipakai warna partikel JS; CSS var (--glow-*, --base-*,
// --accent-*) di style.css menyesuaikan gradient kartu + aksen.
const PALETTES = [
  { id: "glass", name: "GLASS", glow1: "255,154,213", glow2: "125,211,255", glow3: "185,140,255" },
  { id: "neon", name: "NEON", glow1: "255,78,205", glow2: "0,229,255", glow3: "124,77,255" },
  { id: "sunset", name: "SUNSET", glow1: "255,140,90", glow2: "255,90,140", glow3: "255,200,80" },
  { id: "emerald", name: "EMERALD", glow1: "60,255,170", glow2: "0,210,255", glow3: "140,255,120" },
  { id: "ocean", name: "OCEAN", glow1: "80,160,255", glow2: "0,230,220", glow3: "140,120,255" },
  { id: "gold", name: "GOLD", glow1: "255,200,100", glow2: "255,160,60", glow3: "200,220,255" },
];
function paletteColors() {
  return PALETTES.find((p) => p.id === state.palette) || PALETTES[0];
}
function buildBgFx() {
  const fx = $("bg-fx");
  fx.innerHTML = "";
  const rand = (a, b) => a + Math.random() * (b - a);
  const pal = paletteColors();
  const layer = (cls, html) => {
    const d = document.createElement("div");
    d.className = "fx-layer " + cls;
    d.innerHTML = html;
    fx.appendChild(d);
    return d;
  };
  // aurora — 3 blob (CSS murni, warna var --glow-*)
  layer("fx-aurora", '<div class="blob b1"></div><div class="blob b2"></div><div class="blob b3"></div>');
  // bokeh — titik cahaya acak (warna palet)
  const bokeh = layer("fx-bokeh", "");
  const hues = [pal.glow1, pal.glow2, pal.glow3, "255,255,255"];
  for (let i = 0; i < 12; i++) {
    const d = document.createElement("div");
    d.className = "dot";
    const size = rand(5, 24);
    d.style.cssText = `left:${rand(-4, 100).toFixed(0)}%;top:${rand(0, 96).toFixed(0)}%;width:${size.toFixed(0)}px;height:${size.toFixed(0)}px;background:rgba(${hues[i % hues.length]},${rand(0.05, 0.2).toFixed(2)});animation-duration:${rand(7, 12).toFixed(1)}s;animation-delay:${rand(-12, 0).toFixed(1)}s;`;
    bokeh.appendChild(d);
  }
  // grain — noise + vignette (CSS murni)
  layer("fx-grain", '<div class="noise"></div><div class="vig"></div>');
  // star — bintang berkelip (glow palet)
  const star = layer("fx-star", "");
  for (let i = 0; i < 34; i++) {
    const s = document.createElement("div");
    s.className = "star";
    const size = rand(1, 2.2);
    s.style.cssText = `left:${rand(0, 99).toFixed(0)}%;top:${rand(0, 97).toFixed(0)}%;width:${size.toFixed(1)}px;height:${size.toFixed(1)}px;box-shadow:0 0 ${rand(2, 6).toFixed(0)}px rgba(${pal.glow2},.8);animation-duration:${rand(2.2, 5).toFixed(1)}s;animation-delay:${rand(-5, 0).toFixed(1)}s;`;
    star.appendChild(s);
  }
  // embers — percikan naik (selalu hangat, CSS murni)
  const embers = layer("fx-embers", "");
  for (let i = 0; i < 10; i++) {
    const e = document.createElement("div");
    e.className = "ember";
    const size = rand(2, 4.5);
    e.style.cssText = `left:${rand(4, 94).toFixed(0)}%;width:${size.toFixed(1)}px;height:${size.toFixed(1)}px;--sway:${rand(-22, 22).toFixed(0)}px;animation-duration:${rand(7, 14).toFixed(1)}s;animation-delay:${rand(-14, 0).toFixed(1)}s;`;
    embers.appendChild(e);
  }
  // scanline — CRT (CSS murni)
  layer("fx-scanline", '<div class="lines"></div><div class="band"></div><div class="vig"></div>');
  // rain — hujan tipis
  const rain = layer("fx-rain", "");
  for (let i = 0; i < 22; i++) {
    const d = document.createElement("div");
    d.className = "drop";
    d.style.cssText = `left:${rand(0, 99).toFixed(0)}%;height:${rand(9, 22).toFixed(0)}px;animation-duration:${rand(0.5, 1.1).toFixed(2)}s;animation-delay:${rand(-1.1, 0).toFixed(2)}s;`;
    rain.appendChild(d);
  }
  // snow — salju melayang
  const snow = layer("fx-snow", "");
  for (let i = 0; i < 16; i++) {
    const f = document.createElement("div");
    f.className = "flake";
    const size = rand(2, 3.5);
    f.style.cssText = `left:${rand(0, 99).toFixed(0)}%;width:${size.toFixed(1)}px;height:${size.toFixed(1)}px;--sway:${rand(-22, 22).toFixed(0)}px;animation-duration:${rand(6, 11).toFixed(1)}s;animation-delay:${rand(-11, 0).toFixed(1)}s;`;
    snow.appendChild(f);
  }
  // fireflies — kunang-kunang berkelana (glow palet)
  const flies = layer("fx-fireflies", "");
  for (let i = 0; i < 8; i++) {
    const f = document.createElement("div");
    f.className = "fly";
    const size = rand(2, 4);
    f.style.cssText = `left:${rand(6, 92).toFixed(0)}%;top:${rand(10, 88).toFixed(0)}%;width:${size.toFixed(1)}px;height:${size.toFixed(1)}px;background:rgba(${pal.glow1},.95);box-shadow:0 0 ${rand(6, 12).toFixed(0)}px rgba(${pal.glow1},.8);--wx:${rand(-16, 16).toFixed(0)}px;animation-duration:${rand(4, 8).toFixed(1)}s;animation-delay:${rand(-8, 0).toFixed(1)}s;`;
    flies.appendChild(f);
  }
  // ripples — cincin melebar (warna var --glow-2)
  const ripples = layer("fx-ripples", "");
  for (let i = 0; i < 10; i++) {
    const r = document.createElement("div");
    r.className = "ring";
    const size = rand(12, 42);
    r.style.cssText = `left:${rand(5, 90).toFixed(0)}%;top:${rand(10, 85).toFixed(0)}%;width:${size.toFixed(0)}px;height:${size.toFixed(0)}px;animation-duration:${rand(2.5, 6).toFixed(1)}s;animation-delay:${rand(-6, 0).toFixed(1)}s;`;
    ripples.appendChild(r);
  }
  // grid — synthwave (CSS murni)
  layer("fx-grid", '<div class="grid"></div>');
  // orbit — cincin + partikel mengorbit
  const orbit = layer("fx-orbit", "");
  for (let i = 0; i < 2; i++) {
    const r = document.createElement("div");
    r.className = "ring";
    const size = rand(70, 130);
    r.style.cssText = `left:50%;top:50%;width:${size.toFixed(0)}px;height:${size.toFixed(0)}px;margin-left:${(-size / 2).toFixed(0)}px;margin-top:${(-size / 2).toFixed(0)}px;animation-duration:${rand(8, 16).toFixed(1)}s;animation-direction:${i % 2 ? "reverse" : "normal"};`;
    orbit.appendChild(r);
  }
}
function setBgFx(mode, persist = true) {
  const fx = FX_MODES.find((f) => f.id === mode) || FX_MODES[0];
  state.bgFx = fx.id;
  document.body.classList.remove(...FX_MODES.map((f) => "fx-" + f.id));
  document.body.classList.add("fx-" + fx.id);
  const btn = $("btn-bg-fx");
  btn.title = `Efek latar: ${fx.label.toLowerCase()} — klik untuk ganti`;
  btn.querySelector(".fx-name").textContent = fx.label;
  if (persist) saveConfig();
}
function cycleBgFx() {
  const i = FX_MODES.findIndex((f) => f.id === state.bgFx);
  setBgFx(FX_MODES[(i + 1) % FX_MODES.length].id);
}
function applyAppPalette() {
  const p = PALETTES.find((x) => x.id === APP_PALETTE) || PALETTES[0];
  state.palette = p.id;
  document.body.classList.remove(...PALETTES.map((x) => "pal-" + x.id));
  document.body.classList.add("pal-" + p.id);
  buildBgFx();
  setBgFx(state.bgFx, false);
}

// ---------------- run-state UX ----------------
function setEnabled(el, on) {
  if (!el) return;
  el.classList.toggle("disabled", !on);
  el.disabled = !on;
  el.setAttribute("aria-disabled", String(!on));
}
// Fase UI: idle → running → paused (selesai → kembali idle).
// Saat running/paused, body dapat class `running`/`paused` → CSS meredupkan
// semua kontrol non-eksekusi (picker, ADV, settings, dropdown, playlist).
function setRunState(phase) {
  const body = document.body;
  body.classList.toggle("running", phase === "running" || phase === "paused");
  body.classList.toggle("paused", phase === "paused");
  muteControl.setPhase(phase);
  driveControl.setRunning(phase !== "idle");
  $("adv-panel").inert = phase !== "idle" || state.recovering;
  $("dd-engine").parentElement.inert = phase !== "idle" || state.recovering;
  setEnabled($("btn-start"), phase === "idle");
  $("btn-start").title = state.colabResumeRequired && state.engine?.remote
    ? "LANJUTKAN — pulihkan pekerjaan Colab tersimpan" : "START — mulai proses";
  setEnabled($("btn-pause"), (phase === "running" || phase === "paused") && !state.stopping);
  setEnabled($("btn-stop"), phase === "running" || phase === "paused");
  $("btn-pause").title = (state.pauseRequested ?? (phase === "paused"))
    ? "LANJUTKAN — batalkan jeda atau lanjutkan proses" : "PAUSE — jeda proses";
  setEqActive(phase === "running");
}

// ---------------- custom dropdown ----------------
function renderDropdownLabel(target, text, mode = "") {
  target.textContent = text;
  if (!mode) return;
  const sup = document.createElement("sup");
  sup.className = "dd-mode";
  sup.textContent = mode;
  target.appendChild(sup);
}

function makeDropdown(container, onChange) {
  const head = document.createElement("div");
  head.className = "dd-head";
  const label = document.createElement("span");
  label.className = "dd-label";
  const arrow = document.createElement("span");
  arrow.className = "dd-arrow";
  arrow.textContent = "▾";
  head.appendChild(label);
  head.appendChild(arrow);
  const list = document.createElement("div");
  list.className = "dd-list hidden";
  const api = {
    container,
    value: null,
    onChange,
    setOptions(opts) {
      api.value = opts.length ? opts[0].value : null;
      list.innerHTML = "";
      for (const o of opts) {
        const opt = document.createElement("div");
        opt.className = "dd-opt";
        opt.dataset.label = o.label;
        opt.dataset.mode = o.mode || "";
        renderDropdownLabel(opt, o.label, o.mode);
        opt.dataset.val = o.value;
        opt.onclick = () => {
          if (state.running || state.recovering) return;
          api.setValue(o.value);
          close();
          if (api.onChange) api.onChange(o.value);
        };
        list.appendChild(opt);
      }
      render();
    },
    setValue(v) {
      api.value = v;
      render();
    },
    getLabel() {
      const cur = [...list.children].find((o) => o.dataset.val === api.value);
      return cur ? cur.textContent : "";
    },
    // true jika `v` ada di daftar opsi saat ini (guard restore config agar
    // nilai dari engine lain tidak membuat dropdown kosong).
    has(v) {
      return [...list.children].some((o) => o.dataset.val === v);
    },
  };
  function render() {
    const cur = [...list.children].find((o) => o.dataset.val === api.value);
    renderDropdownLabel(
      label,
      cur ? cur.dataset.label : "",
      cur ? cur.dataset.mode : "",
    );
    for (const o of list.children) {
      o.classList.toggle("selected", o.dataset.val === api.value);
    }
  }
  function close() {
    list.classList.add("hidden");
    document.removeEventListener("mousedown", outside);
  }
  function outside(e) {
    if (!container.contains(e.target)) close();
  }
  head.onclick = (e) => {
    if (state.running || state.recovering) return;
    e.stopPropagation();
    const opening = list.classList.contains("hidden");
    document.querySelectorAll(".dd-list:not(.hidden)").forEach((l) => {
      if (l !== list) l.classList.add("hidden");
    });
    list.classList.toggle("hidden", !opening);
    if (opening) document.addEventListener("mousedown", outside);
  };
  container.appendChild(head);
  container.appendChild(list);
  return api;
}

// ---------------- engines + options ----------------
const ddEngine = makeDropdown($("dd-engine"));
const ddFormat = makeDropdown($("dd-format"), () => {
  updateSvgConverterControls();
  updateLcdMeta();
});
const ddScale = makeDropdown($("dd-scale"), updateLcdMeta);
const ddFit = makeDropdown($("dd-fit"), updateLcdMeta);
const ddProxyMode = makeDropdown($("dd-proxy-mode"));
ddProxyMode.setOptions([
  { value: "direct", label: "Langsung (tanpa proxy)" },
  { value: "user", label: "Pakai list proxy" },
  { value: "tor", label: "Tor (rotasi IP otomatis)" },
  { value: "free", label: "Gratis otomatis (HProxy+ProxyScrape)" },
]);
async function loadEngines() {
  state.engines = await invoke("list_engines");
  if (!state.engines.length) {
    setLcd("NO ENGINE");
    return;
  }
  ddEngine.setOptions(
    state.engines.map((e) => {
      const parts = XIXColabEngineUI.splitEngineLabel(e.name);
      return {
        value: e.id,
        label: parts.label,
        mode: parts.mode,
      };
    }),
  );
  ddEngine.onChange = (v) => selectEngine(v);
  selectEngine(state.engines[0].id); // default = first (v2)
}
function selectEngine(id) {
  if (state.engine) engineAdvSession.capture(state.engine.id, advFields);
  state.engine = state.engines.find((e) => e.id === id) || state.engines[0];
  ddEngine.setValue(state.engine.id);
  driveControl.setEngine(state.engine.id);
  renderOptions(state.engine.options_schema);
  engineAdvSession.restore(state.engine.id, advFields);
  const compatibleFiles = XIXColabEngineUI.filterCompatibleFiles(state.files, engineExts());
  if (compatibleFiles.length !== state.files.length) renderPlaylist(compatibleFiles);
  updateSvgConverterControls();
}
function updateLcdMeta() {
  const fmt = (ddFormat.value || "svg").toUpperCase();
  const fitLabel = ddFit.getLabel() || "—";
  $("lcd-fmt").textContent = fmt;
  $("lcd-fit").textContent = fitLabel;
  const extra = advFields.target_mp
    ? ` [${advFields.target_mp.read()} MP]`
    : ddScale.value
    ? ` [${ddScale.value}×]`
    : "";
  setLcd(`READY — ${state.engine.name.toUpperCase()}${extra}`);
}
function renderOptions(schema) {
  const fmt = schema.find((o) => o.id === "format");
  const scale = schema.find((o) => o.id === "scale");
  const fit = schema.find((o) => o.id === "fit");
  if (fmt && fmt.kind.Select) {
    ddFormat.setOptions(fmt.kind.Select.map(([value, label]) => ({ value, label })));
    ddFormat.setValue(fmt.default || "svg");
    ddFormat.container.classList.remove("disabled");
  } else {
    ddFormat.setOptions([{ value: "svg", label: "SVG" }]); // v1 is SVG-only
    ddFormat.container.classList.add("disabled");
  }
  ddFormat.container.title = fmt ? "Format output" : "SVG only (v1)";
  if (scale && scale.kind.Select) {
    ddScale.setOptions(scale.kind.Select.map(([value, label]) => ({ value, label })));
    ddScale.setValue(scale.default != null ? scale.default : scale.kind.Select[0][0]);
    ddScale.container.classList.remove("hidden");
  } else {
    ddScale.setOptions([]);
    ddScale.container.classList.add("hidden");
  }
  if (fit && fit.kind.Select) {
    ddFit.setOptions(fit.kind.Select.map(([value, label]) => ({ value, label })));
    ddFit.setValue(fit.default || "fit");
    ddFit.container.classList.remove("hidden");
  } else {
    // engine tanpa konsep fit (mis. remove-bg) → sembunyikan dropdown
    ddFit.setOptions([]);
    ddFit.container.classList.add("hidden");
  }
  renderAdvOpts(schema);
}

function updateSvgConverterControls() {
  if (!state.engine || state.engine.id !== "svg-converter") return;
  const delay = advFields.batch_delay;
  if (delay) {
    const row = delay.el.closest(".cfg");
    row.classList.add("hidden");
    delay.el.disabled = true;
  }
}


// Opsi engine yang dirender dinamis dari options_schema (Select/Number/Bool/
// Text) ke panel ADV — menambah engine baru tidak perlu sentuh HTML panel.
const advFields = {}; // id -> { read(), set(v), el }
const ADV_SKIP_IDS = new Set(["format", "scale", "fit", "proxy_mode", "proxy_list"]);
const engineAdvSession = XIXColabEngineUI.createAdvancedOptionSession();
const muteControl = XIXColabEngineUI.createMuteControl({
  button: $("btn-mute"),
  getEngineId: () => (state.engine ? state.engine.id : null),
  onChange: () => saveConfig(),
});
const driveControl = XIXColabRemoteUI.createDriveControl({
  onConnected: async () => { if (state.colabResumeRequired) await restoreColabJobs(); },
  button: $("btn-drive"),
  menu: $("drive-menu"),
  invoke,
  onError: (error) => setStatus("GOOGLE DRIVE: " + error, true),
});
function renderAdvOpts(schema) {
  const box = $("adv-opts");
  box.innerHTML = "";
  for (const id of Object.keys(advFields)) delete advFields[id];
  muteControl.set(false);
  for (const def of schema) {
    if (ADV_SKIP_IDS.has(def.id)) continue;
    if (def.id === "mute_audio") {
      advFields.mute_audio = muteControl;
      advFields.mute_audio.set(def.default);
      continue;
    }
    const row = document.createElement("div");
    const lab = document.createElement("label");
    lab.textContent = def.label.toUpperCase();
    let field = null;
    if (def.kind.Select) {
      const ddBox = document.createElement("div");
      ddBox.className = "dd";
      row.className = "cfg";
      row.appendChild(lab);
      row.appendChild(ddBox);
      const dd = makeDropdown(ddBox);
      dd.setOptions(def.kind.Select.map(([value, label]) => ({ value, label })));
      dd.setValue(def.default != null ? def.default : def.kind.Select[0][0]);
      dd.onChange = updateLcdMeta;
      field = {
        read: () => dd.value,
        set: (v) => {
          if (dd.has(v)) dd.setValue(v);
        },
        el: ddBox,
      };
    } else if (def.kind.Number) {
      const range = document.createElement("input");
      range.type = "range";
      range.min = def.kind.Number.min;
      range.max = def.kind.Number.max;
      range.step = def.kind.Number.step;
      range.value = def.default != null ? def.default : def.kind.Number.min;
      const val = document.createElement("span");
      val.className = "mp-val";
      val.textContent = range.value;
      range.oninput = () => {
        val.textContent = range.value;
        updateLcdMeta();
      };
      row.className = "cfg mp";
      row.appendChild(lab);
      row.appendChild(range);
      row.appendChild(val);
      field = {
        read: () => Number(range.value),
        set: (v) => {
          range.value = v;
          val.textContent = v;
        },
        el: range,
      };
    } else if (def.kind === "Bool") {
      const cb = document.createElement("input");
      cb.type = "checkbox";
      cb.checked = !!def.default;
      row.className = "cfg";
      row.appendChild(lab);
      row.appendChild(cb);
      field = { read: () => cb.checked, set: (v) => (cb.checked = !!v), el: cb };
    } else if (def.kind === "Text") {
      const inp = document.createElement("input");
      inp.type = "text";
      inp.value = def.default != null ? String(def.default) : "";
      inp.spellcheck = false;
      row.className = "cfg";
      row.appendChild(lab);
      row.appendChild(inp);
      field = { read: () => inp.value.trim(), set: (v) => (inp.value = v), el: inp };
    }
    if (!field) continue;
    advFields[def.id] = field;
    box.appendChild(row);
  }
}
function collectOptions() {
  const opts = {
    engine: state.engine ? state.engine.id : null,
    format: ddFormat.value,
    scale: ddScale.value,
    fit: ddFit.value,
    proxy_mode: state.proxy.mode,
    proxy_list: state.proxy.list,
  };
  if (state.proxy.mode === "free") opts.hproxy_api_key = state.proxy.key;
  Object.assign(opts, XIXColabEngineUI.collectAdvancedOptions(advFields));
  return opts;
}
// Terapkan opsi tersimpan (config) ke kontrol yang ada untuk engine aktif.
// Hanya nilai yang masih valid untuk engine aktif yang diterapkan — nilai
// sisa dari engine lain (mis. format "both" milik SVG Converter saat engine
// Remove-BG aktif) diabaikan, bukan membuat dropdown kosong.
function applyEngineOptions(o) {
  if (!o) return;
  if (o.format != null && ddFormat.has(o.format) && ddFormat.value !== o.format)
    ddFormat.setValue(o.format);
  if (o.scale != null && ddScale.has(o.scale) && ddScale.value !== o.scale)
    ddScale.setValue(o.scale);
  if (o.fit != null && ddFit.has(o.fit) && ddFit.value !== o.fit)
    ddFit.setValue(o.fit);
  for (const [id, f] of Object.entries(advFields)) {
    if (id in o && o[id] !== undefined && o[id] !== null) f.set(o[id]);
  }
  updateLcdMeta();
}

// ---------------- playlist ----------------
function renderPlaylist(files, restoring = false) {
  if (!restoring && (state.running || state.recovering)) return;
  const ul = $("pl-list");
  ul.innerHTML = "";
  state.files = files;
  $("pl-count").textContent = `${files.length} file`;
  // total LCD langsung = jumlah file di playlist (0/total)
  updateCount(0, files.length);
  updateErrCount(0);
  for (let i = 0; i < files.length; i++) {
    const f = files[i];
    const li = document.createElement("li");
    li.className = "track";
    li.dataset.name = f.name;
    li.dataset.path = XIXColabRemoteUI.normalizePath(f.path);
    li.dataset.jobId = f.job_id || "";
    const num = document.createElement("span");
    num.className = "num";
    num.textContent = String(i + 1).padStart(2, "0");
    const st = document.createElement("span");
    st.className = "st";
    st.textContent = "·";
    const title = document.createElement("span");
    title.className = "title";
    title.textContent = f.name;
    title.title = f.name;
    const len = document.createElement("span");
    len.className = "len";
    len.textContent = fmtSize(f.size);
    li.appendChild(num);
    li.appendChild(st);
    li.appendChild(title);
    li.appendChild(len);
    li.onclick = () => selectRow(li);
    ul.appendChild(li);
  }
}
function fmtSize(bytes) {
  if (bytes >= 1e6) return (bytes / 1e6).toFixed(1) + " MB";
  if (bytes >= 1e3) return (bytes / 1e3).toFixed(1) + " KB";
  return bytes + " B";
}
function selectRow(li) {
  for (const row of $("pl-list").children) row.classList.remove("selected");
  if (li) li.classList.add("selected");
}
function markRow(name, status, error) {
  const li = [...$("pl-list").children].find((el) => el.dataset.name === name);
  if (!li) return;
  li.classList.remove("done", "fail", "processing");
  const st = li.querySelector(".st");
  const len = li.querySelector(".len");
  if (status === "done") {
    li.classList.add("done");
    st.textContent = "✔";
    len.textContent = "OK";
  } else if (status === "fail") {
    li.classList.add("fail");
    st.textContent = "✘";
    len.textContent = "ERR";
    li.title = error || "";
  } else {
    li.classList.add("processing");
    st.textContent = "⏳";
    len.textContent = "…";
    // autofocus: ikuti file yang sedang diproses (scroll minimal bila
    // sudah terlihat, halus bila harus digeser).
    li.scrollIntoView({ block: "nearest", behavior: "smooth" });
  }
}
function setRowProgress(name, percent) {
  const li = [...$("pl-list").children].find((el) => el.dataset.name === name);
  if (!li) return;
  const len = li.querySelector(".len");
  if (len) len.textContent = `${percent}%`;
}
function updateCount(done, total) {
  $("lcd-count").textContent = `${done}/${total}`;
}
function updateErrCount(n) {
  state.errCount = n;
  const el = $("lcd-err");
  el.textContent = `✘${n}`;
  el.classList.toggle("hidden", n === 0);
}
// Clear playlist: menu di tombol trash punya 2 mode — "semua" (hapus semua
// baris) dan "berhasil" (hapus hanya file yang sukses; yang error tetap di
// playlist supaya bisa diproses ulang).
function clearPlaylist() {
  if (state.running || state.recovering) return;
  $("pl-list").innerHTML = "";
  state.files = [];
  $("pl-count").textContent = "0 file";
  updateCount(0, 0);
  updateErrCount(0);
  setSeek(0);
  closeClearMenu();
}
function clearFinished() {
  if (state.running || state.recovering) return;
  const rowKey = (jobId, path) => jobId ? `job:${jobId}` : `file:${path}`;
  const doneKeys = new Set(
    [...$("pl-list").children]
      .filter((li) => li.classList.contains("done"))
      .map((li) => rowKey(li.dataset.jobId, li.dataset.path))
  );
  if (!doneKeys.size) {
    closeClearMenu();
    return;
  }
  for (const li of [...$("pl-list").children]) {
    if (doneKeys.has(rowKey(li.dataset.jobId, li.dataset.path))) li.remove();
  }
  state.files = state.files.filter((f) => !doneKeys.has(rowKey(f.job_id, XIXColabRemoteUI.normalizePath(f.path))));
  $("pl-count").textContent = `${state.files.length} file`;
  // total = sisa (failed + belum diproses); angka baris tetap (tidak reset ke 1)
  updateCount(0, state.files.length);
  updateErrCount(0);
  closeClearMenu();
}
function toggleClearMenu() {
  const menu = $("clear-menu");
  const opening = menu.classList.contains("hidden");
  document.querySelectorAll(".dd-list:not(.hidden)").forEach((l) => l.classList.add("hidden"));
  if (opening) {
    const n = [...$("pl-list").children].filter((li) => li.classList.contains("done")).length;
    $("cm-done").textContent = n ? `Hapus yang berhasil (${n})` : "Hapus yang berhasil";
    menu.classList.remove("hidden");
    document.addEventListener("mousedown", clearMenuOutside);
  } else {
    closeClearMenu();
  }
}
function closeClearMenu() {
  $("clear-menu").classList.add("hidden");
  document.removeEventListener("mousedown", clearMenuOutside);
}
function clearMenuOutside(e) {
  if (!$("clear-menu").contains(e.target)) closeClearMenu();
}
// ---------------- batch ----------------
let colabUi = { rows: [], percent: 0, lcd: "", paused: false };
let colabMissingPaths = new Set();
let colabEventBuffer = [];
let colabDoneBuffer = null;
let colabReady = false;

async function restoreColabJobs() {
  if (state.recovering) return;
  state.recovering = true;
  driveControl.setRunning(true);
  try {
    await colabListenersReady;
    const snapshots = await invoke("resume_colab_jobs", {});
    state.colabResumeRequired = snapshots.some((event) => !["completed", "cancelled", "failed"].includes(event.phase));
    const records = [...new Map([...snapshots, ...colabEventBuffer].map((event) =>
      [event.job_id || XIXColabRemoteUI.normalizePath(event.file), event])).values()];
    if (records.length) {
      selectEngine(records.find((event) => event.engine)?.engine || "video-colab");
      state.batchMode = "colab-remote";
      const files = (await withSizes(records.map((event) => event.file)))
        .map((file, index) => ({ ...file, job_id: records[index].job_id }));
      colabMissingPaths = new Set(files.filter((file) => !file.size).map((file) => XIXColabRemoteUI.normalizePath(file.path)));
      renderPlaylist(files.filter((file) => file.size > 0), true);
      colabUi = { rows: records.map((event) => ({ file: event.file, job_id: event.job_id, phase: "queued" })), percent: 0, lcd: "", paused: false };
      for (const event of records) colabUi = XIXColabRemoteUI.reduceColabEvent(colabUi, event);
      const auth = await driveControl.refresh();
      for (const event of colabEventBuffer) colabUi = XIXColabRemoteUI.reduceColabEvent(colabUi, event);
      colabEventBuffer = [];
      state.running = !!auth.busy;
      state.paused = state.running && colabUi.paused;
      setRunState(state.running ? state.paused ? "paused" : "running" : "idle");
      if (state.running && !state.paused) startTimer(false);
      renderColabProgress();
    } else {
      await driveControl.refresh();
    }
  } catch (error) {
    state.colabResumeRequired = true;
    await driveControl.refresh();
    setStatus("PEMULIHAN COLAB GAGAL: " + error, true);
  } finally {
    colabReady = true;
    state.recovering = false;
    driveControl.setRunning(state.running);
    setRunState(state.running ? state.paused ? "paused" : "running" : "idle");
    if (colabDoneBuffer) {
      const done = colabDoneBuffer;
      colabDoneBuffer = null;
      await onColabDone(done);
    }
  }
}

function renderColabProgress() {
  $("btn-colab-notebook").classList.toggle("hidden", !colabUi.rows.some((row) =>
    row.phase === "runtime-disconnected" || row.phase === "waiting-for-colab" || row.phase === "paused"));
  for (const row of colabUi.rows) {
    const li = [...$("pl-list").children].find((el) =>
      (row.job_id && el.dataset.jobId === row.job_id) ||
      (!el.dataset.jobId && el.dataset.path === XIXColabRemoteUI.normalizePath(row.file)));
    if (!li) continue;
    li.dataset.jobId = row.job_id || "";
    // Keep the selected file's identity aligned for clear-finished and retries.
    const file = state.files[[...$("pl-list").children].indexOf(li)];
    if (file) file.job_id = row.job_id;
    const done = row.phase === "completed";
    const failed = row.phase === "failed";
    const cancelled = row.phase === "cancelled";
    const paused = row.phase === "paused";
    li.classList.toggle("done", done);
    li.classList.toggle("fail", failed);
    li.classList.toggle("cancelled", cancelled);
    li.classList.toggle("processing", !done && !failed && !cancelled);
    li.querySelector(".st").textContent = done ? "✔" : failed ? "✘" : cancelled ? "■" : paused ? "Ⅱ" : "⏳";
    li.querySelector(".len").textContent = done ? "OK" : failed ? "ERR" : cancelled ? "BATAL" : paused ? "JEDA" : `${Math.round(row.percent || 0)}%`;
    li.title = row.message || "";
  }
  const missingSource = colabUi.rows.some((row) => colabMissingPaths.has(XIXColabRemoteUI.normalizePath(row.file)) && !["completed", "cancelled", "failed"].includes(row.phase));
  setLcd(missingSource ? "SUMBER LOKAL TIDAK DITEMUKAN — " + colabUi.lcd : colabUi.lcd);
  setSeek(colabUi.percent);
  updateCount(colabUi.done || 0, colabUi.rows.length);
  updateErrCount(colabUi.failed || 0);
}

function onColabEvent(event) {
  if (!colabReady || state.recovering) {
    colabEventBuffer.push(event);
    return;
  }
  if (state.batchMode !== "colab-remote") return;
  if (!state.colabAuthChecked && !["validating", "authenticating"].includes(event?.phase)) {
    state.colabAuthChecked = true;
    driveControl.refresh();
  }
  colabUi = XIXColabRemoteUI.reduceColabEvent(colabUi, event);
  const wasPaused = state.paused;
  state.paused = colabUi.paused;
  if (state.pauseRequested === state.paused) state.pauseRequested = null;
  if (state.running) {
    setRunState(state.paused ? "paused" : "running");
    if (state.paused) pauseTimer();
    else if (wasPaused) startTimer(false);
  }
  renderColabProgress();
}

async function start() {
  if (state.running || state.recovering || driveControl.isBusy()) return;
  if (state.engine?.remote && state.colabResumeRequired) {
    await restoreColabJobs();
    if (state.colabResumeRequired || state.running || state.recovering || driveControl.isBusy()) return;
  }
  const output = $("cfg-output").value.trim();
  const requirement = XIXColabEngineUI.resolveStartRequirement(
    state.engine,
    state.files.length > 0,
    output.length > 0,
  );
  if (requirement === "input-output-required") {
    setStatus("PILIH FILE / FOLDER INPUT & ISI OUTPUT", true);
    return;
  }
  state.running = true;
  state.batchMode = requirement;
  if (requirement === "colab-remote") {
    state.colabAuthChecked = false;
    colabMissingPaths = new Set();
    // A new run processes selected inputs, not the identities of earlier attempts.
    const inputs = [...new Map(state.files.map((file) =>
      [XIXColabRemoteUI.normalizePath(file.path), file])).values()]
      .map(({ job_id, ...file }) => file);
    renderPlaylist(inputs, true);
    colabUi = { rows: state.files.map((f) => ({ file: f.path, phase: "queued", percent: 0 })), percent: 0, lcd: "MEMERIKSA VIDEO", paused: false };
  }
  state.paused = false;
  state.pauseRequested = null;
  state.stopping = false;
  updateErrCount(0); // hitungan error baru per sesi proses
  setRunState("running");
  setLcd("PROCESSING");
  setSeek(0);
  startTimer();
  saveConfig();
  try {
    const request = {
      files: state.files.map((f) => f.path),
      output,
      options: collectOptions(),
    };
    if (requirement === "colab-remote") {
      await invoke("start_colab_batch", request);
    } else {
      await invoke("start_batch", { ...request, engineId: state.engine.id });
    }
  } catch (e) {
    stopBatchUI("ERROR: " + e, true);
  }
}
async function stop() {
  if (!state.running || state.stopping) return;
  if (state.batchMode === "colab-remote") {
    state.stopping = true;
    setRunState(state.paused ? "paused" : "running");
    setLcd("MEMINTA PEMBATALAN COLAB");
    try { await invoke("stop_colab_batch", {}); }
    catch (error) {
      state.stopping = false;
      setRunState(state.paused ? "paused" : "running");
      setStatus("PEMBATALAN GAGAL: " + error, true);
    }
    return;
  }
  invoke("stop_batch");
  setLcd("STOPPING...");
}
async function togglePause() {
  if (!state.running) return;
  if (state.batchMode === "colab-remote") {
    if (state.stopping) return;
    state.pauseRequested = state.pauseRequested === null ? !state.paused : !state.pauseRequested;
    const requested = state.pauseRequested;
    const generation = (state.pauseGeneration || 0) + 1;
    state.pauseGeneration = generation;
    setRunState(state.paused ? "paused" : "running");
    setLcd(requested ? "MENUNGGU CHECKPOINT UNTUK JEDA" : "JALANKAN RUN ALL KEMBALI DI COLAB");
    try {
      await invoke("pause_colab_batch", { paused: requested });
    } catch (error) {
      if (generation === state.pauseGeneration) {
        state.pauseRequested = null;
        setRunState(state.paused ? "paused" : "running");
        setStatus("PERMINTAAN JEDA GAGAL: " + error, true);
      }
    }
    return;
  }
  state.paused = !state.paused;
  invoke("pause_batch", { paused: state.paused });
  if (state.paused) pauseTimer();
  else startTimer(false);
  setRunState(state.paused ? "paused" : "running");
  setLcd(state.paused ? "PAUSED" : "PROCESSING");
}
function stopBatchUI(status, isErr) {
  state.running = false;
  state.paused = false;
  state.pauseRequested = null;
  state.stopping = false;
  setRunState("idle");
  pauseTimer(); // membeku — total waktu proses tetap terlihat
  setStatus(status, isErr);
}

// ---------------- window controls ----------------
$("btn-close").onclick = () => win.close();
$("btn-min").onclick = () => win.minimize();
$("btn-output-dir").onclick = () => {
  const out = $("cfg-output").value.trim();
  if (out) invoke("open_dir", { path: out });
};

// ---------------- folder browse ----------------
async function pickFolder(inputId) {
  if (state.running || state.recovering) return;
  const res = await open({ directory: true, multiple: false });
  if (state.running || state.recovering) return;
  if (typeof res === "string") {
    $(inputId).value = res;
    saveConfig();
  }
}
function engineExts() {
  return (state.engine && state.engine.input_exts) || ["jpg", "jpeg", "png", "webp"];
}
function isEngineFile(path) {
  return XIXColabEngineUI.filterCompatibleFiles([path], engineExts()).length === 1;
}
async function pickFolderInput() {
  if (state.running || state.recovering) return;
  const res = await open({ directory: true, multiple: false });
  if (typeof res !== "string") return;
  try {
    renderPlaylist(
      await invoke("scan_dir", { path: res.replace(/\\/g, "/"), exts: engineExts() })
    );
    saveConfig();
  } catch {
    /* folder not readable */
  }
}
function fileInfoFromPath(path) {
  const normalized = String(path).replace(/\\/g, "/");
  const name = normalized.slice(normalized.lastIndexOf("/") + 1);
  const ext = name.includes(".") ? name.slice(name.lastIndexOf(".") + 1).toLowerCase() : "";
  return { path, name, size: 0, ext };
}
async function withSizes(paths) {
  let sizes = [];
  try {
    sizes = await invoke("stat_files", { files: paths });
  } catch {
    /* sizes stay 0 */
  }
  return paths.map((p, i) => ({ ...fileInfoFromPath(p), size: sizes[i] || 0 }));
}
async function pickFiles() {
  if (state.running || state.recovering) return;
  const res = await open({
    multiple: true,
    filters: [{ name: "File", extensions: engineExts() }],
  });
  if (!res || !res.length) return;
  const paths = res.map((p) => String(p).replace(/\\/g, "/"));
  renderPlaylist(await withSizes(paths));
  saveConfig();
}
$("btn-output").onclick = () => pickFolder("cfg-output");
$("btn-pick-files").onclick = pickFiles;
$("btn-pick-folder").onclick = pickFolderInput;


// ---------------- drag & drop files ----------------
document.addEventListener("dragover", (e) => e.preventDefault());
listen("tauri://drag-drop", (e) => {
  if (state.running || state.recovering) return;
  const paths = (e.payload && e.payload.paths) || [];
  if (!paths.length) return;
  const norm = paths.map((p) => String(p).replace(/\\/g, "/"));
  if (norm.length === 1 && !isEngineFile(norm[0])) {
    // folder drop → scan it (hanya ekstensi engine aktif)
    invoke("scan_dir", { path: norm[0], exts: engineExts() })
      .then(renderPlaylist)
      .catch(() => {});
  } else {
    // file drop(s) → abaikan format yang tidak didukung engine aktif
    const compatible = XIXColabEngineUI.filterCompatibleFiles(norm, engineExts());
    if (compatible.length) withSizes(compatible).then(renderPlaylist);
  }
});

// ---------------- ADV / PL toggles ----------------
const SPLIT_MIN = 90;
$("btn-adv").onclick = () => {
  const panel = $("adv-panel");
  const opening = panel.classList.contains("hidden");
  panel.classList.toggle("hidden");
  $("btn-adv").classList.toggle("on", opening);
  if (opening) {
    // shrink playlist so the panel fits (window height is fixed)
    const advH = panel.offsetHeight;
    const pl = $("playlist");
    const newH = Math.max(SPLIT_MIN, pl.offsetHeight - advH);
    pl.style.height = newH + "px";
  }
};
$("btn-mute").onclick = () => {
  muteControl.toggle();
};

// ---------------- settings modal (proxy) ----------------
function openModal() {
  ddProxyMode.setValue(state.proxy.mode);
  $("proxy-list").value = state.proxy.list.join("\n");
  $("hproxy-key").value = state.proxy.key;
  $("modal-overlay").classList.remove("hidden");
}
function closeModal() {
  $("modal-overlay").classList.add("hidden");
}
$("proxy-close").onclick = closeModal;
$("modal-overlay").addEventListener("mousedown", (e) => {
  if (e.target === $("modal-overlay")) closeModal();
});
$("proxy-save").onclick = () => {
  state.proxy.mode = ddProxyMode.value;
  state.proxy.list = $("proxy-list").value
    .split("\n")
    .map((s) => s.trim())
    .filter(Boolean);
  closeModal();
  state.proxy.key = $("hproxy-key").value.trim();
  saveConfig();
};
$("btn-settings").onclick = openModal;

// ---------------- config persistence ----------------
async function saveConfig() {
  try {
    await invoke("save_config", {
      cfg: {
        last_output: $("cfg-output").value.trim() || null,
        engine_options: collectOptions(),
        bg_fx: state.bgFx,
        palette: APP_PALETTE,
        proxy_mode: state.proxy.mode,
        proxy_list: state.proxy.list,
        hproxy_api_key: state.proxy.key,
      },
    });
  } catch {
    /* non-fatal */
  }
}

// ---------------- batch events ----------------
async function openColabNotebook() {
  try {
    await invoke("open_colab_notebook", {});
    setLcd("JALANKAN RUN ALL DI COLAB");
  } catch (error) {
    $("btn-colab-notebook").classList.remove("hidden");
    setStatus("BUKA COLAB KEMBALI: " + error, true);
  }
}
$("btn-colab-notebook").onclick = openColabNotebook;
async function onColabDone(payload) {
  if (!colabReady || state.recovering) {
    colabDoneBuffer = payload;
    return;
  }
  if (state.batchMode !== "colab-remote") return;
  const { ok, fail, cancelled, total } = payload;
  state.colabResumeRequired = fail > 0;
  stopBatchUI(`selesai ${ok} · gagal ${fail} · dibatalkan ${cancelled} / ${total}`, fail > 0);
  updateCount(total, total);
  updateErrCount(fail);
  setSeek(100);
  $("btn-colab-notebook").classList.add("hidden");
  $("pl-status").textContent = fail ? `SELESAI (${fail} GAGAL)` : cancelled ? `DIBATALKAN (${cancelled})` : "DONE";
  $("pl-status").className = fail ? "err" : "ok";
  await driveControl.refresh();
}
const colabListenersReady = Promise.all([
  listen("colab://open-notebook", openColabNotebook),
  listen("colab://event", (e) => onColabEvent(e.payload)),
  listen("colab://done", (e) => onColabDone(e.payload)),
]);
listen("tor://status", (e) => {
  const { state: torState, message } = e.payload || {};
  if (torState === "starting") {
    setLcd("TOR STARTING");
    setEqActive(true);
  } else if (torState === "ready") {
    setLcd("TOR READY");
  } else if (torState === "error") {
    setLcd(message || "TOR ERROR");
    setEqActive(false);
  }
});
listen("batch://event", (e) => {
  if (state.batchMode !== "local-batch") return;
  const ev = e.payload;
  if (ev.FileStart) {
    markRow(ev.FileStart.name, "processing");
    // LCD: file ke-(index+1) sedang diproses; slider: hanya file yang SUDAH
    // selesai (index/total) — file yang baru mulai tidak dihitung, jadi
    // list 1 file tidak langsung full.
    updateCount(ev.FileStart.index + 1, ev.FileStart.total);
    setSeek(ev.FileStart.total ? (ev.FileStart.index / ev.FileStart.total) * 100 : 0);
    setLcd("PROCESSING");
    setEqActive(true);
  } else if (ev.FileProgress) {
    setRowProgress(ev.FileProgress.name, ev.FileProgress.percent);
    setLcd(`PROCESSING ${ev.FileProgress.percent}%`);
  } else if (ev.FileDone) {
    markRow(ev.FileDone.name, "done");
    setLcd("OK");
  } else if (ev.FileFail) {
    markRow(ev.FileFail.name, "fail", ev.FileFail.error);
    updateErrCount(state.errCount + 1);
    setLcd("FAIL");
  }
});
listen("batch://done", (e) => {
  if (state.batchMode !== "local-batch") return;
  const { ok, fail, total } = e.payload;
  setLcd("SELESAI");
  stopBatchUI(`selesai ${ok} · gagal ${fail} / ${total}`, fail > 0);
  updateCount(total, total);
  setSeek(100);
  const st = $("pl-status");
  st.textContent = fail > 0 ? `SELESAI (${fail} GAGAL)` : "DONE";
  st.className = fail > 0 ? "err" : "ok";
  saveConfig();
});

// ---------------- init ----------------
(async () => {
  let cfg = null;
  try {
    cfg = await invoke("get_config");
  } catch {
    /* defaults */
  }
  if (cfg) {
    if (cfg.last_output) $("cfg-output").value = cfg.last_output;
    if (cfg.proxy_mode) state.proxy.mode = cfg.proxy_mode;
    if (Array.isArray(cfg.proxy_list)) state.proxy.list = cfg.proxy_list;
    if (typeof cfg.hproxy_api_key === "string") state.proxy.key = cfg.hproxy_api_key;
    if (cfg.bg_fx) state.bgFx = cfg.bg_fx;
  }
  applyAppPalette();
  await loadEngines();
  // restore engine terakhir yang dipilih (loadEngines default ke engine
  // pertama; kalau config punya engine valid, pilih itu dulu).
  const savedEngineConfig = XIXEngineConfig.resolveSavedEngineConfig(
    state.engines,
    cfg && cfg.engine_options,
  );
  if (savedEngineConfig) {
    selectEngine(savedEngineConfig.engine.id);
    // Restore opsi hanya jika engine tersimpan masih tersedia. Opsi engine
    // yang sudah dihapus tidak boleh diterapkan ke engine default.
    applyEngineOptions(savedEngineConfig.options);
  }
  $("btn-start").onclick = start;
  $("btn-pause").onclick = togglePause;
  $("btn-stop").onclick = stop;
  $("btn-remove").onclick = toggleClearMenu;
  $("cm-all").onclick = clearPlaylist;
  $("cm-done").onclick = clearFinished;
  $("btn-bg-fx").onclick = cycleBgFx;
  setRunState("idle");
  setSeek(0);
  await driveControl.refresh();
  await restoreColabJobs();
})();
