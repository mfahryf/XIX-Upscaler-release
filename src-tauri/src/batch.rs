//! Serial batch runner — one file at a time (safe for third-party rate
//! limits), emitting a `BatchEvent` per file so the UI can update the
//! playlist live. Cancellation is cooperative: checked between files.

use crate::engines::process_control::ProcessControl;
use crate::engines::{is_http_403, Engine, EngineError, EngineOptions};
use parking_lot::Mutex;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

const IMAGE_EXTS: &[&str] = &["jpg", "jpeg", "png", "webp"];

pub fn is_image_path(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| IMAGE_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

#[derive(Clone, Debug, Serialize)]
pub enum BatchEvent {
    FileStart {
        index: usize,
        total: usize,
        name: String,
    },
    FileDone {
        name: String,
        output: String,
    },
    FileFail {
        name: String,
        error: String,
    },
    FileProgress {
        name: String,
        percent: u8,
    },
}

/// Jeda default antar file (detik) — menghormati rate limit server yang
/// mem-flag IP setelah request beruntun (403). Bisa diatur per-engine via
/// opsi ADV `batch_delay`.
const DEFAULT_BATCH_DELAY_SECS: f64 = 3.0;
/// Backoff sebelum retry saat 403 (server memblokir IP sementara).
const RETRY_403_BACKOFF_SECS: f64 = 30.0;

/// Konkurensi worker pool: clamp 1..=8 (default 3). Nilai dari opsi ADV
/// `concurrency`; missing/invalid → default. `1` = jalur serial hari ini.
/// Dibaca sebagai f64 lalu dibulatkan (serde Number 5.0 tetap terbaca).
pub fn clamp_concurrency(v: Option<f64>) -> usize {
    let n = v.unwrap_or(3.0).round();
    if n.is_nan() {
        return 3;
    }
    n.clamp(1.0, 8.0) as usize
}

/// Sleep terpotong kecil-kecil supaya cancel dihormati.
async fn sleep_interruptible(secs: f64, cancel: &AtomicBool) {
    let mut remaining = secs;
    while remaining > 0.0 {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let step = remaining.min(0.5);
        tokio::time::sleep(std::time::Duration::from_millis((step * 1000.0) as u64)).await;
        remaining -= step;
    }
}

/// Worker memegang engine miliknya sendiri; `Box<dyn Engine>` ikut
/// mengimplementasikan `Engine` (delegasi) supaya factory lib.rs bisa
/// mengembalikan box dari registry.
impl Engine for Box<dyn Engine> {
    fn id(&self) -> &str {
        (**self).id()
    }
    fn name(&self) -> &str {
        (**self).name()
    }
    fn options_schema(&self) -> Vec<crate::engines::OptionDef> {
        (**self).options_schema()
    }
    fn input_exts(&self) -> &'static [&'static str] {
        (**self).input_exts()
    }
    fn output_name(&self, file: &Path, opts: &EngineOptions) -> String {
        (**self).output_name(file, opts)
    }
    fn max_batch_concurrency(&self) -> usize {
        (**self).max_batch_concurrency()
    }
    fn is_remote(&self) -> bool {
        (**self).is_remote()
    }
    fn process<'a>(
        &'a self,
        file: &'a Path,
        out_dir: &'a Path,
        opts: &'a EngineOptions,
        progress: Option<&'a crate::net::http::ProgressSink>,
    ) -> crate::net::http::BoxFuture<'a, Result<Vec<u8>, EngineError>> {
        (**self).process(file, out_dir, opts, progress)
    }
    fn process_controlled<'a>(
        &'a self,
        file: &'a Path,
        out_dir: &'a Path,
        opts: &'a EngineOptions,
        progress: Option<&'a crate::net::http::ProgressSink>,
        control: Option<&'a ProcessControl>,
    ) -> crate::net::http::BoxFuture<'a, Result<Vec<u8>, EngineError>> {
        (**self).process_controlled(file, out_dir, opts, progress, control)
    }
}

