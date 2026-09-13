use parking_lot::{Condvar, Mutex};
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read};
use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TorRuntimePaths {
    exe: PathBuf,
    geoip: PathBuf,
    geoip6: PathBuf,
}

impl TorRuntimePaths {
    fn from_root(root: &Path) -> Self {
        Self {
            exe: root.join("tor/tor.exe"),
            geoip: root.join("data/geoip"),
            geoip6: root.join("data/geoip6"),
        }
    }

    fn validate(&self) -> Result<(), String> {
        for path in [&self.exe, &self.geoip, &self.geoip6] {
            if !path.is_file() {
                return Err(format!(
                    "bundled Tor runtime tidak lengkap: file wajib tidak ditemukan: {}",
                    path.display()
                ));
            }
        }
        Ok(())
    }
}

pub fn resolve_runtime(
    resource_root: Option<&Path>,
    cwd: &Path,
) -> Result<TorRuntimePaths, String> {
    let mut roots = Vec::with_capacity(4);
    if let Some(resource_root) = resource_root {
        roots.push(resource_root.join("tor-runtime"));
    }
    roots.push(cwd.join("src-tauri/tor-runtime"));
    roots.push(cwd.join("tor-runtime"));
    roots.push(cwd.join("desktop/src-tauri/tor-runtime"));

    let mut first_validation_error = None;
    for root in &roots {
        if root.exists() {
            let paths = TorRuntimePaths::from_root(root);
            match paths.validate() {
                Ok(()) => return Ok(paths),
                Err(error) if first_validation_error.is_none() => {
                    first_validation_error = Some(error);
                }
                Err(_) => {}
            }
        }
    }

    if let Some(error) = first_validation_error {
        return Err(error);
    }

    Err(format!(
        "bundled Tor runtime tidak ditemukan; lokasi yang dicoba: {}",
        roots
            .iter()
            .map(|root| root.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

#[derive(Debug, PartialEq, Eq)]
enum BootstrapEvent {
    Progress(String),
    Ready,
    Fatal(String),
    Ignore,
}

fn parse_bootstrap_line(line: &str) -> BootstrapEvent {
    for marker in ["[err]", "[fatal]"] {
        if let Some((_, message)) = line.split_once(marker) {
            return BootstrapEvent::Fatal(message.trim().to_owned());
        }
    }

    if let Some(start) = line.find("Bootstrapped ") {
        let progress = line[start..].trim().to_owned();
        if progress.starts_with("Bootstrapped 100%") {
            BootstrapEvent::Ready
        } else {
            BootstrapEvent::Progress(progress)
        }
    } else {
        BootstrapEvent::Ignore
    }
}

struct TorLaunchConfig {
    paths: TorRuntimePaths,
    data_dir: PathBuf,
    socks_port: u16,
}

impl TorLaunchConfig {
    fn args(&self) -> Vec<OsString> {
        vec![
            "--ClientOnly".into(),
            "1".into(),
            "--SocksPort".into(),
            format!("127.0.0.1:{}", self.socks_port).into(),
            "--DataDirectory".into(),
            self.data_dir.as_os_str().to_owned(),
            "--GeoIPFile".into(),
            self.paths.geoip.as_os_str().to_owned(),
            "--GeoIPv6File".into(),
            self.paths.geoip6.as_os_str().to_owned(),
            "--AvoidDiskWrites".into(),
            "1".into(),
            "--Log".into(),
            "notice stdout".into(),
        ]
    }
}


trait ManagedTorChild: Send {
    fn try_wait(&mut self) -> Result<Option<ExitStatus>, String>;
    fn force_kill(&mut self) -> Result<(), String>;
}

#[derive(Clone, Copy)]
struct TerminationTimeouts {
    reap: Duration,
    poll: Duration,
}

impl Default for TerminationTimeouts {
    fn default() -> Self {
        Self {
            reap: Duration::from_secs(2),
            poll: Duration::from_millis(25),
        }
    }
}

trait TorLauncher: Send + Sync {
    fn launch(&self, config: &TorLaunchConfig) -> Result<LaunchedTor, String>;
}

struct LaunchedTor {
    child: Box<dyn ManagedTorChild>,
    logs: mpsc::Receiver<String>,
}

struct RunningTor {
    child: Box<dyn ManagedTorChild>,
    endpoint: String,
}

struct StartupFailure {
    message: String,
    child: Option<Box<dyn ManagedTorChild>>,
}

impl StartupFailure {
    fn without_child(message: String) -> Self {
        Self {
            message,
            child: None,
        }
    }

    fn with_child(message: String, child: Box<dyn ManagedTorChild>) -> Self {
        Self {
            message,
            child: Some(child),
        }
    }
}

type StartupOutcome = Result<Box<dyn ManagedTorChild>, StartupFailure>;

struct StartupCompletion {
    done: bool,
    outcome: Option<StartupOutcome>,
}

struct StartupControl {
    cancelled: AtomicBool,
    completion: Mutex<StartupCompletion>,
    done: Condvar,
}

impl StartupControl {
    fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            completion: Mutex::new(StartupCompletion {
                done: false,
                outcome: None,
            }),
            done: Condvar::new(),
        }
    }

    fn finish(&self, outcome: StartupOutcome) {
        let mut completion = self.completion.lock();
        if completion.done {
            drop(completion);
            let (message, child) = match outcome {
                Ok(child) => ("duplicate startup completion".to_owned(), Some(child)),
                Err(failure) => (failure.message, failure.child),
            };
            if let Some(mut child) = child {
                if let Err(error) =
                    terminate_child(&mut *child, TerminationTimeouts::default())
                {
                    self.completion.lock().outcome = Some(Err(
                        StartupFailure::with_child(
                            format!("{message}; child cleanup failed: {error}"),
                            child,
                        ),
                    ));
                }
            }
            return;
        }
        completion.outcome = Some(outcome);
        completion.done = true;
        self.done.notify_all();
    }

    fn cancel_and_wait(
        &self,
        completion_timeout: Duration,
        termination_timeouts: TerminationTimeouts,
    ) -> Result<(), String> {
        self.cancelled.store(true, Ordering::Release);
        let deadline = Instant::now() + completion_timeout;
        let outcome = {
            let mut completion = self.completion.lock();
            while !completion.done {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(format!(
                        "timed out after {completion_timeout:?} waiting for startup cancellation; ownership retained"
                    ));
                }
                self.done.wait_for(&mut completion, remaining);
            }
            completion.outcome.take()
        };

        let (message, child) = match outcome {
            Some(Ok(child)) => ("startup Tor dibatalkan".to_owned(), Some(child)),
            Some(Err(failure)) => (failure.message, failure.child),
            None => return Ok(()),
        };
        if let Some(mut child) = child {
            if let Err(error) = terminate_child(&mut *child, termination_timeouts) {
                self.completion.lock().outcome = Some(Err(
                    StartupFailure::with_child(message, child),
                ));
                return Err(error);
            }
        }
        Ok(())
    }

    fn take_outcome(&self) -> Option<StartupOutcome> {
        self.completion.lock().outcome.take()
    }

    fn restore_child(&self, message: String, child: Box<dyn ManagedTorChild>) {
        self.completion.lock().outcome =
            Some(Err(StartupFailure::with_child(message, child)));
    }
}

struct StartupLease<'a> {
    slot: &'a Mutex<Option<Arc<StartupControl>>>,
    control: Arc<StartupControl>,
    completion_timeout: Duration,
    termination_timeouts: TerminationTimeouts,
    armed: bool,
}

