const assert = require("node:assert/strict");
const test = require("node:test");
const { createFrontend, flush } = require("./test-support/frontend-harness.js");
const connected = { connected: true, masked_account: "f***@example.com", busy: false };
const event = (file, phase, percent = 0, extra = {}) => ({
  job_id: "job-one", file, phase, percent, message: "", is_error: phase === "failed", ...extra,
});

test("disconnect immediately hides account identity while revocation is pending", async () => {
  let finish;
  const ui = await createFrontend({ handlers: {
    colab_auth_status: () => connected,
    colab_auth_disconnect: () => new Promise((resolve) => { finish = resolve; }),
  } });
  await ui.$("btn-drive").click();
  const disconnecting = ui.$("drive-disconnect").click();
  await flush();
  assert.equal(ui.$("drive-account").textContent, "");
  finish(); await disconnecting;
});

test("reconnecting DRIVE automatically resumes a restored pending job", async () => {
  let authenticated = false; let resumed = 0;
  const ui = await createFrontend({ handlers: {
    colab_auth_status: () => ({ connected: authenticated, masked_account: authenticated ? connected.masked_account : null, busy: authenticated }),
    colab_auth_connect: () => { authenticated = true; return { ...connected, busy: false }; },
    resume_colab_jobs: () => { resumed++; return [event("C:/input/clip.mp4", "uploading")]; },
  } });
  await ui.$("btn-drive").click();
  assert.equal(resumed, 2);
  assert.equal(ui.$("btn-start").disabled, true);
  assert.equal(ui.calls.some((c) => c.command === "start_colab_batch"), false);
});

test("late local batch events cannot stop or overwrite an active Colab batch", async () => {
  const ui = await createFrontend();
  await ui.pick();
  await ui.$("btn-start").click();
  await ui.emit("colab://event", event("C:/input/clip.mp4", "uploading", 20));
  await ui.emit("batch://event", { FileFail: { name: "clip.mp4", error: "Old failure" } });
  await ui.emit("batch://done", { ok: 0, fail: 1, total: 1 });
  assert.equal(ui.$("pl-list").children[0].classList.contains("fail"), false);
  assert.equal(ui.$("btn-start").disabled, true);
  assert.equal(ui.$("btn-drive").disabled, true);
  assert.match(ui.$("lcd-status").textContent, /UPLOADING/);
});

test("DRIVE lights up when Start authenticated automatically and uploading begins", async () => {
  let authenticated = false;
  const ui = await createFrontend({ handlers: {
    colab_auth_status: () => authenticated ? { ...connected, busy: true } : { connected: false, masked_account: null, busy: false },
  } });
  await ui.pick();
  await ui.$("btn-start").click();
  authenticated = true;
  await ui.emit("colab://event", event("C:/input/clip.mp4", "uploading", 10));
  assert.equal(ui.$("btn-drive").getAttribute("aria-pressed"), "true");
  assert.equal(ui.$("btn-drive").disabled, true);
});

test("clearing successful Colab rows preserves a failed video with the same filename", async () => {
  const ui = await createFrontend();
  await ui.pick(["C:/input/clip.mp4", "C:/other/clip.mp4"]);
  await ui.$("btn-start").click();
  await ui.emit("colab://event", event("C:/input/clip.mp4", "completed", 100));
  await ui.emit("colab://event", event("C:/other/clip.mp4", "failed", 0, { job_id: "job-two" }));
  await ui.emit("colab://done", { ok: 1, fail: 1, cancelled: 0, total: 2 });
  await ui.$("cm-done").click();
  assert.equal(ui.$("pl-list").children.length, 1);
  assert.equal(ui.$("pl-list").children[0].dataset.path, "c:/other/clip.mp4");
});

test("missing input/output remains actionable and rejected Start leaves controls available", async () => {
  const ui = await createFrontend({ handlers: {
    start_colab_batch: () => { throw new Error("Video exceeds 60 seconds"); },
  } });
  await ui.$("btn-start").click();
  assert.match(ui.$("lcd-status").textContent, /SELECT INPUT FILES/);
  assert.equal(ui.calls.some((x) => x.command === "start_colab_batch"), false);
  await ui.pick();
  await ui.$("btn-start").click();
  assert.match(ui.$("lcd-status").textContent, /60 seconds/);
  assert.equal(ui.$("btn-drive").disabled, false);
  assert.equal(ui.$("btn-start").disabled, false);
  assert.equal(ui.intervals.size, 0);
});

