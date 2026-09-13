const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const test = require("node:test");

const modulePath = path.join(__dirname, "colab-engine-ui.js");
const ColabEngineUI = fs.existsSync(modulePath) ? require(modulePath) : {};

function requireApi(name) {
  assert.equal(typeof ColabEngineUI[name], "function", `${name} must be implemented`);
  return ColabEngineUI[name];
}

function createFakeButton() {
  const classes = new Set(["hidden"]);
  const attributes = new Map();
  return {
    disabled: false,
    classList: {
      contains: (name) => classes.has(name),
      toggle(name, force) {
        if (force === undefined ? !classes.has(name) : force) classes.add(name);
        else classes.delete(name);
      },
    },
    setAttribute: (name, value) => attributes.set(name, String(value)),
    getAttribute: (name) => attributes.get(name) ?? null,
  };
}

function createMuteHarness() {
  const createMuteControl = requireApi("createMuteControl");
  const button = createFakeButton();
  const saved = [];
  let engineId = "video-colab";
  const control = createMuteControl({
    button,
    getEngineId: () => engineId,
    onChange: (value) => saved.push(value),
  });
  return {
    button,
    control,
    saved,
    setEngine: (value) => {
      engineId = value;
    },
  };
}

test("Colab Start routes to the remote coordinator with input and output", () => {
  const mode = ColabEngineUI.resolveStartRequirement
    ? ColabEngineUI.resolveStartRequirement(
        { id: "video-colab", remote: true },
        true,
        true,
      )
    : undefined;
  assert.equal(mode, "colab-remote");
});

test("local image engines keep using the local batch", () => {
  const mode = ColabEngineUI.resolveStartRequirement
    ? ColabEngineUI.resolveStartRequirement(
        { id: "upscale-esrgan", remote: false },
        true,
        true,
      )
    : undefined;
  assert.equal(mode, "local-batch");
});

test("local batch still requires both input and output", () => {
  assert.equal(
    ColabEngineUI.resolveStartRequirement(
      { id: "upscale-esrgan", remote: false },
      true,
      false,
    ),
    "input-output-required",
  );
});

test("Colab is split into the visible label and superscript mode", () => {
  const parts = ColabEngineUI.splitEngineLabel
    ? ColabEngineUI.splitEngineLabel("Video (Colab)")
    : undefined;
  assert.deepEqual(parts, {
    label: "Video",
    mode: "(Colab)",
  });
});

test("playlist keeps only extensions accepted by the selected engine", () => {
  const files = [
    { path: "C:/input/photo.png", name: "photo.png" },
    { path: "C:/input/clip.MP4", name: "clip.MP4" },
    { path: "C:/input/movie.mkv", name: "movie.mkv" },
  ];
  assert.deepEqual(
    ColabEngineUI.filterCompatibleFiles(files, ["mp4", "mkv"]),
    [files[1], files[2]],
  );
});

test("Colab video exposes MUTE with its saved state", () => {
  assert.deepEqual(
    ColabEngineUI.resolveMuteControl
      ? ColabEngineUI.resolveMuteControl("video-colab", false)
      : undefined,
    { visible: true, pressed: false },
  );
  assert.deepEqual(
    ColabEngineUI.resolveMuteControl
      ? ColabEngineUI.resolveMuteControl("video-colab", true)
      : undefined,
    { visible: true, pressed: true },
  );
});

test("non-Colab engines hide and reset MUTE", () => {
  assert.deepEqual(
    ColabEngineUI.resolveMuteControl
      ? ColabEngineUI.resolveMuteControl("upscale-esrgan", true)
      : undefined,
    { visible: false, pressed: false },
  );
});

test("MUTE toggles its visible state, aria state, and collected option", () => {
  const collectAdvancedOptions = requireApi("collectAdvancedOptions");
  const { button, control, saved } = createMuteHarness();

  control.set(false);
  assert.equal(button.classList.contains("hidden"), false);
  assert.equal(button.getAttribute("aria-pressed"), "false");

  assert.equal(control.toggle(), true);
  assert.equal(button.classList.contains("on"), true);
  assert.equal(button.getAttribute("aria-pressed"), "true");
  assert.deepEqual(collectAdvancedOptions({ mute_audio: control }), {
    mute_audio: true,
  });
  assert.deepEqual(saved, [true]);
});

test("advanced MUTE value survives switching away from and back to Colab", () => {
  const createAdvancedOptionSession = requireApi("createAdvancedOptionSession");
  const session = createAdvancedOptionSession();
  const { button, control, setEngine } = createMuteHarness();

  control.set(true);
  session.capture("video-colab", { mute_audio: control });

  setEngine("upscale-esrgan");
  control.set(false);
  assert.equal(button.classList.contains("hidden"), true);

  setEngine("video-colab");
  control.set(false);
  assert.equal(session.restore("video-colab", { mute_audio: control }), true);
  assert.equal(control.read(), true);
  assert.equal(button.getAttribute("aria-pressed"), "true");
  assert.equal(button.classList.contains("hidden"), false);
});

for (const phase of ["running", "paused"]) {
  test(`MUTE is natively locked while ${phase}`, () => {
    const { button, control, saved } = createMuteHarness();
    control.set(false);

    control.setPhase(phase);

    assert.equal(button.disabled, true);
    assert.equal(button.getAttribute("aria-disabled"), "true");
    assert.equal(control.toggle(), false);
    assert.equal(control.read(), false);
    assert.deepEqual(saved, []);
  });
}

test("MUTE becomes interactive again when the run returns to idle", () => {
  const { button, control, saved } = createMuteHarness();
  control.set(false);
  control.setPhase("running");

  control.setPhase("idle");

  assert.equal(button.disabled, false);
  assert.equal(button.getAttribute("aria-disabled"), "false");
  assert.equal(control.toggle(), true);
  assert.deepEqual(saved, [true]);
});