impl StartupLease<'_> {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for StartupLease<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let owns_slot = self
            .slot
            .lock()
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(active, &self.control));
        if owns_slot {
            if let Err(error) = self.control.cancel_and_wait(
                self.completion_timeout,
                self.termination_timeouts,
            ) {
                eprintln!("TOR ERROR: startup child cleanup failed: {error}");
                return;
            }
            let mut slot = self.slot.lock();
            if slot
                .as_ref()
                .is_some_and(|active| Arc::ptr_eq(active, &self.control))
            {
                slot.take();
            }
        }
    }
}

const STARTUP_CANCELLATION_TIMEOUT: Duration = Duration::from_secs(2);

pub struct TorManager {
    startup_gate: tokio::sync::Mutex<()>,
    starting: Mutex<Option<Arc<StartupControl>>>,
    process: Mutex<Option<RunningTor>>,
    launcher: Arc<dyn TorLauncher>,
    bootstrap_timeout: Duration,
    shutdown_epoch: AtomicU64,
    termination_timeouts: TerminationTimeouts,
}

impl TorManager {
    pub fn new() -> Self {
        Self::new_with(Arc::new(StdTorLauncher), Duration::from_secs(8 * 60))
    }

    fn new_with(launcher: Arc<dyn TorLauncher>, bootstrap_timeout: Duration) -> Self {
        Self {
            startup_gate: tokio::sync::Mutex::new(()),
            starting: Mutex::new(None),
            process: Mutex::new(None),
            launcher,
            bootstrap_timeout,
            shutdown_epoch: AtomicU64::new(0),
            termination_timeouts: TerminationTimeouts::default(),
        }
    }

    #[cfg(test)]
    fn with_ready_child(
        child: impl ManagedTorChild + 'static,
        endpoint: String,
    ) -> Self {
        let mut manager = Self::new();
        manager.termination_timeouts = TerminationTimeouts {
            reap: Duration::from_millis(5),
            poll: Duration::from_millis(1),
        };
        *manager.process.lock() = Some(RunningTor {
            child: Box::new(child),
            endpoint,
        });
        manager
    }