test("active upload keeps its playlist and options stable under picker, drag/drop and engine changes", async () => {
  const ui = await createFrontend();
  await ui.pick();
  await ui.$("btn-start").click();
  await ui.pick(["C:/other/other.mp4"]);
  await ui.emit("tauri://drag-drop", { paths: ["C:/drop/drop.mp4"] });
  await ui.chooseEngine("upscale-esrgan");
  assert.equal(ui.$("pl-list").children.length, 1);
  assert.equal(ui.$("pl-list").children[0].querySelector(".title").textContent, "clip.mp4");
  assert.equal(ui.$("btn-mute").classList.contains("hidden"), false);
  assert.equal(ui.$("adv-panel").inert, true);
  assert.equal(ui.$("btn-start").disabled, true);
});

test("completion during recovery is retained even when the finished job disappears from the snapshot", async () => {
  let returnSnapshot;
  const ui = await createFrontend({ handlers: {
    resume_colab_jobs: () => new Promise((resolve) => { returnSnapshot = resolve; }),
  } });
  await ui.emit("colab://event", event("C:/input/clip.mp4", "completed", 100));
  await ui.emit("colab://done", { ok: 1, fail: 0, cancelled: 0, total: 1 });
  returnSnapshot([]);
  await flush();
  assert.equal(ui.$("pl-list").children.length, 1);
  assert.equal(ui.$("pl-list").children[0].querySelector(".len").textContent, "OK");
  assert.equal(ui.$("pl-status").textContent, "DONE");
  assert.equal(ui.$("lcd-count").textContent, "1/1");
  assert.equal(ui.$("btn-start").classList.contains("disabled"), false);
  assert.equal(ui.intervals.size, 0);
});

test("recovery waits for listeners and newer live progress wins over the returned snapshot", async () => {
  let allowListeners, returnSnapshot;
  const gate = new Promise((resolve) => { allowListeners = resolve; });
  const ui = await createFrontend({ handlers: {
    listen: (name) => name.startsWith("colab://") ? gate : undefined,
    resume_colab_jobs: () => new Promise((resolve) => { returnSnapshot = resolve; }),
    colab_auth_status: () => ({ ...connected, busy: true }),
  } });
  assert.equal(ui.calls.some((x) => x.command === "resume_colab_jobs"), false);
  allowListeners();
  await flush();
  await ui.emit("colab://event", event("C:/input/clip.mp4", "downloading", 80));
  returnSnapshot([event("C:/input/clip.mp4", "downloading", 10)]);
  await flush();
  assert.equal(ui.$("pl-list").children.length, 1);
  assert.equal(ui.$("pl-list").children[0].querySelector(".len").textContent, "80%");
  assert.equal(ui.calls.filter((x) => x.command === "resume_colab_jobs").length, 1);
});

test("missing source files do not create phantom playlist rows or stop a recovered download", async () => {
  const ui = await createFrontend({ snapshots: [event("C:/gone/clip.mp4", "downloading", 70)], handlers: {
    colab_auth_status: () => ({ ...connected, busy: true }), stat_files: () => [0],
  } });
  assert.equal(ui.$("pl-list").children.length, 0);
  assert.equal(ui.document.body.classList.contains("running"), true);
  assert.match(ui.$("lcd-status").textContent, /LOCAL SOURCE NOT FOUND/);
  assert.equal(ui.calls.some((x) => x.command === "stop_colab_batch"), false);
  await ui.emit("colab://event", event("C:/gone/clip.mp4", "completed", 100));
  await ui.emit("colab://done", { ok: 1, fail: 0, cancelled: 0, total: 1 });
  assert.match(ui.$("lcd-status").textContent, /finished 1/);
  assert.equal(ui.$("lcd-count").textContent, "1/1");
});

test("Start retries saved failed uploads through resume while an inactive pending job allows disconnect", async () => {
  let resumes = 0;
  const ui = await createFrontend({ handlers: {
    colab_auth_status: () => connected,
    resume_colab_jobs: () => { resumes++; return [event("C:/input/clip.mp4", "uploading", 0, { message: "Upload interrupted" })]; },
  } });
  assert.match(ui.$("btn-start").title, /LANJUTKAN/);
  assert.equal(ui.$("btn-drive").disabled, false);
  await ui.$("btn-drive").click();
  await ui.$("drive-disconnect").click();
  assert.equal(ui.calls.some((x) => x.command === "colab_auth_disconnect"), true);
  ui.$("cfg-output").value = "";
  await ui.$("btn-start").click();
  assert.equal(resumes, 2);
  assert.equal(ui.calls.some((x) => x.command === "start_colab_batch"), false);
});