/// Run the batch with a rolling worker pool. Emits `on_event` for every file
/// (serialized through one aggregator so per-file event order is intact),
/// checks `cancel` and suspends while `pause` before each claim, returns
/// `(ok, fail)`. Stops early on rate limit: workers finish their current file
/// then exit without claiming more.
///
/// `engines_for_worker(w)` dipanggil sekali per worker (w = 0..concurrency)
/// sehingga tiap worker punya instance engine sendiri — state rotator proxy
/// tidak balapan antar worker. Mitigasi rate limit (opsi ADV semua engine):
/// - `concurrency` (1..=8, default 3): jumlah worker; `1` = jalur serial.
/// - `batch_delay` (detik): gate global — paling banyak satu file *mulai*
///   per window, di semua worker (file in-flight tidak terpengaruh).
/// - `retry_403`: HTTP 403 (IP diblokir sementara) → backoff 30s, retry sekali.
pub async fn run_batch<F, E>(
    engines_for_worker: impl Fn(usize) -> E,
    files: Vec<PathBuf>,
    out_dir: &Path,
    opts: &EngineOptions,
    cancel: Arc<AtomicBool>,
    pause: Arc<AtomicBool>,
    mut on_event: F,
) -> (u32, u32)
where
    F: FnMut(BatchEvent),
    E: Engine + Send + 'static,
{
    let total = files.len();
    let delay_secs = opts
        .get("batch_delay")
        .and_then(|v| v.as_f64())
        .unwrap_or(DEFAULT_BATCH_DELAY_SECS)
        .max(0.0);
    let retry_403 = opts
        .get("retry_403")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let mut first_engine = Some(engines_for_worker(0));
    let engine_limit = first_engine
        .as_ref()
        .unwrap()
        .max_batch_concurrency()
        .clamp(1, 8);
    let concurrency =
        clamp_concurrency(opts.get("concurrency").and_then(|v| v.as_f64())).min(engine_limit);

    // Antrian file dengan indeks asli; claim = pop index terkecil yang belum
    // di-claim (urutan input terjaga). Mutex sinkron aman: guard tidak
    // pernah dipegang lintas await.
    let queue = Arc::new(Mutex::new(
        files
            .into_iter()
            .enumerate()
            .map(|(i, f)| (f, i))
            .collect::<std::collections::VecDeque<(PathBuf, usize)>>(),
    ));
    // Gate pacing global: satu START per `batch_delay` detik di seluruh pool.
    let next_start = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false)); // worker selesai in-flight lalu berhenti
                                                 // Semua event + counter lewat satu channel ke aggregator agar emission
                                                 // terserialisasi dan urutan per-file tetap FileStart → progress → akhir.
    enum WorkerMsg {
        Event(BatchEvent),
        Ok,
        Fail,
        RateLimited,
    }
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<WorkerMsg>();

    // Worker N: claim → pacing → process_file! (badan identik jalur serial)
    // → laporkan lewat channel. Engine instance milik worker sendiri dari
    // factory (hindari balapan rotator proxy antar worker).
    macro_rules! process_file {
        ($engine:expr, $emit:expr, $file:expr, $cancel:expr, $out_dir:expr, $opts:expr, $control:expr) => {{
            let engine = &$engine;
            let emit = $emit;
            let file = $file;
            let out_dir = $out_dir;
            let opts = $opts;
            let name = file
                .0
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("?")
                .to_string();
            emit(BatchEvent::FileStart {
                index: file.1,
                total,
                name: name.clone(),
            });
            // Ok: tandai sukses + emit FileDone. Dua jalur (biasa & retry 403)
            // berbagi kode ini; `name` di-clone karena dipakai lagi di cabang fail.
            macro_rules! mark_done {
                () => {{
                    emit(BatchEvent::FileDone {
                        name: name.clone(),
                        output: engine.output_name(&file.0, opts),
                    });
                }};
            }
            // Jalankan process sambil meneruskan tick progress (v2 SSE) ke UI.
            // Saat future selesai, drain tick yang sudah antre sebelum mengembalikan
            // hasil agar FileDone/FileFail selalu dikirim setelah progress terakhir.
            macro_rules! run_once {
                () => {{
                    let (ptx, mut prx) = tokio::sync::mpsc::unbounded_channel::<u8>();
                    let mut fut = std::pin::pin!(engine.process_controlled(
                        &file.0,
                        out_dir,
                        opts,
                        Some(&ptx),
                        $control
                    ));
                    loop {
                        tokio::select! {
                            r = &mut fut => {
                                while let Ok(pct) = prx.try_recv() {
                                    emit(BatchEvent::FileProgress {
                                        name: name.clone(),
                                        percent: pct,
                                    });
                                }
                                break r;
                            }
                            Some(pct) = prx.recv() => {
                                emit(BatchEvent::FileProgress { name: name.clone(), percent: pct });
                            }
                        }
                    }
                }};
            }
            // Retry 403 tepat satu kali per file (IP diblokir sementara);
            // 403 kedua langsung gagal dengan penanda suffix. Penghitung
            // percobaan mencegah loop 403 tanpa batas.
            let mut attempts_403 = 0u32;
            let outcome: Result<(), Option<String>> = loop {
                match run_once!() {
                    Ok(_) => {
                        mark_done!();
                        break Ok(());
                    }
                    Err(EngineError::Cancelled) => break Err(None),
                    Err(EngineError::RateLimit) => {
                        break Err(Some("rate limit reached (429) — batch stopped".to_string()))
                    }
                    Err(e) if retry_403 && attempts_403 == 0 && is_http_403(&e) => {
                        attempts_403 += 1;
                        sleep_interruptible(RETRY_403_BACKOFF_SECS, $cancel).await;
                        if $cancel.load(Ordering::Relaxed) {
                            break Err(None); // cancel menang: tanpa FileFail
                        }
                        continue; // coba sekali lagi lewat kepala loop
                    }
                    Err(e) if retry_403 && is_http_403(&e) => {
                        break Err(Some(format!("{e} (setelah retry 403)")));
                    }
                    Err(e) => break Err(Some(e.to_string())),
                }
            };
            outcome
        }};
    }

    // Spawn setiap worker sebagai task terpisah supaya blocking work
    // (seperti Command::output() milik svg-converter) tidak menahan
    // aggregator membaca channel event. Ownership dipindahkan: out_dir
    // dan opts di-clone per worker, engine dimiliki worker.
    let out_dir_owned = out_dir.to_path_buf();
    let opts_owned = opts.clone();
    let mut handles = Vec::with_capacity(concurrency);
    for worker_index in 0..concurrency {
        let queue = queue.clone();
        let next_start = next_start.clone();
        let stop = stop.clone();
        let tx = tx.clone();
        let worker_cancel = cancel.clone();
        let worker_pause = pause.clone();
        let engine = if worker_index == 0 {
            first_engine.take().unwrap()
        } else {
            engines_for_worker(worker_index)
        };
        let out_dir_val = out_dir_owned.clone();
        let opts_val = opts_owned.clone();
        handles.push(tauri::async_runtime::spawn(async move {
            let out_dir: &Path = &out_dir_val;
            let opts: &EngineOptions = &opts_val;
            let cancel = &*worker_cancel;
            let pause = &*worker_pause;
            let control = ProcessControl::new(worker_cancel.clone(), worker_pause.clone());
            loop {
                while pause.load(Ordering::Relaxed) {
                    if cancel.load(Ordering::Relaxed) {
                        return;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                }
                if cancel.load(Ordering::Relaxed) || stop.load(Ordering::Relaxed) {
                    return;
                }
                let claimed = {
                    let mut q = queue.lock();
                    q.pop_front()
                };
                let Some(file) = claimed else { return };
                if delay_secs > 0.0 {
                    loop {
                        if cancel.load(Ordering::Relaxed) || stop.load(Ordering::Relaxed) {
                            return;
                        }
                        let now_ms = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0);
                        let target_ms = next_start.load(Ordering::SeqCst);
                        if now_ms >= target_ms
                            && next_start
                                .compare_exchange(
                                    target_ms,
                                    now_ms + (delay_secs * 1000.0) as u64,
                                    Ordering::SeqCst,
                                    Ordering::SeqCst,
                                )
                                .is_ok()
                        {
                            break;
                        }
                        let wait_ms = target_ms.saturating_sub(now_ms);
                        tokio::time::sleep(std::time::Duration::from_millis(wait_ms.min(50))).await;
                    }
                }
                let file_name = file
                    .0
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("?")
                    .to_string();
                match process_file!(
                    engine,
                    |ev| {
                        let _ = tx.send(WorkerMsg::Event(ev));
                    },
                    file,
                    &*worker_cancel,
                    out_dir,
                    opts,
                    Some(&control)
                ) {
                    Ok(()) => {
                        let _ = tx.send(WorkerMsg::Ok);
                    }
                    Err(Some(msg)) => {
                        let is_rate_limit = msg.contains("rate limit");
                        let name = file_name;
                        let _ =
                            tx.send(WorkerMsg::Event(BatchEvent::FileFail { name, error: msg }));
                        if is_rate_limit {
                            stop.store(true, Ordering::SeqCst);
                            let _ = tx.send(WorkerMsg::RateLimited);
                        } else {
                            let _ = tx.send(WorkerMsg::Fail);
                        }
                    }
                    Err(None) => {} // dibatalkan saat backoff 403
                }
            }
        }));
    }
    // Drop sender utama — channel menutup hanya setelah semua worker
    // selesai (masing-masing punya clone tx sendiri).
    drop(tx);
    let mut ok = 0u32;
    let mut fail = 0u32;
    // Aggregator: event diterima LANGSUNG saat worker mengirim, tanpa
    // diblokir proses engine (worker di-spawn di task terpisah).
    // Channel menutup sendiri saat worker terakhir selesai + tx drop.
    while let Some(msg) = rx.recv().await {
        match msg {
            WorkerMsg::Event(ev) => on_event(ev),
            WorkerMsg::Ok => ok += 1,
            WorkerMsg::Fail | WorkerMsg::RateLimited => fail += 1,
        }
    }
    // Tunggu semua worker benar-benar selesai.
    for h in handles {
        let _ = h.await;
    }
    (ok, fail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engines::{OptionDef, OptionKind};

    /// FakeEngine: `fail_times` = berapa kali pertama process() gagal dengan
    /// error yang ditentukan (dipakai tes retry 403).
    struct FakeEngine {
        fail_times: std::sync::atomic::AtomicU32,
        fail_err: Option<EngineError>,
    }

    impl FakeEngine {
        fn ok() -> Self {
            FakeEngine {
                fail_times: std::sync::atomic::AtomicU32::new(0),
                fail_err: None,
            }
        }
        fn fail_n_times(n: u32, err: EngineError) -> Self {
            FakeEngine {
                fail_times: std::sync::atomic::AtomicU32::new(n),
                fail_err: Some(err),
            }
        }
    }

    impl Engine for FakeEngine {
        fn id(&self) -> &str {
            "fake"
        }
        fn name(&self) -> &str {
            "Fake"
        }
        fn options_schema(&self) -> Vec<OptionDef> {
            vec![OptionDef {
                id: "x".into(),
                label: "X".into(),
                kind: OptionKind::Bool,
                default: serde_json::json!(false),
            }]
        }
        fn output_name(&self, file: &Path, _opts: &EngineOptions) -> String {
            format!("{}.svg", file.file_stem().unwrap().to_string_lossy())
        }
        fn process<'a>(
            &'a self,
            _f: &'a Path,
            _o: &'a Path,
            _opts: &'a EngineOptions,
            _progress: Option<&'a crate::net::http::ProgressSink>,
        ) -> crate::net::http::BoxFuture<'a, Result<Vec<u8>, EngineError>> {
            Box::pin(async move {
                // decrement aman: hanya kurangi kalau masih > 0 (fetch_sub
                // pada 0 wrap ke u32::MAX → tanpa guard ini bakal gagal terus)
                let mut n = self.fail_times.load(std::sync::atomic::Ordering::SeqCst);
                loop {
                    if n == 0 {
                        return Ok(b"ok".to_vec());
                    }
                    match self.fail_times.compare_exchange(
                        n,
                        n - 1,
                        std::sync::atomic::Ordering::SeqCst,
                        std::sync::atomic::Ordering::SeqCst,
                    ) {
                        Ok(_) => {
                            return Err(self
                                .fail_err
                                .clone()
                                .unwrap_or(EngineError::Other("mock fail".into())));
                        }
                        Err(cur) => n = cur,
                    }
                }
            })
        }
    }

    /// Opsi tes tanpa jeda antar file (agar suite cepat) + retry 403 aktif.
    fn test_opts() -> EngineOptions {
        [
            ("batch_delay".to_string(), serde_json::json!(0)),
            ("retry_403".to_string(), serde_json::json!(true)),
        ]
        .into_iter()
        .collect()
    }

    /// Factory sesuai kontrak run_batch: dipanggil sekali per worker dan
    /// menghasilkan instance engine baru untuk worker itu.
    fn factory_for<E>(make: impl Fn() -> E) -> impl Fn(usize) -> E
    where
        E: crate::engines::Engine + 'static,
    {
        move |_| make()
    }

    #[tokio::test]
    async fn run_batch_counts_ok_and_emits_events() {
        let dir = std::env::temp_dir().join("xix-batch-test");
        std::fs::create_dir_all(&dir).unwrap();
        for n in ["a.png", "b.png", "c.txt"] {
            std::fs::write(dir.join(n), b"x").unwrap();
        }
        let files: Vec<PathBuf> = ["a.png", "b.png", "c.txt"]
            .iter()
            .map(|n| dir.join(n))
            .collect();
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let ev = events.clone();
        let (ok, fail) = run_batch(
            factory_for(FakeEngine::ok),
            files,
            &dir,
            &test_opts(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            move |e| ev.lock().unwrap().push(e),
        )
        .await;
        assert_eq!(
            ok, 3,
            "engine processes every file passed in (filtering is scan's job)"
        );
        assert_eq!(fail, 0);
        assert_eq!(events.lock().unwrap().len(), 6); // 3 start + 3 done
    }

    /// Engine yang mengirim beberapa tick progress lewat sink sebelum sukses —
    /// dipakai untuk memastikan run_batch meneruskannya sebagai FileProgress.
    struct ProgressEngine;
    impl Engine for ProgressEngine {
        fn id(&self) -> &str {
            "prog"
        }
        fn name(&self) -> &str {
            "Prog"
        }
        fn options_schema(&self) -> Vec<OptionDef> {
            vec![]
        }
        fn output_name(&self, file: &Path, _o: &EngineOptions) -> String {
            format!("{}.svg", file.file_stem().unwrap().to_string_lossy())
        }
        fn process<'a>(
            &'a self,
            _f: &'a Path,
            _o: &'a Path,
            _opts: &'a EngineOptions,
            progress: Option<&'a crate::net::http::ProgressSink>,
        ) -> crate::net::http::BoxFuture<'a, Result<Vec<u8>, EngineError>> {
            Box::pin(async move {
                if let Some(tx) = progress {
                    for p in [25u8, 50, 75, 100] {
                        let _ = tx.send(p);
                        tokio::task::yield_now().await; // let the select loop drain
                    }
                }
                Ok(b"ok".to_vec())
            })
        }
    }

    /// Enqueues every progress tick and completes in the same poll so the
    /// completion branch must drain the receiver deterministically.
    struct QueuedProgressEngine {
        fail: bool,
    }

    impl Engine for QueuedProgressEngine {
        fn id(&self) -> &str {
            "queued-prog"
        }
        fn name(&self) -> &str {
            "Queued Prog"
        }
        fn options_schema(&self) -> Vec<OptionDef> {
            vec![]
        }
        fn output_name(&self, file: &Path, _o: &EngineOptions) -> String {
            format!("{}.svg", file.file_stem().unwrap().to_string_lossy())
        }
        fn process<'a>(
            &'a self,
            _f: &'a Path,
            _o: &'a Path,
            _opts: &'a EngineOptions,
            progress: Option<&'a crate::net::http::ProgressSink>,
        ) -> crate::net::http::BoxFuture<'a, Result<Vec<u8>, EngineError>> {
            Box::pin(async move {
                if let Some(tx) = progress {
                    for p in [25u8, 50, 75, 100] {
                        let _ = tx.send(p);
                    }
                }
                if self.fail {
                    Err(EngineError::Other("queued failure".into()))
                } else {
                    Ok(b"ok".to_vec())
                }
            })
        }
    }

    #[tokio::test]
    async fn run_batch_drains_queued_progress_before_done() {
        let dir = std::env::temp_dir().join("xix-batch-queued-progress-success");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("p.png");
        std::fs::write(&file, b"x").unwrap();
        let mut events = Vec::new();
        let result = run_batch(
            factory_for(|| QueuedProgressEngine { fail: false }),
            vec![file],
            &dir,
            &test_opts(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            |e| events.push(e),
        )
        .await;

        assert_eq!(result, (1, 0));
        assert!(matches!(
            &events[..],
            [
                BatchEvent::FileStart { .. },
                BatchEvent::FileProgress { percent: 25, .. },
                BatchEvent::FileProgress { percent: 50, .. },
                BatchEvent::FileProgress { percent: 75, .. },
                BatchEvent::FileProgress { percent: 100, .. },
                BatchEvent::FileDone { .. },
            ]
        ));
    }

    #[tokio::test]
    async fn run_batch_drains_queued_progress_before_fail() {
        let dir = std::env::temp_dir().join("xix-batch-queued-progress-fail");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("p.png");
        std::fs::write(&file, b"x").unwrap();
        let mut events = Vec::new();
        let result = run_batch(
            factory_for(|| QueuedProgressEngine { fail: true }),
            vec![file],
            &dir,
            &test_opts(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            |e| events.push(e),
        )
        .await;

        assert_eq!(result, (0, 1));
        assert!(matches!(
            &events[..],
            [
                BatchEvent::FileStart { .. },
                BatchEvent::FileProgress { percent: 25, .. },
                BatchEvent::FileProgress { percent: 50, .. },
                BatchEvent::FileProgress { percent: 75, .. },
                BatchEvent::FileProgress { percent: 100, .. },
                BatchEvent::FileFail { .. },
            ]
        ));
    }

    #[tokio::test]
    async fn run_batch_forwards_progress_ticks() {
        let dir = std::env::temp_dir().join("xix-batch-progress");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("p.png");
        std::fs::write(&file, b"x").unwrap();
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let ev = events.clone();
        let (ok, fail) = run_batch(
            factory_for(|| ProgressEngine),
            vec![file],
            &dir,
            &test_opts(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            move |e| ev.lock().unwrap().push(e),
        )
        .await;
        assert_eq!((ok, fail), (1, 0));
        let evs = events.lock().unwrap();
        let pcts: Vec<u8> = evs
            .iter()
            .filter_map(|e| match e {
                BatchEvent::FileProgress { percent, .. } => Some(*percent),
                _ => None,
            })
            .collect();
        assert_eq!(
            pcts,
            vec![25, 50, 75, 100],
            "semua tick progress diteruskan berurutan"
        );
        // dan tetap ada FileStart + FileDone di sekitarnya
        assert!(matches!(evs.first(), Some(BatchEvent::FileStart { .. })));
        assert!(matches!(evs.last(), Some(BatchEvent::FileDone { .. })));
    }

    #[tokio::test]
    async fn run_batch_stops_when_cancelled() {
        let dir = std::env::temp_dir().join("xix-batch-cancel");
        std::fs::create_dir_all(&dir).unwrap();
        let files: Vec<PathBuf> = (0..5).map(|i| dir.join(format!("f{i}.png"))).collect();
        for f in &files {
            std::fs::write(f, b"x").unwrap();
        }
        let cancel = Arc::new(AtomicBool::new(true));
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let ev = events.clone();
        let (ok, fail) = run_batch(
            factory_for(FakeEngine::ok),
            files,
            &dir,
            &test_opts(),
            cancel,
            Arc::new(AtomicBool::new(false)),
            move |e| ev.lock().unwrap().push(e),
        )
        .await;
        assert_eq!(ok, 0);
        assert_eq!(fail, 0);
        assert!(events.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn run_batch_waits_while_paused_then_resumes() {
        let dir = std::env::temp_dir().join("xix-batch-pause");
        std::fs::create_dir_all(&dir).unwrap();
        let files: Vec<PathBuf> = (0..3).map(|i| dir.join(format!("f{i}.png"))).collect();
        for f in &files {
            std::fs::write(f, b"x").unwrap();
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let pause = Arc::new(AtomicBool::new(true));
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let ev = events.clone();
        let paused = tokio::time::timeout(
            std::time::Duration::from_millis(250),
            run_batch(
                factory_for(FakeEngine::ok),
                files.clone(),
                &dir,
                &test_opts(),
                cancel.clone(),
                pause.clone(),
                move |e| ev.lock().unwrap().push(e),
            ),
        )
        .await;
        assert!(paused.is_err(), "paused batch must not process files");
        pause.store(false, Ordering::Relaxed);
        let (ok, fail) = run_batch(
            factory_for(FakeEngine::ok),
            files.clone(),
            &dir,
            &test_opts(),
            cancel.clone(),
            pause.clone(),
            |_| {},
        )
        .await;
        assert_eq!(ok, 3, "resumes after unpause");
        assert_eq!(fail, 0);
    }

    #[tokio::test]
    async fn cancel_while_paused_still_stops() {
        let dir = std::env::temp_dir().join("xix-batch-pause-cancel");
        std::fs::create_dir_all(&dir).unwrap();
        let files: Vec<PathBuf> = (0..3).map(|i| dir.join(format!("f{i}.png"))).collect();
        let cancel = Arc::new(AtomicBool::new(false));
        let pause = Arc::new(AtomicBool::new(true));
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let ev = events.clone();
        let paused = tokio::time::timeout(
            std::time::Duration::from_millis(250),
            run_batch(
                factory_for(FakeEngine::ok),
                files.clone(),
                &dir,
                &test_opts(),
                cancel.clone(),
                pause.clone(),
                move |e| ev.lock().unwrap().push(e),
            ),
        )
        .await;
        assert!(paused.is_err(), "still suspended while paused");
        cancel.store(true, Ordering::Relaxed);
        let (ok, fail) = run_batch(
            factory_for(FakeEngine::ok),
            files.clone(),
            &dir,
            &test_opts(),
            cancel.clone(),
            pause.clone(),
            |_| {},
        )
        .await;
        assert_eq!(ok, 0, "cancel wins over pause");
        assert_eq!(fail, 0);
    }

    #[tokio::test]
    async fn retries_once_on_http_403_then_succeeds() {
        let dir = std::env::temp_dir().join("xix-batch-403");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.png");
        std::fs::write(&file, b"x").unwrap();

        let eng = factory_for(|| {
            FakeEngine::fail_n_times(
                1,
                EngineError::Network("HTTP 403 Forbidden: blocked".into()),
            )
        });
        let (ok, fail) = run_batch(
            eng,
            vec![file],
            &dir,
            &test_opts(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .await;
        assert_eq!(ok, 1, "403 pertama ditangani, retry sekali sukses");
        assert_eq!(fail, 0);
    }

    #[tokio::test]
    async fn retry_403_disabled_marks_fail_without_retry() {
        let dir = std::env::temp_dir().join("xix-batch-403-off");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.png");
        std::fs::write(&file, b"x").unwrap();

        let eng = factory_for(|| {
            FakeEngine::fail_n_times(
                99,
                EngineError::Network("HTTP 403 Forbidden: blocked".into()),
            )
        });
        let opts: EngineOptions = [("batch_delay".to_string(), serde_json::json!(0))]
            .into_iter()
            .collect(); // retry_403 default true? — set false eksplisit:
        let opts = {
            let mut m = opts;
            m.insert("retry_403".to_string(), serde_json::json!(false));
            m
        };
        let (ok, fail) = run_batch(
            eng,
            vec![file],
            &dir,
            &opts,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .await;
        assert_eq!(ok, 0);
        assert_eq!(
            fail, 1,
            "retry dimatikan → langsung fail, tidak menunggu backoff"
        );
    }

    #[tokio::test]
    async fn persistent_403_fails_once_with_suffix_never_hangs() {
        let dir = std::env::temp_dir().join("xix-batch-403-persistent");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.png");
        std::fs::write(&file, b"x").unwrap();
        // Engine yang selalu 403: retry harus tepat satu kali (2 process
        // calls), lalu FileFail dengan suffix — bukan loop tanpa batas.
        let eng = factory_for(|| {
            FakeEngine::fail_n_times(
                u32::MAX,
                EngineError::Network("HTTP 403 Forbidden: blocked".into()),
            )
        });
        let mut events = Vec::new();
        let (ok, fail) = run_batch(
            eng,
            vec![file],
            &dir,
            &test_opts(), // retry_403 aktif
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            |e| events.push(e),
        )
        .await;
        assert_eq!((ok, fail), (0, 1));
        let fails: Vec<&String> = events
            .iter()
            .filter_map(|e| match e {
                BatchEvent::FileFail { error, .. } => Some(error),
                _ => None,
            })
            .collect();
        assert_eq!(fails.len(), 1, "tepat satu FileFail setelah satu retry");
        assert!(
            fails[0].ends_with("HTTP 403 Forbidden: blocked (setelah retry 403)"),
            "FileFail harus berakhiran suffix retry-403: {fails:?}"
        );
    }

    #[tokio::test]
    async fn batch_delay_waits_between_files() {
        let dir = std::env::temp_dir().join("xix-batch-delay");
        std::fs::create_dir_all(&dir).unwrap();
        let files: Vec<PathBuf> = (0..3).map(|i| dir.join(format!("f{i}.png"))).collect();
        for f in &files {
            std::fs::write(f, b"x").unwrap();
        }
        // 0.15s per file × 2 jeda → total ≥ ~0.3s; tanpa jeda selesai ≪ 0.1s.
        let opts: EngineOptions = [
            ("batch_delay".to_string(), serde_json::json!(0.15)),
            ("retry_403".to_string(), serde_json::json!(false)),
        ]
        .into_iter()
        .collect();
        let t0 = std::time::Instant::now();
        let (ok, fail) = run_batch(
            factory_for(FakeEngine::ok),
            files,
            &dir,
            &opts,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .await;
        let elapsed = t0.elapsed();
        assert_eq!(ok, 3);
        assert_eq!(fail, 0);
        assert!(
            elapsed.as_secs_f64() >= 0.25,
            "harus menunggu jeda antar file (elapsed={elapsed:?})"
        );
    }

    #[test]
    fn image_ext_filter() {
        assert!(is_image_path(Path::new("a.PNG")));
        assert!(is_image_path(Path::new("a.webp")));
        assert!(!is_image_path(Path::new("a.txt")));
        assert!(!is_image_path(Path::new("a")));
    }

    // ---- Task 3: rolling worker pool ------------------------------------

    #[test]
    fn concurrency_clamped_to_valid_range() {
        assert_eq!(clamp_concurrency(None), 3);
        assert_eq!(clamp_concurrency(Some(0.0)), 1);
        assert_eq!(clamp_concurrency(Some(99.0)), 8);
        assert_eq!(clamp_concurrency(Some(4.0)), 4);
        // Nilai pecahan dari serde Number tetap terbaca (as_f64), dibulatkan.
        assert_eq!(clamp_concurrency(Some(5.0)), 5);
        assert_eq!(clamp_concurrency(Some(0.4)), 1); // round(0.4)=0 → clamp 1
        assert_eq!(clamp_concurrency(Some(8.6)), 8); // round → 9 → clamp 8
    }

    /// Log bersama untuk mengamati urutan claim/start dari sisi engine.
    type SharedLog = Arc<parking_lot::Mutex<Vec<String>>>;
    type SharedTimes = Arc<parking_lot::Mutex<Vec<(String, std::time::Instant)>>>;

    fn log_of(log: &SharedLog, prefix: &str) -> Vec<usize> {
        log.lock()
            .iter()
            .filter_map(|e| e.strip_prefix(prefix))
            .filter_map(|s| s.parse::<usize>().ok())
            .collect()
    }

    /// Engine instan yang mencatat setiap process() ke log bersama, lalu
    /// menunggu di gate sebelum sukses. `gate` = jumlah release tersedia;
    /// proses menunggu sampai tersedia, lalu menguranginya.
    struct GatedLogEngine {
        log: SharedLog,
        gate: Arc<(parking_lot::Mutex<u32>, tokio::sync::Notify)>,
    }

    impl GatedLogEngine {
        fn with_gate(
            log: SharedLog,
            gate: Arc<(parking_lot::Mutex<u32>, tokio::sync::Notify)>,
        ) -> Self {
            GatedLogEngine { log, gate }
        }
    }

    /// Melepas n proses yang menunggu di gate bersama.
    fn release_gate(gate: &Arc<(parking_lot::Mutex<u32>, tokio::sync::Notify)>, n: u32) {
        let (count, notify) = &**gate;
        *count.lock() += n;
        notify.notify_waiters();
    }

    impl Engine for GatedLogEngine {
        fn id(&self) -> &str {
            "gated"
        }
        fn name(&self) -> &str {
            "Gated"
        }
        fn options_schema(&self) -> Vec<crate::engines::OptionDef> {
            vec![]
        }
        fn output_name(&self, file: &Path, _o: &EngineOptions) -> String {
            format!("{}.svg", file.file_stem().unwrap().to_string_lossy())
        }
        fn process<'a>(
            &'a self,
            file: &'a Path,
            _o: &'a Path,
            _opts: &'a EngineOptions,
            _progress: Option<&'a crate::net::http::ProgressSink>,
        ) -> crate::net::http::BoxFuture<'a, Result<Vec<u8>, EngineError>> {
            let idx = file
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.parse::<usize>().ok());
            let gate = self.gate.clone();
            Box::pin(async move {
                if let Some(i) = idx {
                    self.log.lock().push(format!("process:{i}"));
                }
                let (count, notify) = &*gate;
                loop {
                    let n = *count.lock();
                    if n > 0 {
                        *count.lock() = n - 1;
                        break;
                    }
                    notify.notified().await;
                    tokio::task::yield_now().await;
                }
                Ok(b"ok".to_vec())
            })
        }
    }

    #[tokio::test]
    async fn rolling_pool_claims_next_file_without_waiting_for_batch_of_n() {
        let dir = std::env::temp_dir().join("xix-batch-rolling");
        std::fs::create_dir_all(&dir).unwrap();
        let files: Vec<PathBuf> = (0..5).map(|i| dir.join(format!("{i}.png"))).collect();
        for f in &files {
            std::fs::write(f, b"x").unwrap();
        }
        // 3 worker mulai file 0..2; melepas satu gate membuat worker roll ke
        // file 3..4 sementara dua lainnya masih in flight.
        let log: SharedLog = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let opts: EngineOptions = [
            ("batch_delay".to_string(), serde_json::json!(0)),
            ("concurrency".to_string(), serde_json::json!(3)),
            ("retry_403".to_string(), serde_json::json!(true)),
        ]
        .into_iter()
        .collect();
        // Gate bersama untuk sinkronisasi tes dengan engine di dalam pool.
        let gate: Arc<(parking_lot::Mutex<u32>, tokio::sync::Notify)> =
            Arc::new((parking_lot::Mutex::new(0), tokio::sync::Notify::new()));
        let gate2 = gate.clone();
        let log2 = log.clone();
        let handle = tokio::spawn(async move {
            run_batch(
                move |_| GatedLogEngine::with_gate(log2.clone(), gate2.clone()),
                files,
                &dir,
                &opts,
                Arc::new(AtomicBool::new(false)),
                Arc::new(AtomicBool::new(false)),
                |_| {},
            )
            .await
        });
        wait_for(
            || log_of(&log, "process:").len() >= 3,
            "lebar pool = 3 in flight",
        )
        .await;
        let mut claimed3 = log_of(&log, "process:");
        claimed3.sort_unstable();
        assert_eq!(
            claimed3,
            vec![0, 1, 2],
            "tepat 3 file berbeda in flight (urutan bebas)"
        );
        // Lepas ketiganya: worker yang selesai langsung claim 3 dan 4 tanpa
        // menunggu seluruh batch-of-N selesai.
        release_gate(&gate, 3);
        wait_for(
            || log_of(&log, "process:").len() >= 5,
            "semua file ter-claim",
        )
        .await;
        let mut claimed5 = log_of(&log, "process:");
        claimed5.sort_unstable();
        assert_eq!(
            claimed5,
            vec![0, 1, 2, 3, 4],
            "file 3 dan 4 harus di-claim saat rekan masih in flight"
        );
        release_gate(&gate, 2);
        let (ok, fail) = handle.await.unwrap();
        assert_eq!((ok, fail), (5, 0));
    }

    /// Engine yang mencatat timestamp process() per file untuk pengujian
    /// pacing gate; sukses instan.
    struct TimedEngine {
        times: SharedTimes,
    }

    impl Engine for TimedEngine {
        fn id(&self) -> &str {
            "timed"
        }
        fn name(&self) -> &str {
            "Timed"
        }
        fn options_schema(&self) -> Vec<crate::engines::OptionDef> {
            vec![]
        }
        fn output_name(&self, file: &Path, _o: &EngineOptions) -> String {
            format!("{}.svg", file.file_stem().unwrap().to_string_lossy())
        }
        fn process<'a>(
            &'a self,
            file: &'a Path,
            _o: &'a Path,
            _opts: &'a EngineOptions,
            _progress: Option<&'a crate::net::http::ProgressSink>,
        ) -> crate::net::http::BoxFuture<'a, Result<Vec<u8>, EngineError>> {
            let idx = file
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(usize::MAX);
            Box::pin(async move {
                self.times
                    .lock()
                    .push((format!("{idx}"), std::time::Instant::now()));
                Ok(b"ok".to_vec())
            })
        }
    }

    #[tokio::test]
    async fn pacing_gate_limits_starts_per_delay_window() {
        let dir = std::env::temp_dir().join("xix-batch-pacing");
        std::fs::create_dir_all(&dir).unwrap();
        let files: Vec<PathBuf> = (0..4).map(|i| dir.join(format!("{i}.png"))).collect();
        for f in &files {
            std::fs::write(f, b"x").unwrap();
        }
        let times: SharedTimes = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let opts: EngineOptions = [
            ("batch_delay".to_string(), serde_json::json!(0.15)),
            ("concurrency".to_string(), serde_json::json!(4)),
            ("retry_403".to_string(), serde_json::json!(true)),
        ]
        .into_iter()
        .collect();
        let times2 = times.clone();
        run_batch(
            move |_| TimedEngine {
                times: times2.clone(),
            },
            files,
            &dir,
            &opts,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .await;
        let starts = times.lock();
        assert_eq!(starts.len(), 4);
        for w in starts.windows(2) {
            let gap = (w[1].1 - w[0].1).as_secs_f64();
            assert!(
                gap >= 0.14,
                "start berurutan harus >= delay terpisah secara global (gap={gap:?})"
            );
        }
    }

    /// Engine sukses kecuali file tertentu yang mengembalikan RateLimit.
    struct RateLimitAtEngine {
        log: SharedLog,
        limit_at: std::sync::atomic::AtomicU64,
    }

    impl RateLimitAtEngine {
        fn new(log: SharedLog, at: u64) -> Self {
            RateLimitAtEngine {
                log,
                limit_at: std::sync::atomic::AtomicU64::new(at),
            }
        }
    }

    impl Engine for RateLimitAtEngine {
        fn id(&self) -> &str {
            "ratelimit"
        }
        fn name(&self) -> &str {
            "RateLimit"
        }
        fn options_schema(&self) -> Vec<crate::engines::OptionDef> {
            vec![]
        }
        fn output_name(&self, file: &Path, _o: &EngineOptions) -> String {
            format!("{}.svg", file.file_stem().unwrap().to_string_lossy())
        }
        fn process<'a>(
            &'a self,
            file: &'a Path,
            _o: &'a Path,
            _opts: &'a EngineOptions,
            _progress: Option<&'a crate::net::http::ProgressSink>,
        ) -> crate::net::http::BoxFuture<'a, Result<Vec<u8>, EngineError>> {
            let idx = file
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(u64::MAX);
            Box::pin(async move {
                self.log.lock().push(format!("process:{idx}"));
                if self.limit_at.load(Ordering::Relaxed) == idx {
                    Err(EngineError::RateLimit)
                } else {
                    Ok(b"ok".to_vec())
                }
            })
        }
    }

    #[tokio::test]
    async fn rate_limit_drains_inflight_then_stops_pool() {
        let dir = std::env::temp_dir().join("xix-batch-ratelimit");
        std::fs::create_dir_all(&dir).unwrap();
        let files: Vec<PathBuf> = (0..6).map(|i| dir.join(format!("{i}.png"))).collect();
        for f in &files {
            std::fs::write(f, b"x").unwrap();
        }
        // File 0 langsung RateLimit → tidak ada file lain boleh mulai;
        // pool berhenti setelah in-flight drain. Delay besar membuat rekan
        // masih tertahan di pacing gate saat stop menyebar → deterministik.
        let opts: EngineOptions = [
            ("batch_delay".to_string(), serde_json::json!(5)),
            ("retry_403".to_string(), serde_json::json!(true)),
            ("concurrency".to_string(), serde_json::json!(3)),
        ]
        .into_iter()
        .collect();
        let log: SharedLog = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let log2 = log.clone();
        let mut events = Vec::new();
        let (ok, fail) = run_batch(
            move |_| RateLimitAtEngine::new(log2.clone(), 0),
            files,
            &dir,
            &opts,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            |e| events.push(e),
        )
        .await;
        assert_eq!(
            log_of(&log, "process:"),
            vec![0],
            "tidak ada file lain di-claim"
        );
        assert_eq!((ok, fail), (0, 1));
        assert!(
            matches!(
                &events[..],
                [
                    BatchEvent::FileStart { index: 0, .. },
                    BatchEvent::FileFail { .. }
                ]
            ),
            "event stream: hanya file 0 start lalu fail"
        );
    }

    #[test]
    fn common_options_expose_concurrency_number() {
        let defs = crate::engines::common_batch_options(8);
        let c = defs
            .iter()
            .find(|d| d.id == "concurrency")
            .expect("concurrency option");
        assert!(matches!(
            c.kind,
            crate::engines::OptionKind::Number {
                min: 1.0,
                max: 8.0,
                step: 1.0
            }
        ));
        assert_eq!(c.default, serde_json::json!(3));
    }

    /// Opsi tes + concurrency eksplisit.
    fn test_opts_with_concurrency(n: i64) -> EngineOptions {
        [
            ("batch_delay".to_string(), serde_json::json!(0)),
            ("retry_403".to_string(), serde_json::json!(true)),
            ("concurrency".to_string(), serde_json::json!(n)),
        ]
        .into_iter()
        .collect()
    }

    /// Poll sampai kondisi benar atau timeout (detik); panik saat habis.
    /// Async: tidur via tokio agar task lain di runtime current-thread
    /// tetap jalan selama menunggu.
    async fn wait_for(mut cond: impl FnMut() -> bool, msg: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if cond() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("timeout menunggu: {msg}");
    }

    struct LimitedEngine {
        active: Arc<std::sync::atomic::AtomicUsize>,
        peak: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl Engine for LimitedEngine {
        fn id(&self) -> &str {
            "limited"
        }
        fn name(&self) -> &str {
            "Limited"
        }
        fn options_schema(&self) -> Vec<OptionDef> {
            vec![]
        }
        fn output_name(&self, file: &Path, _opts: &EngineOptions) -> String {
            file.display().to_string()
        }
        fn max_batch_concurrency(&self) -> usize {
            1
        }
        fn process<'a>(
            &'a self,
            _file: &'a Path,
            _out: &'a Path,
            _opts: &'a EngineOptions,
            _progress: Option<&'a crate::net::http::ProgressSink>,
        ) -> crate::net::http::BoxFuture<'a, Result<Vec<u8>, EngineError>> {
            Box::pin(async move {
                let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
                self.peak.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(15)).await;
                self.active.fetch_sub(1, Ordering::SeqCst);
                Ok(vec![])
            })
        }
    }

    #[tokio::test]
    async fn engine_limit_caps_requested_worker_count() {
        let dir = std::env::temp_dir();
        let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let peak = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let a = active.clone();
        let p = peak.clone();
        let files = (0..4)
            .map(|i| dir.join(format!("xix-limit-{i}.png")))
            .collect();
        run_batch(
            move |_| LimitedEngine {
                active: a.clone(),
                peak: p.clone(),
            },
            files,
            &dir,
            &test_opts_with_concurrency(8),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .await;
        assert_eq!(peak.load(Ordering::SeqCst), 1);
    }
}