    pub async fn ensure_ready(
        &self,
        paths: TorRuntimePaths,
        data_dir: PathBuf,
    ) -> Result<String, String> {
        let _startup = self.startup_gate.lock().await;
        let shutdown_epoch = self.shutdown_epoch.load(Ordering::Acquire);

        {
            let mut process = self.process.lock();
            if let Some(running) = process.as_mut() {
                match running.child.try_wait() {
                    Ok(None) => return Ok(running.endpoint.clone()),
                    Ok(Some(_)) => {
                        process.take();
                    }
                    Err(error) => {
                        return Err(format!(
                            "gagal memeriksa proses Tor yang sedang berjalan: {error}"
                        ));
                    }
                }
            }
        }

        std::fs::create_dir_all(&data_dir).map_err(|error| {
            format!(
                "gagal membuat direktori data Tor {}: {error}",
                data_dir.display()
            )
        })?;

        let socks_port = allocate_loopback_port()?;
        let endpoint = format!("socks5://127.0.0.1:{socks_port}");
        let config = TorLaunchConfig {
            paths,
            data_dir,
            socks_port,
        };
        let launcher = self.launcher.clone();
        let timeout = self.bootstrap_timeout;
        let control = Arc::new(StartupControl::new());
        {
            let mut starting = self.starting.lock();
            if self.shutdown_epoch.load(Ordering::Acquire) != shutdown_epoch {
                return Err("startup Tor dibatalkan oleh shutdown".to_owned());
            }
            if starting.is_some() {
                return Err("startup Tor sebelumnya belum selesai".to_owned());
            }
            *starting = Some(control.clone());
        }
        let mut lease = StartupLease {
            slot: &self.starting,
            control: control.clone(),
            completion_timeout: STARTUP_CANCELLATION_TIMEOUT,
            termination_timeouts: self.termination_timeouts,
            armed: true,
        };
        let worker_control = control.clone();
        let worker = tauri::async_runtime::spawn_blocking(move || {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                match launcher.launch(&config) {
                    Ok(launched) => {
                        wait_for_bootstrap(launched, timeout, &worker_control.cancelled)
                    }
                    Err(error) => Err(StartupFailure::without_child(error)),
                }
            }))
            .unwrap_or_else(|_| {
                Err(StartupFailure::without_child(
                    "tugas peluncuran/bootstrap Tor panik".to_owned(),
                ))
            });
            worker_control.finish(outcome);
        });
        if let Err(error) = worker.await {
            control.finish(Err(StartupFailure::without_child(format!(
                "gagal menjalankan tugas peluncuran/bootstrap Tor: {error}"
            ))));
        }

        let mut starting = self.starting.lock();
        let owns_slot = starting
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(active, &control));
        if !owns_slot {
            lease.disarm();
            return Err("startup Tor dibatalkan oleh shutdown".to_owned());
        }
        let outcome = control.take_outcome().ok_or_else(|| {
            "tugas peluncuran/bootstrap Tor selesai tanpa hasil".to_owned()
        })?;
        if control.cancelled.load(Ordering::Acquire)
            || self.shutdown_epoch.load(Ordering::Acquire) != shutdown_epoch
        {
            let cancellation = "startup Tor dibatalkan oleh shutdown".to_owned();
            return match outcome {
                Ok(mut child) => match terminate_child(
                    &mut *child,
                    self.termination_timeouts,
                ) {
                    Ok(()) => {
                        starting.take();
                        lease.disarm();
                        Err(cancellation)
                    }
                    Err(error) => {
                        control.restore_child(cancellation.clone(), child);
                        lease.disarm();
                        Err(format!(
                            "{cancellation}; selain itu proses Tor gagal dihentikan: {error}"
                        ))
                    }
                },
                Err(mut failure) => {
                    let message = failure.message;
                    match failure.child.take() {
                        Some(mut child) => match terminate_child(
                            &mut *child,
                            self.termination_timeouts,
                        ) {
                            Ok(()) => {
                                starting.take();
                                lease.disarm();
                                Err(message)
                            }
                            Err(error) => {
                                control.restore_child(message.clone(), child);
                                lease.disarm();
                                Err(format!(
                                    "{message}; selain itu proses Tor gagal dihentikan: {error}"
                                ))
                            }
                        },
                        None => {
                            starting.take();
                            lease.disarm();
                            Err(message)
                        }
                    }
                }
            };
        }

        match outcome {
            Ok(child) => {
                *self.process.lock() = Some(RunningTor {
                    child,
                    endpoint: endpoint.clone(),
                });
                starting.take();
                lease.disarm();
                Ok(endpoint)
            }
            Err(mut failure) => {
                let message = failure.message;
                match failure.child.take() {
                    Some(mut child) => {
                        match terminate_child(&mut *child, self.termination_timeouts) {
                            Ok(()) => {
                                starting.take();
                                lease.disarm();
                                Err(message)
                            }
                            Err(error) => {
                                control.restore_child(message.clone(), child);
                                lease.disarm();
                                Err(format!(
                                    "{message}; selain itu proses Tor gagal dihentikan: {error}"
                                ))
                            }
                        }
                    }
                    None => {
                        starting.take();
                        lease.disarm();
                        Err(message)
                    }
                }
            }
        }
    }

    pub fn shutdown(&self) -> Result<(), String> {
        self.shutdown_epoch.fetch_add(1, Ordering::AcqRel);
        let mut errors = Vec::new();
        let starting = self.starting.lock().clone();
        if let Some(starting) = starting {
            match starting.cancel_and_wait(
                STARTUP_CANCELLATION_TIMEOUT,
                self.termination_timeouts,
            ) {
                Ok(()) => {
                    let mut slot = self.starting.lock();
                    if slot
                        .as_ref()
                        .is_some_and(|active| Arc::ptr_eq(active, &starting))
                    {
                        slot.take();
                    }
                }
                Err(error) => errors.push(format!("startup Tor gagal dihentikan: {error}")),
            }
        }
        {
            let mut process = self.process.lock();
            if let Some(running) = process.as_mut() {
                match terminate_child(&mut *running.child, self.termination_timeouts) {
                    Ok(()) => {
                        process.take();
                    }
                    Err(error) => errors.push(format!("proses Tor gagal dihentikan: {error}")),
                }
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}

impl Default for TorManager {
    fn default() -> Self {
        Self::new()
    }
}

fn allocate_loopback_port() -> Result<u16, String> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .map_err(|error| format!("gagal mengalokasikan port SOCKS loopback untuk Tor: {error}"))?;
    let port = listener
        .local_addr()
        .map_err(|error| format!("gagal membaca port SOCKS loopback Tor: {error}"))?
        .port();
    drop(listener);
    Ok(port)
}

fn wait_for_bootstrap(
    mut launched: LaunchedTor,
    timeout: Duration,
    cancelled: &AtomicBool,
) -> StartupOutcome {
    let started = Instant::now();
    let mut last_progress = None;

    loop {
        if cancelled.load(Ordering::Acquire) {
            return Err(StartupFailure::with_child(
                "startup Tor dibatalkan".to_owned(),
                launched.child,
            ));
        }
        match launched.child.try_wait() {
            Ok(Some(status)) => {
                if let Some(message) = queued_fatal_message(&launched.logs) {
                    return Err(StartupFailure::without_child(format!(
                        "Tor gagal bootstrap: {message}"
                    )));
                }
                return Err(StartupFailure::without_child(format!(
                    "Tor berhenti sebelum bootstrap selesai dengan status {status}"
                )));
            }
            Ok(None) => {}
            Err(error) => {
                return Err(StartupFailure::with_child(
                    format!("gagal memeriksa proses Tor saat bootstrap: {error}"),
                    launched.child,
                ));
            }
        }

        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            let progress = last_progress
                .as_deref()
                .map(|line| format!("; progres terakhir: {line}"))
                .unwrap_or_default();
            return Err(StartupFailure::with_child(
                format!("Tor bootstrap timeout setelah {timeout:?}{progress}"),
                launched.child,
            ));
        }

        match launched
            .logs
            .recv_timeout(remaining.min(Duration::from_millis(100)))
        {
            Ok(line) => match parse_bootstrap_line(&line) {
                BootstrapEvent::Progress(progress) => last_progress = Some(progress),
                BootstrapEvent::Ready if cancelled.load(Ordering::Acquire) => {
                    return Err(StartupFailure::with_child(
                        "startup Tor dibatalkan".to_owned(),
                        launched.child,
                    ));
                }
                BootstrapEvent::Ready => {
                    return match launched.child.try_wait() {
                        Ok(None) => Ok(launched.child),
                        Ok(Some(status)) => Err(StartupFailure::without_child(
                            queued_fatal_message(&launched.logs)
                                .map(|message| format!("Tor gagal bootstrap: {message}"))
                                .unwrap_or_else(|| {
                                    format!(
                                        "Tor berhenti saat menyelesaikan bootstrap dengan status {status}"
                                    )
                                }),
                        )),
                        Err(error) => Err(StartupFailure::with_child(
                            format!(
                                "gagal memeriksa proses Tor setelah bootstrap selesai: {error}"
                            ),
                            launched.child,
                        )),
                    };
                }
                BootstrapEvent::Fatal(message) => {
                    return Err(StartupFailure::with_child(
                        format!("Tor gagal bootstrap: {message}"),
                        launched.child,
                    ));
                }
                BootstrapEvent::Ignore => {}
            },
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return match launched.child.try_wait() {
                    Ok(Some(status)) => Err(StartupFailure::without_child(format!(
                        "Tor berhenti sebelum bootstrap selesai dengan status {status}"
                    ))),
                    Ok(None) => Err(StartupFailure::with_child(
                        "stream log Tor ditutup sebelum bootstrap selesai".to_owned(),
                        launched.child,
                    )),
                    Err(error) => Err(StartupFailure::with_child(
                        format!(
                            "stream log Tor ditutup dan status proses gagal diperiksa: {error}"
                        ),
                        launched.child,
                    )),
                };
            }
        }
    }
}