test("empty recovery refreshes busy status so the owner can disconnect and start a new batch", async () => {
  let statusCalls = 0;
  const ui = await createFrontend({ handlers: {
    colab_auth_status: () => ({ ...connected, busy: ++statusCalls === 1 }),
    resume_colab_jobs: () => [],
  } });
  assert.equal(ui.$("btn-drive").disabled, false);
  await ui.pick();
  await ui.$("btn-start").click();
  assert.equal(ui.calls.filter((x) => x.command === "start_colab_batch").length, 1);
});

test("Start after a failed finished job starts the selected file again when recovery is empty", async () => {
  const ui = await createFrontend();
  await ui.pick();
  await ui.$("btn-start").click();
  await ui.emit("colab://event", event("C:/input/clip.mp4", "failed"));
  await ui.emit("colab://done", { ok: 0, fail: 1, cancelled: 0, total: 1 });
  await ui.$("btn-start").click();
  const starts = ui.calls.filter((x) => x.command === "start_colab_batch");
  assert.equal(starts.length, 2);
  assert.deepEqual(starts[1].args.files, ["C:/input/clip.mp4"]);
  assert.equal(ui.$("pl-list").children.length, 1);
});

test("recovered jobs for the same source have independent row statuses and clear-finished identity", async () => {
  const ui = await createFrontend({ snapshots: [
    event("C:/input/clip.mp4", "downloading", 20),
    event("C:/input/clip.mp4", "downloading", 50, { job_id: "job-two" }),
  ] });
  await ui.emit("colab://event", event("C:/input/clip.mp4", "completed", 100));
  await ui.emit("colab://event", event("C:/input/clip.mp4", "failed", 0, { job_id: "job-two" }));
  const rows = ui.$("pl-list").children;
  assert.equal(rows[0].querySelector(".len").textContent, "OK");
  assert.equal(rows[1].querySelector(".len").textContent, "ERR");
  await ui.emit("colab://done", { ok: 1, fail: 1, cancelled: 0, total: 2 });
  await ui.$("cm-done").click();
  assert.equal(ui.$("pl-list").children.length, 1);
  assert.equal(ui.$("pl-list").children[0].dataset.jobId, "job-two");
  assert.equal(ui.$("pl-list").children[0].querySelector(".len").textContent, "ERR");
});

test("rerunning a successful source binds progress to the new job instead of the old row identity", async () => {
  const ui = await createFrontend();
  await ui.pick();
  await ui.$("btn-start").click();
  await ui.emit("colab://event", event("C:/input/clip.mp4", "completed", 100));
  await ui.emit("colab://done", { ok: 1, fail: 0, cancelled: 0, total: 1 });
  await ui.$("btn-start").click();
  await ui.emit("colab://event", event("C:/input/clip.mp4", "uploading", 25, { job_id: "new-job" }));
  assert.equal(ui.$("pl-list").children.length, 1);
  assert.equal(ui.$("pl-list").children[0].dataset.jobId, "new-job");
  assert.equal(ui.$("pl-list").children[0].querySelector(".len").textContent, "25%");
});

test("a fresh retry collapses historical attempts for the same source into one selected input", async () => {
  let resumes = 0;
  const ui = await createFrontend({ handlers: { resume_colab_jobs: () => ++resumes === 1 ? [
    event("C:/input/clip.mp4", "downloading"),
    event("C:/input/clip.mp4", "downloading", 0, { job_id: "job-two" }),
  ] : [] } });
  for (const job_id of ["job-one", "job-two"]) {
    await ui.emit("colab://event", event("C:/input/clip.mp4", "failed", 0, { job_id }));
  }
  await ui.emit("colab://done", { ok: 0, fail: 2, cancelled: 0, total: 2 });
  await ui.$("btn-start").click();
  const start = ui.calls.find((x) => x.command === "start_colab_batch");
  assert.ok(start);
  assert.deepEqual(start.args.files, ["C:/input/clip.mp4"]);
  assert.equal(ui.$("pl-count").textContent, "1 file");
});

