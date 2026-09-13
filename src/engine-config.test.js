const assert = require("node:assert/strict");
const test = require("node:test");

const { resolveSavedEngineConfig } = require("./engine-config.js");

const engines = [
  { id: "upscale-v1", name: "Image (Online)" },
  { id: "upscale-esrgan", name: "Image (Offline)" },
  { id: "video-colab", name: "Video (Colab)" },
];

test("removed engine options are not restored onto the default engine", () => {
  const restored = resolveSavedEngineConfig(engines, {
    engine: "removed-engine",
    suffix: "-imdn",
  });

  assert.equal(restored, null);
});

test("valid engine options are restored with their matching engine", () => {
  const options = { engine: "upscale-esrgan", scale: "4" };
  const restored = resolveSavedEngineConfig(engines, options);

  assert.ok(restored);
  assert.equal(restored.engine, engines[1]);
  assert.equal(restored.options, options);
});

test("legacy Colab ESRGAN settings migrate to the combined Colab engine", () => {
  const restored = resolveSavedEngineConfig(engines, {
    engine: "video-colab-esrgan",
    model: "realesrgan-x2plus",
    scale: "2",
    interpolation: "target",
    target_fps: "59.94",
  });

  assert.ok(restored);
  assert.equal(restored.engine, engines[2]);
  assert.equal(restored.options.engine, "video-colab");
  assert.equal(restored.options.scale, "2");
  assert.equal(restored.options.interpolation, "59.94");
  assert.equal(restored.options.model, undefined);
  assert.equal(restored.options.target_fps, undefined);
});