fn queued_fatal_message(logs: &mpsc::Receiver<String>) -> Option<String> {
    logs.try_iter().find_map(|line| match parse_bootstrap_line(&line) {
        BootstrapEvent::Fatal(message) => Some(message),
        _ => None,
    })
}


fn wait_for_exit(
    child: &mut dyn ManagedTorChild,
    timeout: Duration,
    poll: Duration,
) -> Result<bool, String> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return Ok(true),
            Ok(None) if Instant::now() >= deadline => return Ok(false),
            Ok(None) => {
                std::thread::sleep(poll.min(deadline.saturating_duration_since(Instant::now())))
            }
            Err(error) => return Err(format!("reap failed: {error}")),
        }
    }
}

fn terminate_child(
    child: &mut dyn ManagedTorChild,
    timeouts: TerminationTimeouts,
) -> Result<(), String> {
    match child.try_wait() {
        Ok(Some(_)) => return Ok(()),
        Ok(None) => {}
        Err(error) => return Err(format!("initial reap check failed: {error}")),
    }

    child
        .force_kill()
        .map_err(|error| format!("force kill failed: {error}"))?;

    match wait_for_exit(child, timeouts.reap, timeouts.poll) {
        Ok(true) => Ok(()),
        Ok(false) => Err(format!(
            "force kill succeeded but reap timed out after {:?}",
            timeouts.reap
        )),
        Err(error) => Err(format!("force kill succeeded but {error}")),
    }
}


struct StdTorLauncher;

impl TorLauncher for StdTorLauncher {
    fn launch(&self, config: &TorLaunchConfig) -> Result<LaunchedTor, String> {
        let mut command = Command::new(&config.paths.exe);
        command
            .args(config.args())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }

        let mut child = command.spawn().map_err(|error| {
            format!(
                "gagal menjalankan bundled Tor {}: {error}",
                config.paths.exe.display()
            )
        })?;
        let (sender, logs) = mpsc::channel();
        let Some(stdout) = child.stdout.take() else {
            let _ = sender.send(
                "[err] gagal menangkap stdout dari proses bundled Tor".to_owned(),
            );
            return Ok(LaunchedTor {
                child: Box::new(StdManagedTorChild { child }),
                logs,
            });
        };
        let Some(stderr) = child.stderr.take() else {
            let _ = sender.send(
                "[err] gagal menangkap stderr dari proses bundled Tor".to_owned(),
            );
            return Ok(LaunchedTor {
                child: Box::new(StdManagedTorChild { child }),
                logs,
            });
        };