test("startup restores saved Colab rows after listeners are ready without reuploading or opening login", async () => {
  const ui = await createFrontend({ engine: "upscale-esrgan", snapshots: [
    event("\\\\?\\C:\\input\\clip.mp4", "downloading", 65),
  ], handlers: { colab_auth_status: () => ({ ...connected, busy: true }) } });
  assert.equal(ui.calls.filter((x) => x.command === "resume_colab_jobs").length, 1);
  assert.equal(ui.$("pl-list").children.length, 1);
  assert.equal(ui.$("pl-list").children[0].querySelector(".title").textContent, "clip.mp4");
  assert.equal(ui.$("pl-list").children[0].querySelector(".len").textContent, "65%");
  assert.equal(ui.document.body.classList.contains("running"), true);
  assert.equal(ui.$("btn-drive").disabled, true);
  assert.equal(ui.intervals.size, 1);
  assert.equal(ui.calls.some((x) => ["start_colab_batch", "start_batch", "colab_auth_connect", "open_colab_notebook"].includes(x.command)), false);
  await ui.emit("colab://event", event("C:\\input\\clip.mp4", "completed", 100));
  assert.equal(ui.$("pl-list").children[0].querySelector(".len").textContent, "OK");
});

test("notebook signals use only the fixed backend command and runtime recovery offers reopening", async () => {
  const ui = await createFrontend();
  await ui.pick();
  await ui.$("btn-start").click();
  await ui.emit("colab://open-notebook", { url: "https://untrusted.invalid" });
  assert.deepEqual(ui.calls.find((x) => x.command === "open_colab_notebook"), { command: "open_colab_notebook", args: {} });
  assert.match(ui.$("lcd-status").textContent, /RUN ALL/);
  await ui.emit("colab://event", event("C:/input/clip.mp4", "runtime-disconnected"));
  assert.equal(ui.$("btn-colab-notebook").classList.contains("hidden"), false);
  await ui.$("btn-colab-notebook").click();
  assert.equal(ui.calls.filter((x) => x.command === "open_colab_notebook").length, 2);
});

test("Stop requests remote cancellation and waits for final counts before unlocking the UI", async () => {
  const ui = await createFrontend();
  await ui.pick();
  await ui.$("btn-start").click();
  await ui.$("btn-stop").click();
  assert.deepEqual(ui.calls.find((x) => x.command === "stop_colab_batch")?.args, {});
  assert.equal(ui.$("btn-drive").disabled, true);
  await ui.emit("colab://event", event("C:/input/clip.mp4", "cancelled"));
  assert.equal(ui.$("pl-list").children[0].classList.contains("cancelled"), true);
  await ui.emit("colab://done", { ok: 0, fail: 0, cancelled: 1, total: 1 });
  assert.equal(ui.document.body.classList.contains("running"), false);
  assert.equal(ui.$("btn-drive").disabled, false);
  assert.equal(ui.$("btn-mute").disabled, false);
  assert.equal(ui.intervals.size, 0);
  assert.match(ui.$("pl-status").textContent, /CANCELLED/);
  assert.match(ui.$("lcd-status").textContent, /cancelled 1/);
  assert.equal(ui.$("lcd-count").textContent, "1/1");
  assert.equal(ui.calls.some((x) => x.command === "stop_batch"), false);
});

test("Pause waits for all active workers to confirm a checkpoint before showing paused", async () => {
  const ui = await createFrontend();
  await ui.pick(["C:/input/clip.mp4", "C:/other/clip.mp4"]);
  await ui.$("btn-start").click();
  await ui.$("btn-pause").click();
  assert.deepEqual(ui.calls.find((x) => x.command === "pause_colab_batch")?.args, { paused: true });
  assert.equal(ui.document.body.classList.contains("paused"), false);
  assert.equal(ui.intervals.size, 1);
  assert.match(ui.$("lcd-status").textContent, /CHECKPOINT/);
  await ui.emit("colab://event", event("C:/input/clip.mp4", "paused"));
  assert.equal(ui.document.body.classList.contains("paused"), false);
  await ui.emit("colab://event", event("C:/other/clip.mp4", "paused", 0, { job_id: "job-two" }));
  assert.equal(ui.document.body.classList.contains("paused"), true);
  assert.equal(ui.intervals.size, 0);
  assert.equal(ui.$("btn-drive").disabled, true);
  assert.equal(ui.$("btn-colab-notebook").classList.contains("hidden"), false);
  await ui.$("btn-pause").click();
  assert.deepEqual(ui.calls.filter((x) => x.command === "pause_colab_batch").at(-1).args, { paused: false });
  await ui.emit("colab://event", event("C:/input/clip.mp4", "processing", 10));
  assert.equal(ui.document.body.classList.contains("paused"), false);
  assert.equal(ui.intervals.size, 1);
  assert.equal(ui.calls.some((x) => x.command === "pause_batch"), false);
});

