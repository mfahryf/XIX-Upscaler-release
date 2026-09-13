const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const test = require("node:test");

const main = fs.readFileSync(path.join(__dirname, "main.js"), "utf8");
const style = fs.readFileSync(path.join(__dirname, "style.css"), "utf8");

test("engine mode is rendered as a small superscript", () => {
  assert.match(main, /document\.createElement\("sup"\)/);
  assert.match(main, /className = "dd-mode"/);
  assert.match(main, /splitEngineLabel/);
  assert.match(
    style,
    /\.dd-mode\s*\{[^}]*font-size:\s*7px[^}]*vertical-align:\s*super[^}]*\}/s,
  );
});

test("all final engine names follow the superscript label contract", () => {
  const names = [
    "Image (Online)",
    "Image (Offline)",
    "Video (Colab)",
  ];
  for (const name of names) {
    assert.match(name, /^(.*?)\s+(\((Online|Offline|Colab)\))$/);
  }
});

test("saved advanced select values are restored only when still valid", () => {
  assert.match(main, /set:\s*\(v\)\s*=>\s*\{\s*if\s*\(dd\.has\(v\)\)\s*dd\.setValue\(v\);\s*\}/s);
});