        if let Err(error) = spawn_log_reader("stdout", stdout, sender.clone()) {
            let _ = sender.send(format!(
                "[err] gagal memulai pembaca stdout Tor: {error}"
            ));
            return Ok(LaunchedTor {
                child: Box::new(StdManagedTorChild { child }),
                logs,
            });
        }
        if let Err(error) = spawn_log_reader("stderr", stderr, sender.clone()) {
            let _ = sender.send(format!(
                "[err] gagal memulai pembaca stderr Tor: {error}"
            ));
            return Ok(LaunchedTor {
                child: Box::new(StdManagedTorChild { child }),
                logs,
            });
        }
        drop(sender);

        Ok(LaunchedTor {
            child: Box::new(StdManagedTorChild { child }),
            logs,
        })
    }
}


fn spawn_log_reader<R: Read + Send + 'static>(
    stream: &'static str,
    reader: R,
    sender: mpsc::Sender<String>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name(format!("tor-{stream}"))
        .spawn(move || {
            for line in BufReader::new(reader).lines() {
                match line {
                    Ok(line) => {
                        if sender.send(line).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = sender.send(format!(
                            "[err] gagal membaca {stream} Tor: {error}"
                        ));
                        break;
                    }
                }
            }
        })
}


struct StdManagedTorChild {
    child: Child,
}

impl ManagedTorChild for StdManagedTorChild {
    fn try_wait(&mut self) -> Result<Option<ExitStatus>, String> {
        self.child
            .try_wait()
            .map_err(|error| format!("gagal memeriksa status proses Tor: {error}"))
    }