test("a pending Pause can be changed back to Run before every checkpoint confirms", async () => {
  const requests = [];
  const ui = await createFrontend({ handlers: {
    pause_colab_batch: ({ paused }) => new Promise((resolve) => requests.push({ paused, resolve })),
  } });
  await ui.pick();
  await ui.$("btn-start").click();
  void ui.$("btn-pause").click();
  await flush();
  void ui.$("btn-pause").click();
  await flush();
  assert.deepEqual(requests.map((request) => request.paused), [true, false]);
  requests.forEach((request) => request.resolve());
  await flush();
});

test("a stale DRIVE status response cannot replace a newer successful login", async () => {
  let finishOldStatus;
  const ui = await createFrontend({ handlers: {
    colab_auth_status: () => new Promise((resolve) => { finishOldStatus = resolve; }),
    colab_auth_connect: () => connected,
  } });
  await ui.$("btn-drive").click();
  assert.equal(ui.$("btn-drive").getAttribute("aria-pressed"), "true");
  finishOldStatus({ connected: false, masked_account: null, busy: false });
  await flush();
  assert.equal(ui.$("btn-drive").getAttribute("aria-pressed"), "true");
  assert.equal(ui.$("drive-account").textContent, "f***@example.com");
});

test("Start sends selected videos and options to Colab and locks MUTE", async () => {
  const ui = await createFrontend();
  await ui.pick();
  await ui.$("btn-mute").click();
  await ui.$("btn-start").click();
  const call = ui.calls.find((x) => x.command === "start_colab_batch");
  assert.ok(call, "Start must reach the remote coordinator");
  assert.deepEqual(call.args, {
    files: ["C:/input/clip.mp4"], output: "C:/output",
    options: { engine: "video-colab", format: "mp4", scale: "4", fit: null,
      proxy_mode: "direct", proxy_list: [], interpolation: "off", mute_audio: true, keep_drive_files: false },
  });
  assert.equal(ui.calls.some((x) => x.command === "start_batch"), false);
  assert.equal(ui.$("btn-mute").disabled, true);
  assert.equal(ui.intervals.size, 1);
});

test("Colab video exposes 2x and 4x scale without a model selector", async () => {
  const ui = await createFrontend();
  await ui.pick();
  await ui.$("btn-start").click();
  const call = ui.calls.find((x) => x.command === "start_colab_batch");
  assert.ok(call, "Start must reach the Colab coordinator");
  assert.equal(call.args.options.engine, "video-colab");
  assert.equal(call.args.options.scale, "4");
  assert.equal(call.args.options.interpolation, "off");
  assert.equal(call.args.options.model, undefined);
  assert.equal(call.args.options.target_fps, undefined);
  assert.equal(ui.$("adv-opts").querySelectorAll(".cfg").some((row) => row.textContent.includes("MODEL")), false);

  const second = await createFrontend();
  const scaleOption = second.$("dd-scale").querySelectorAll(".dd-opt")
    .find((option) => option.dataset.val === "2");
  assert.ok(scaleOption, "the 2x scale must be selectable");
  await scaleOption.click();
  await second.pick();
  await second.$("btn-start").click();
  const secondCall = second.calls.find((x) => x.command === "start_colab_batch");
  assert.equal(secondCall.args.options.engine, "video-colab");
  assert.equal(secondCall.args.options.scale, "2");
  assert.equal(secondCall.args.options.model, undefined);
});

