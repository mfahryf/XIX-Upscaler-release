const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const test = require("node:test");
const vm = require("node:vm");
const ColabEngineUI = require("./colab-engine-ui.js");

// Captured from serde_json serialization of VideoColabEngine::options_schema().
// Unit variants are strings; variants carrying settings remain objects.
const colabSchema = [
  { id: "format", label: "Format", kind: { Select: [["mp4", "MP4"]] }, default: "mp4" },
  { id: "scale", label: "Scale", kind: { Select: [["2", "2×"], ["4", "4×"]] }, default: "4" },
  { id: "interpolation", label: "Interpolation", kind: { Select: [["off", "Off"], ["23.976", "23.976"], ["24", "24"], ["25", "25"], ["29.97", "29.97"], ["30", "30"], ["48", "48"], ["50", "50"], ["59.94", "59.94"], ["60", "60"]] }, default: "off" },
  { id: "mute_audio", label: "Mute", kind: "Bool", default: false },
  { id: "suffix", label: "Suffix output", kind: "Text", default: "-colab" },
  { id: "keep_drive_files", label: "Simpan file kerja di Drive", kind: "Bool", default: false },
];

function element(tag) {
  return {
    tagName: tag.toUpperCase(),
    children: [],
    value: "",
    checked: false,
    classList: { toggle() {} },
    setAttribute() {},
    appendChild(child) { this.children.push(child); },
  };
}

function createHarness(engineId = "video-colab") {
  const main = fs.readFileSync(path.join(__dirname, "main.js"), "utf8");
  const start = main.indexOf("function renderAdvOpts(schema)");
  const end = main.indexOf("function applyEngineOptions(", start);
  assert.ok(start >= 0 && end > start, "load the actual renderer and collector");
  const box = element("div");
  const state = { engine: { id: engineId }, proxy: { mode: "direct", list: [] } };
  const context = vm.createContext({
    document: { createElement: element },
    $: () => box,
    advFields: {},
    ADV_SKIP_IDS: new Set(["format", "scale", "fit", "proxy_mode", "proxy_list"]),
    state,
    ddFormat: { value: "mp4" },
    ddScale: { value: "4" },
    ddFit: { value: "" },
    muteControl: ColabEngineUI.createMuteControl({
      button: element("button"),
      getEngineId: () => state.engine.id,
    }),
    XIXColabEngineUI: ColabEngineUI,
    updateLcdMeta() {},
    // Only browser controls are simulated; render/read/set logic runs from main.js.
    makeDropdown() {
      return {
        options: [],
        setOptions(options) { this.options = options; },
        has(value) { return this.options.some((option) => option.value === value); },
        setValue(value) { this.value = value; },
      };
    },
  });
  vm.runInContext(main.slice(start, end), context, { filename: "main.js" });
  return {
    context,
    box,
    fields: context.advFields,
    render: (schema) => context.renderAdvOpts(schema),
    // Normalize VM object prototypes, just as the command transport does.
    collect: () => JSON.parse(JSON.stringify(context.collectOptions())),
  };
}

test("Rust Colab Bool/Text options render and reach collectOptions with their defaults", () => {
  const ui = createHarness();
  ui.render(colabSchema);

  assert.deepEqual(ui.collect(), {
    engine: "video-colab", format: "mp4", scale: "4", fit: "",
    proxy_mode: "direct", proxy_list: [],
    interpolation: "off",
    mute_audio: false, suffix: "-colab", keep_drive_files: false,
  });
  assert.equal(ui.fields.keep_drive_files.el.type, "checkbox");
  assert.equal(ui.fields.suffix.el.type, "text");
  assert.ok(ui.box.children.some((row) => row.children.includes(ui.fields.keep_drive_files.el)));
  assert.ok(ui.box.children.some((row) => row.children.includes(ui.fields.suffix.el)));
});

test("Colab checkbox, text and select edits survive collection and saved-value restoration", () => {
  const ui = createHarness();
  ui.render(colabSchema);
  assert.ok(ui.fields.keep_drive_files, "Drive retention checkbox must be rendered");
  assert.ok(ui.fields.suffix, "suffix text input must be rendered");

  ui.fields.keep_drive_files.el.checked = true;
  ui.fields.suffix.el.value = "  -final_v2  ";
  ui.fields.interpolation.set("59.94");
  ui.fields.interpolation.set("unsupported");
  ui.fields.mute_audio.set(true);
  assert.deepEqual(ui.collect(), {
    engine: "video-colab", format: "mp4", scale: "4", fit: "",
    proxy_mode: "direct", proxy_list: [],
    interpolation: "59.94",
    mute_audio: true, suffix: "-final_v2", keep_drive_files: true,
  });

  ui.fields.keep_drive_files.set(false);
  ui.fields.suffix.set("");
  assert.equal(ui.collect().keep_drive_files, false);
  assert.equal(ui.collect().suffix, "");
});

test("image options retain boolean/text defaults, numeric edits and shared controls", () => {
  const ui = createHarness("upscale-v1");
  ui.context.ddFormat.value = "auto";
  ui.context.ddScale.value = "";
  ui.context.ddFit.value = "none";
  ui.render([
    { id: "format", label: "Format", kind: { Select: [["auto", "Auto"]] }, default: "auto" },
    { id: "fit", label: "Fit", kind: { Select: [["none", "None"]] }, default: "none" },
    { id: "skip_existing", label: "Skip existing", kind: "Bool", default: false },
    { id: "suffix", label: "Suffix", kind: "Text", default: "-4K" },
    { id: "use_proxy", label: "Use proxy", kind: "Bool", default: true },
    { id: "batch_delay", label: "Delay", kind: { Number: { min: 0, max: 60, step: 1 } }, default: 3 },
  ]);
  assert.deepEqual(ui.collect(), {
    engine: "upscale-v1", format: "auto", scale: "", fit: "none",
    proxy_mode: "direct", proxy_list: [],
    skip_existing: false, suffix: "-4K", use_proxy: true, batch_delay: 3,
  });
  assert.equal(ui.fields.format, undefined);
  assert.equal(ui.fields.fit, undefined);
  assert.equal(ui.fields.batch_delay.el.type, "range");
  ui.fields.batch_delay.el.value = "7";
  ui.fields.batch_delay.el.oninput();
  assert.equal(ui.collect().batch_delay, 7);
  ui.fields.batch_delay.set(0);
  ui.fields.use_proxy.set(false);
  ui.fields.skip_existing.set(true);
  ui.fields.suffix.set("-custom");
  assert.deepEqual(ui.collect(), {
    engine: "upscale-v1", format: "auto", scale: "", fit: "none",
    proxy_mode: "direct", proxy_list: [],
    skip_existing: true, suffix: "-custom", use_proxy: false, batch_delay: 0,
  });
});