    fn force_kill(&mut self) -> Result<(), String> {
        self.child
            .kill()
            .map_err(|error| format!("gagal melakukan force kill proses Tor: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use parking_lot::Mutex;
    use std::sync::Arc;
    use std::time::Duration;

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    fn temp_runtime(label: &str) -> PathBuf {
        let unique = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "xix-tor-{label}-{}-{unique}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn create_runtime(root: &Path) {
        fs::create_dir_all(root.join("tor")).unwrap();
        fs::create_dir_all(root.join("data")).unwrap();
        fs::write(root.join("tor/tor.exe"), []).unwrap();
        fs::write(root.join("data/geoip"), []).unwrap();
        fs::write(root.join("data/geoip6"), []).unwrap();
    }

    fn create_partial_runtime_without_geoip6(root: &Path) {
        fs::create_dir_all(root.join("tor")).unwrap();
        fs::create_dir_all(root.join("data")).unwrap();
        fs::write(root.join("tor/tor.exe"), []).unwrap();
        fs::write(root.join("data/geoip"), []).unwrap();
    }

    #[test]
    fn resolver_prefers_packaged_resource_layout() {
        let root = temp_runtime("packaged");
        create_runtime(&root.join("tor-runtime"));

        let paths = resolve_runtime(Some(&root), Path::new("missing-cwd")).unwrap();

        assert!(paths.exe.ends_with("tor-runtime/tor/tor.exe"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resolver_finds_development_runtime_layout() {
        let cwd = temp_runtime("development");
        create_runtime(&cwd.join("desktop/src-tauri/tor-runtime"));

        let paths = resolve_runtime(None, &cwd).unwrap();

        assert_eq!(paths.exe, cwd.join("desktop/src-tauri/tor-runtime/tor/tor.exe"));
        fs::remove_dir_all(cwd).unwrap();
    }

    #[test]
    fn resolver_skips_incomplete_higher_priority_candidate() {
        let cwd = temp_runtime("fallback");
        let resource_root = cwd.join("resources");
        create_partial_runtime_without_geoip6(&resource_root.join("tor-runtime"));
        create_runtime(&cwd.join("src-tauri/tor-runtime"));

        let paths = resolve_runtime(Some(&resource_root), &cwd).unwrap();

        assert_eq!(paths.exe, cwd.join("src-tauri/tor-runtime/tor/tor.exe"));
        fs::remove_dir_all(cwd).unwrap();
    }

    #[test]
    fn resolver_reports_missing_geoip6() {
        let root = temp_runtime("missing-geoip6");
        create_partial_runtime_without_geoip6(&root.join("tor-runtime"));

        let err = resolve_runtime(Some(&root), Path::new("missing-cwd")).unwrap_err();

        assert!(err.contains("geoip6"), "{err}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resolver_reports_all_attempted_roots_when_runtime_is_absent() {
        let cwd = temp_runtime("absent");
        let resource_root = cwd.join("resources");

        let err = resolve_runtime(Some(&resource_root), &cwd).unwrap_err();

        assert!(err.contains("bundled Tor runtime tidak ditemukan"), "{err}");
        assert!(err.contains(&resource_root.join("tor-runtime").display().to_string()), "{err}");
        assert!(err.contains(&cwd.join("src-tauri/tor-runtime").display().to_string()), "{err}");
        assert!(err.contains(&cwd.join("tor-runtime").display().to_string()), "{err}");
        assert!(err.contains(&cwd.join("desktop/src-tauri/tor-runtime").display().to_string()), "{err}");
        fs::remove_dir_all(cwd).unwrap();
    }

    #[test]
    fn bootstrap_parser_recognizes_ready() {
        assert_eq!(
            parse_bootstrap_line("Bootstrapped 100% (done): Done"),
            BootstrapEvent::Ready
        );
    }

    #[test]
    fn bootstrap_parser_keeps_actionable_fatal_message() {
        assert_eq!(
            parse_bootstrap_line("[err] Failed to bind one of the listener ports."),
            BootstrapEvent::Fatal("Failed to bind one of the listener ports.".into())
        );
    }

    #[test]
    fn bootstrap_parser_reports_progress_and_ignores_unrelated_lines() {
        assert_eq!(
            parse_bootstrap_line("Bootstrapped 45% (requesting_descriptors): Asking for relay descriptors"),
            BootstrapEvent::Progress(
                "Bootstrapped 45% (requesting_descriptors): Asking for relay descriptors".into()
            )
        );
        assert_eq!(
            parse_bootstrap_line("Opening Socks listener on 127.0.0.1:19050"),
            BootstrapEvent::Ignore
        );
    }

    #[test]
    fn launch_config_uses_loopback_socks_and_bundled_geoip_files() {
        let config = TorLaunchConfig {
            paths: TorRuntimePaths {
                exe: PathBuf::from("runtime/tor/tor.exe"),
                geoip: PathBuf::from("runtime/data/geoip"),
                geoip6: PathBuf::from("runtime/data/geoip6"),
            },
            data_dir: PathBuf::from("writable/tor-data"),
            socks_port: 19050,
        };

        assert_eq!(
            config.args(),
            [
                "--ClientOnly",
                "1",
                "--SocksPort",
                "127.0.0.1:19050",
                "--DataDirectory",
                "writable/tor-data",
                "--GeoIPFile",
                "runtime/data/geoip",
                "--GeoIPv6File",
                "runtime/data/geoip6",
                "--AvoidDiskWrites",
                "1",
                "--Log",
                "notice stdout",
            ]
        );
    }

    struct FakeChildState {
        exited: AtomicBool,
        killed: AtomicBool,
        waited: AtomicBool,
        force_killed: AtomicBool,
        exit_on_force: AtomicBool,
        fail_force_kill: AtomicBool,
        fail_wait: AtomicBool,
    }

    impl Default for FakeChildState {
        fn default() -> Self {
            Self {
                exited: AtomicBool::new(false),
                killed: AtomicBool::new(false),
                waited: AtomicBool::new(false),
                force_killed: AtomicBool::new(false),
                exit_on_force: AtomicBool::new(true),
                fail_force_kill: AtomicBool::new(false),
                fail_wait: AtomicBool::new(false),
            }
        }
    }

    struct FakeChild {
        state: Arc<FakeChildState>,
    }

    impl FakeChild {
        fn running() -> Self {
            Self {
                state: Arc::new(FakeChildState::default()),
            }
        }

        fn exited() -> Self {
            let child = Self::running();
            child.state.exited.store(true, Ordering::SeqCst);
            child
        }



        fn failing_force_kill() -> Self {
            let child = Self::running();
            child
                .state
                .fail_force_kill
                .store(true, Ordering::SeqCst);
            child
        }

        fn failing_wait() -> Self {
            let child = Self::running();
            child.state.fail_wait.store(true, Ordering::SeqCst);
            child
        }

        fn flags(&self) -> Arc<FakeChildState> {
            self.state.clone()
        }
    }

    impl ManagedTorChild for FakeChild {
        fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>, String> {
            if self.state.fail_wait.load(Ordering::SeqCst)
                && self.state.force_killed.load(Ordering::SeqCst)
            {
                return Err("injected wait failure".into());
            }
            if self.state.exited.load(Ordering::SeqCst) {
                self.state.waited.store(true, Ordering::SeqCst);
                use std::os::windows::process::ExitStatusExt;
                Ok(Some(std::process::ExitStatus::from_raw(0)))
            } else {
                Ok(None)
            }
        }


        fn force_kill(&mut self) -> Result<(), String> {
            self.state.force_killed.store(true, Ordering::SeqCst);
            self.state.killed.store(true, Ordering::SeqCst);
            if self.state.fail_force_kill.load(Ordering::SeqCst) {
                return Err("injected force kill failure".into());
            }
            if self.state.exit_on_force.load(Ordering::SeqCst) {
                self.state.exited.store(true, Ordering::SeqCst);
            }
            Ok(())
        }
    }


    #[derive(Clone)]
    enum FakeLaunchBehavior {
        Lines {
            delay: Duration,
            lines: Vec<String>,
        },
        Silent {
            hold_channel_for: Duration,
        },
    }

    struct FakeLauncher {
        launches: AtomicUsize,
        behavior: FakeLaunchBehavior,
        children: Mutex<Vec<Arc<FakeChildState>>>,
    }

    impl FakeLauncher {
        fn ready() -> Arc<Self> {
            Self::ready_after(Duration::ZERO)
        }

        fn ready_after(delay: Duration) -> Arc<Self> {
            Arc::new(Self {
                launches: AtomicUsize::new(0),
                behavior: FakeLaunchBehavior::Lines {
                    delay,
                    lines: vec!["Bootstrapped 100% (done): Done".into()],
                },
                children: Mutex::new(Vec::new()),
            })
        }

        fn fatal(message: &str) -> Arc<Self> {
            Arc::new(Self {
                launches: AtomicUsize::new(0),
                behavior: FakeLaunchBehavior::Lines {
                    delay: Duration::ZERO,
                    lines: vec![format!("[err] {message}")],
                },
                children: Mutex::new(Vec::new()),
            })
        }

        fn silent(hold_channel_for: Duration) -> Arc<Self> {
            Arc::new(Self {
                launches: AtomicUsize::new(0),
                behavior: FakeLaunchBehavior::Silent { hold_channel_for },
                children: Mutex::new(Vec::new()),
            })
        }

        fn launch_count(&self) -> usize {
            self.launches.load(Ordering::SeqCst)
        }

        fn last_child(&self) -> Arc<FakeChildState> {
            self.children.lock().last().unwrap().clone()
        }
    }

    impl TorLauncher for FakeLauncher {
        fn launch(&self, _config: &TorLaunchConfig) -> Result<LaunchedTor, String> {
            self.launches.fetch_add(1, Ordering::SeqCst);
            let child = FakeChild::running();
            self.children.lock().push(child.flags());
            let (sender, logs) = std::sync::mpsc::channel();
            let behavior = self.behavior.clone();
            std::thread::spawn(move || match behavior {
                FakeLaunchBehavior::Lines { delay, lines } => {
                    std::thread::sleep(delay);
                    for line in lines {
                        if sender.send(line).is_err() {
                            break;
                        }
                    }
                }
                FakeLaunchBehavior::Silent { hold_channel_for } => {
                    std::thread::sleep(hold_channel_for);
                    drop(sender);
                }
            });
            Ok(LaunchedTor {
                child: Box::new(child),
                logs,
            })
        }
    }

    fn test_paths() -> TorRuntimePaths {
        TorRuntimePaths {
            exe: PathBuf::from("test-runtime/tor/tor.exe"),
            geoip: PathBuf::from("test-runtime/data/geoip"),
            geoip6: PathBuf::from("test-runtime/data/geoip6"),
        }
    }

    fn temp_data() -> PathBuf {
        temp_runtime("data").join("tor-data")
    }

    #[tokio::test]
    async fn ensure_ready_is_idempotent() {
        let launcher = FakeLauncher::ready();
        let manager = TorManager::new_with(launcher.clone(), Duration::from_secs(1));
        let data_dir = temp_data();

        let first = manager
            .ensure_ready(test_paths(), data_dir.clone())
            .await
            .unwrap();
        let second = manager
            .ensure_ready(test_paths(), data_dir.clone())
            .await
            .unwrap();

        assert_eq!(first, second);
        assert_eq!(launcher.launch_count(), 1);
        assert!(data_dir.is_dir());
        manager.shutdown().unwrap();
    }

    #[tokio::test]
    async fn concurrent_ensure_ready_starts_one_child() {
        let launcher = FakeLauncher::ready_after(Duration::from_millis(20));
        let manager = Arc::new(TorManager::new_with(
            launcher.clone(),
            Duration::from_secs(1),
        ));

        let (a, b) = tokio::join!(
            manager.ensure_ready(test_paths(), temp_data()),
            manager.ensure_ready(test_paths(), temp_data())
        );

        assert_eq!(a.unwrap(), b.unwrap());
        assert_eq!(launcher.launch_count(), 1);
        manager.shutdown().unwrap();
    }

    #[tokio::test]
    async fn cancelled_startup_kills_child_and_allows_retry() {
        let launcher = FakeLauncher::ready_after(Duration::from_millis(250));
        let manager = Arc::new(TorManager::new_with(
            launcher.clone(),
            Duration::from_secs(1),
        ));
        let starting_manager = manager.clone();
        let startup = tokio::spawn(async move {
            starting_manager
                .ensure_ready(test_paths(), temp_data())
                .await
        });
        while launcher.launch_count() == 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }

        startup.abort();
        assert!(startup.await.unwrap_err().is_cancelled());

        let cancelled_child = launcher.last_child();
        assert!(cancelled_child.killed.load(Ordering::SeqCst));
        assert!(cancelled_child.waited.load(Ordering::SeqCst));

        manager
            .ensure_ready(test_paths(), temp_data())
            .await
            .unwrap();
        assert_eq!(launcher.launch_count(), 2);
        manager.shutdown().unwrap();
    }

    #[tokio::test]
    async fn shutdown_during_bootstrap_kills_child_and_allows_retry() {
        let launcher = FakeLauncher::ready_after(Duration::from_millis(250));
        let manager = Arc::new(TorManager::new_with(
            launcher.clone(),
            Duration::from_secs(1),
        ));
        let starting_manager = manager.clone();
        let startup = tokio::spawn(async move {
            starting_manager
                .ensure_ready(test_paths(), temp_data())
                .await
        });
        while launcher.launch_count() == 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }

        let shutdown_manager = manager.clone();
        tokio::task::spawn_blocking(move || shutdown_manager.shutdown())
            .await
            .unwrap()
            .unwrap();

        let stopped_child = launcher.last_child();
        assert!(stopped_child.killed.load(Ordering::SeqCst));
        assert!(stopped_child.waited.load(Ordering::SeqCst));
        assert!(startup.await.unwrap().is_err());

        manager
            .ensure_ready(test_paths(), temp_data())
            .await
            .unwrap();
        assert_eq!(launcher.launch_count(), 2);
        manager.shutdown().unwrap();
    }


    #[test]
    fn startup_cancellation_kill_failure_retains_child_for_retry() {
        let child = FakeChild::failing_force_kill();
        let flags = child.flags();
        let control = Arc::new(StartupControl::new());
        control.finish(Ok(Box::new(child)));
        let manager = TorManager::new();
        *manager.starting.lock() = Some(control);

        let error = manager.shutdown().unwrap_err();

        assert!(error.contains("force kill failed"), "{error}");
        assert!(manager.starting.lock().is_some());
        flags.fail_force_kill.store(false, Ordering::SeqCst);
        manager.shutdown().unwrap();
        assert!(flags.waited.load(Ordering::SeqCst));
        assert!(manager.starting.lock().is_none());
    }

    #[tokio::test]
    async fn stale_exited_child_is_restarted() {
        let launcher = FakeLauncher::ready();
        let manager = TorManager::new_with(launcher.clone(), Duration::from_secs(1));
        *manager.process.lock() = Some(RunningTor {
            child: Box::new(FakeChild::exited()),
            endpoint: "socks5://127.0.0.1:1".into(),
        });

        let endpoint = manager
            .ensure_ready(test_paths(), temp_data())
            .await
            .unwrap();

        assert_ne!(endpoint, "socks5://127.0.0.1:1");
        assert_eq!(launcher.launch_count(), 1);
        manager.shutdown().unwrap();
    }

    #[tokio::test]
    async fn bootstrap_timeout_is_bounded_and_kills_child() {
        let launcher = FakeLauncher::silent(Duration::from_millis(250));
        let manager = TorManager::new_with(launcher.clone(), Duration::from_millis(30));

        let err = manager
            .ensure_ready(test_paths(), temp_data())
            .await
            .unwrap_err();

        assert!(err.contains("timeout"), "{err}");
        let child = launcher.last_child();
        assert!(child.killed.load(Ordering::SeqCst));
        assert!(child.waited.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn bootstrap_fatal_error_is_actionable_and_kills_child() {
        let launcher = FakeLauncher::fatal("Failed to bind one of the listener ports.");
        let manager = TorManager::new_with(launcher.clone(), Duration::from_secs(1));

        let err = manager
            .ensure_ready(test_paths(), temp_data())
            .await
            .unwrap_err();

        assert!(
            err.contains("Failed to bind one of the listener ports."),
            "{err}"
        );
        let child = launcher.last_child();
        assert!(child.killed.load(Ordering::SeqCst));
        assert!(child.waited.load(Ordering::SeqCst));
    }

    #[test]
    fn exited_child_prefers_queued_fatal_log_detail() {
        let child = FakeChild::exited();
        let (sender, logs) = std::sync::mpsc::channel();
        sender
            .send("[err] Failed to bind one of the listener ports.".into())
            .unwrap();
        drop(sender);
        let cancelled = AtomicBool::new(false);

        let err = match wait_for_bootstrap(
            LaunchedTor {
                child: Box::new(child),
                logs,
            },
            Duration::from_secs(1),
            &cancelled,
        ) {
            Err(error) => error,
            Ok(_) => panic!("exited child unexpectedly reached ready state"),
        };

        assert!(
            err.message
                .contains("Failed to bind one of the listener ports."),
            "{}",
            err.message
        );
    }

    #[tokio::test]
    async fn bootstrap_wait_does_not_hold_process_mutex() {
        let launcher = FakeLauncher::ready_after(Duration::from_millis(150));
        let manager = Arc::new(TorManager::new_with(
            launcher.clone(),
            Duration::from_secs(1),
        ));
        let starting_manager = manager.clone();
        let startup = tokio::spawn(async move {
            starting_manager
                .ensure_ready(test_paths(), temp_data())
                .await
        });
        while launcher.launch_count() == 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }

        assert!(
            manager.process.try_lock().is_some(),
            "process mutex was held while bootstrap waited for log lines"
        );

        startup.await.unwrap().unwrap();
        manager.shutdown().unwrap();
    }
    #[test]
    fn shutdown_directly_force_kills_and_reaps_owned_child() {
        let child = FakeChild::running();
        let flags = child.flags();
        let manager =
            TorManager::with_ready_child(child, "socks5://127.0.0.1:19050".into());

        manager.shutdown().unwrap();

        assert!(flags.force_killed.load(Ordering::SeqCst));
        assert!(flags.waited.load(Ordering::SeqCst));
        assert!(manager.process.lock().is_none());
    }

    #[test]
    fn shutdown_reap_timeout_retains_child_for_retry() {
        let child = FakeChild::running();
        child.state.exit_on_force.store(false, Ordering::SeqCst);
        let flags = child.flags();
        let manager =
            TorManager::with_ready_child(child, "socks5://127.0.0.1:19050".into());

        let error = manager.shutdown().unwrap_err();

        assert!(error.contains("timed out"), "{error}");
        assert!(manager.process.lock().is_some());
        flags.exit_on_force.store(true, Ordering::SeqCst);
        manager.shutdown().unwrap();
        assert!(flags.waited.load(Ordering::SeqCst));
        assert!(manager.process.lock().is_none());
    }

    #[test]
    fn shutdown_reports_force_kill_failure_and_retains_child_for_retry() {
        let child = FakeChild::failing_force_kill();
        let flags = child.flags();
        let manager =
            TorManager::with_ready_child(child, "socks5://127.0.0.1:19050".into());

        let error = manager.shutdown().unwrap_err();

        assert!(error.contains("force"), "{error}");
        assert!(manager.process.lock().is_some());
        flags.fail_force_kill.store(false, Ordering::SeqCst);
        manager.shutdown().unwrap();
        assert!(manager.process.lock().is_none());
    }

    #[test]
    fn shutdown_reports_wait_failure_and_retains_child_for_retry() {
        let child = FakeChild::failing_wait();
        let flags = child.flags();
        let manager =
            TorManager::with_ready_child(child, "socks5://127.0.0.1:19050".into());

        let error = manager.shutdown().unwrap_err();

        assert!(error.contains("wait"), "{error}");
        assert!(flags.force_killed.load(Ordering::SeqCst));
        assert!(manager.process.lock().is_some());
        flags.fail_wait.store(false, Ordering::SeqCst);
        manager.shutdown().unwrap();
        assert!(flags.waited.load(Ordering::SeqCst));
        assert!(manager.process.lock().is_none());
    }
}