test("Colab progress matches canonical Windows paths and keeps equal filenames independent", async () => {
  const ui = await createFrontend();
  await ui.pick(["C:/input/clip.mp4", "C:/other/clip.mp4"]);
  await ui.$("btn-start").click();
  await ui.emit("colab://event", event("\\\\?\\C:\\input\\clip.mp4", "uploading", 42));
  const rows = ui.$("pl-list").children;
  assert.equal(rows[0].querySelector(".len").textContent, "42%");
  assert.notEqual(rows[1].querySelector(".len").textContent, "42%");
  assert.match(ui.$("lcd-status").textContent, /UPLOADING/);
  assert.notEqual(ui.$("seek-fill").style.width, "0%");
  await ui.emit("colab://event", event("C:\\other\\clip.mp4", "failed", 0,
    { job_id: "job-two", message: "Drive penuh" }));
  assert.equal(rows[1].classList.contains("fail"), true);
  assert.match(rows[1].title, /Drive penuh/);
  assert.equal(rows[0].classList.contains("fail"), false);
  await ui.emit("colab://event", event("C:\\input\\clip.mp4", "runtime-disconnected", 42));
  assert.match(ui.$("lcd-status").textContent, /RUN ALL AGAIN/);
  assert.equal(ui.document.body.classList.contains("running"), true);
});

test("a temporary Drive outage keeps its retry message instead of asking for Run all", async () => {
  const ui = await createFrontend();
  await ui.pick();
  await ui.$("btn-start").click();
  await ui.emit("colab://event", event("C:/input/clip.mp4", "runtime-disconnected", 42, {
    message: "DRIVE CONNECTION LOST — RETRYING",
  }));
  assert.match(ui.$("lcd-status").textContent, /DRIVE CONNECTION LOST/);
  assert.doesNotMatch(ui.$("lcd-status").textContent, /RUN ALL/);
});

test("an active Colab batch prevents account switching even from an already open menu", async () => {
  const ui = await createFrontend({ handlers: { colab_auth_status: () => connected } });
  await ui.pick();
  await ui.$("btn-drive").click();
  await ui.$("btn-start").click();
  assert.equal(ui.$("btn-drive").disabled, true);
  assert.equal(ui.$("drive-menu").classList.contains("hidden"), true);
  await ui.$("drive-switch").click();
  await ui.$("drive-disconnect").click();
  assert.equal(ui.calls.some((x) => x.command === "colab_auth_disconnect"), false);
});

test("switching Drive accounts clears the old identity and locks auth controls until login ends", async () => {
  let completeLogin;
  const ui = await createFrontend({ handlers: {
    colab_auth_status: () => connected,
    colab_auth_connect: () => new Promise((resolve) => { completeLogin = resolve; }),
  } });
  await ui.$("btn-drive").click();
  const switching = ui.$("drive-switch").click();
  await flush();
  assert.equal(ui.$("btn-drive").disabled, true);
  assert.equal(ui.$("drive-switch").disabled, true);
  assert.equal(ui.$("drive-disconnect").disabled, true);
  assert.equal(ui.$("drive-account").textContent, "");
  await ui.$("btn-start").click();
  assert.equal(ui.calls.some((x) => x.command === "start_colab_batch"), false);
  completeLogin({ connected: true, masked_account: "n***@example.com", busy: false });
  await switching;
  assert.equal(ui.$("btn-drive").disabled, false);
  assert.equal(ui.$("drive-account").textContent, "n***@example.com");
  assert.deepEqual(ui.calls.filter((x) => /colab_auth_(connect|disconnect)/.test(x.command)).map((x) => x.command),
    ["colab_auth_disconnect", "colab_auth_connect"]);
});

test("DRIVE connects, shows only masked account, and disconnects from its menu", async () => {
  const ui = await createFrontend();
  const button = ui.$("btn-drive");
  assert.ok(button, "Colab must expose DRIVE");
  assert.equal(button.classList.contains("hidden"), false);
  assert.equal(button.getAttribute("aria-pressed"), "false");
  await button.click();
  assert.equal(button.getAttribute("aria-pressed"), "true");
  await button.click();
  assert.equal(ui.$("drive-menu").classList.contains("hidden"), false);
  assert.equal(ui.$("drive-account").textContent, "f***@example.com");
  await ui.$("drive-disconnect").click();
  assert.equal(button.getAttribute("aria-pressed"), "false");
  assert.equal(ui.$("drive-account").textContent, "");
  assert.equal(ui.$("drive-menu").classList.contains("hidden"), true);
  assert.deepEqual(ui.calls.filter((x) => /colab_auth_(connect|disconnect)/.test(x.command)).map((x) => x.command),
    ["colab_auth_connect", "colab_auth_disconnect"]);
  await ui.chooseEngine("upscale-esrgan");
  assert.equal(button.classList.contains("hidden"), true);
});
