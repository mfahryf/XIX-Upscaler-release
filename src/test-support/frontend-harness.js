const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

// Browser and Tauri are the external boundaries. Every application script is
// evaluated unchanged, in the order declared by the real HTML; no app is launched.
function element(tag = "div") {
  const attrs = new Map();
  const el = {
    tagName: tag.toUpperCase(), children: [], dataset: {}, style: {}, value: "",
    disabled: false, checked: false, title: "", offsetHeight: 120, parentElement: null,
    setAttribute(key, value) {
      attrs.set(key, String(value));
      if (key === "class") this.className = value;
      if (key === "id") this.id = value;
    },
    getAttribute: (key) => attrs.get(key) ?? null,
    appendChild(child) { child.parentElement = this; this.children.push(child); return child; },
    remove() { this.parentElement.children = this.parentElement.children.filter((x) => x !== this); },
    contains(child) { return child === this || this.children.some((x) => x.contains(child)); },
    querySelectorAll(selector) {
      const matches = (node) => selector.startsWith(".")
        ? node.classList.contains(selector.slice(1).split(":")[0]) && (!selector.includes(":not(.hidden)") || !node.classList.contains("hidden"))
        : selector.startsWith("#") ? node.id === selector.slice(1) : node.tagName === selector.toUpperCase();
      return this.children.flatMap((child) => [...(matches(child) ? [child] : []), ...child.querySelectorAll(selector)]);
    },
    querySelector(selector) { return this.querySelectorAll(selector)[0] || null; },
    closest(selector) { return this.classList.contains(selector.slice(1)) ? this : this.parentElement?.closest(selector); },
    addEventListener(name, fn) { this["on" + name] = fn; },
    removeEventListener(name, fn) { if (this["on" + name] === fn) delete this["on" + name]; },
    scrollIntoView() {}, focus() {},
    async click() { if (!this.disabled) return this.onclick?.({ target: this, stopPropagation() {} }); },
  };
  let text = "";
  let classes = new Set();
  Object.defineProperties(el, {
    className: { get: () => [...classes].join(" "), set: (value) => { classes = new Set(String(value).split(/\s+/)); } },
    textContent: { get: () => text + el.children.map((x) => x.textContent).join(""), set: (value) => { text = String(value); el.children = []; } },
    innerHTML: { set: () => { text = ""; el.children = []; } },
  });
  el.classList = {
    contains: (x) => classes.has(x),
    add: (...xs) => xs.forEach((x) => classes.add(x)),
    remove: (...xs) => xs.forEach((x) => classes.delete(x)),
    toggle(x, force = !classes.has(x)) { if (force) classes.add(x); else classes.delete(x); return force; },
  };
  return el;
}

function documentFromHtml(html) {
  const document = element("document");
  const stack = [document];
  for (const match of html.matchAll(/<\/?([a-z][a-z0-9-]*)\b([^>]*)>/gi)) {
    const [token, tag, attributes] = match;
    if (token.startsWith("</")) { stack.pop(); continue; }
    const node = element(tag);
    for (const attr of attributes.matchAll(/([\w-]+)="([^"]*)"/g)) node.setAttribute(attr[1], attr[2]);
    if (/\bdisabled\b/.test(attributes.replace(/"[^"]*"/g, ""))) node.disabled = true;
    stack.at(-1).appendChild(node);
    if (!["meta", "link", "input", "br", "img"].includes(tag) && !token.endsWith("/>")) stack.push(node);
  }
  document.body = document.querySelector("body");
  document.createElement = element;
  document.getElementById = (id) => document.querySelector("#" + id);
  return document;
}

const engines = [
  { id: "upscale-esrgan", name: "Image (Offline)", remote: false, input_exts: ["png"], options_schema: [] },
  { id: "video-colab", name: "Video (Colab)", remote: true, input_exts: ["mp4", "mkv"], options_schema: [
    { id: "format", label: "Format", kind: { Select: [["mp4", "MP4"]] }, default: "mp4" },
    { id: "scale", label: "Scale", kind: { Select: [["2", "2×"], ["4", "4×"]] }, default: "4" },
    { id: "interpolation", label: "Interpolation", kind: { Select: [["off", "Off"], ["23.976", "23.976"], ["24", "24"], ["25", "25"], ["29.97", "29.97"], ["30", "30"], ["48", "48"], ["50", "50"], ["59.94", "59.94"], ["60", "60"]] }, default: "off" },
    { id: "mute_audio", label: "Mute", kind: "Bool", default: false },
    { id: "keep_drive_files", label: "Simpan file kerja di Drive", kind: "Bool", default: false },
  ] },
];
const flush = () => new Promise((resolve) => setImmediate(resolve));

async function createFrontend({ engine = "video-colab", handlers = {}, snapshots = [] } = {}) {
  const src = path.join(__dirname, "..");
  const html = fs.readFileSync(path.join(src, "index.html"), "utf8");
  const document = documentFromHtml(html);
  const calls = [], listeners = new Map(), intervals = new Set();
  let picked = ["C:/input/clip.mp4"];
  const defaults = {
    get_config: () => ({ last_output: "C:/output", engine_options: { engine } }),
    list_engines: () => engines,
    stat_files: ({ files }) => files.map(() => 1024),
    colab_auth_status: () => ({ connected: false, masked_account: null, busy: false }),
    colab_auth_connect: () => ({ connected: true, masked_account: "f***@example.com", busy: false }),
    resume_colab_jobs: () => snapshots,
  };
  const context = vm.createContext({
    document, console,
    setInterval(fn) { intervals.add(fn); return fn; }, clearInterval(fn) { intervals.delete(fn); },
    window: { __TAURI__: {
      core: { async invoke(command, args = {}) {
        calls.push({ command, args: JSON.parse(JSON.stringify(args)) });
        return (handlers[command] || defaults[command] || (() => null))(args);
      } },
      event: { async listen(name, callback) {
        if (handlers.listen) await handlers.listen(name);
        listeners.set(name, callback);
        return () => listeners.delete(name);
      } },
      window: { getCurrentWindow: () => ({ close() {}, minimize() {} }) },
      dialog: { open: async () => picked },
    } },
  });
  for (const match of html.matchAll(/<script src="([^"]+)"><\/script>/g)) {
    vm.runInContext(fs.readFileSync(path.join(src, match[1]), "utf8"), context, { filename: match[1] });
  }
  await flush();
  return {
    document, calls, intervals,
    $: (id) => document.getElementById(id),
    async emit(name, payload) { await listeners.get(name)?.({ payload }); await flush(); },
    async pick(paths = ["C:/input/clip.mp4"]) { picked = paths; await document.getElementById("btn-pick-files").click(); },
    async chooseEngine(value) {
      const option = document.getElementById("dd-engine").querySelectorAll(".dd-opt").find((x) => x.dataset.val === value);
      await option.click();
    },
  };
}

module.exports = { createFrontend, element, flush };
