//! Non-blocking execution of script-initiated resource loads.
//!
//! A script fetch is split into three phases so the caller's thread
//! never blocks on the network:
//!
//! 1. PREPARE (caller's thread): URL resolution, the remote-page
//!    `file://` gate and the Cookie header — read-only session state,
//!    see [`crate::Session::prepare_request`]. The prepared
//!    [`ResourceRequest`] is `Send` and may cross to a worker.
//! 2. EXECUTE (here): the blocking `loader.load` runs inline, on a
//!    short-lived worker thread, or parked for a test to drive —
//!    strategy picked at construction.
//! 3. APPLY (caller's thread): the drained completion stores its
//!    `Set-Cookie` headers in the jar ([`crate::Session::store_response_cookies`])
//!    and settles the waiting promise / XHR / module load.
//!
//! Session state stays single-threaded; only prepared requests and
//! finished responses cross thread boundaries. An optional wake hook
//! fires (on the worker) when a threaded completion lands, so the
//! shell can schedule the drain instead of polling.

use lumen_platform::{LoadError, ResourceLoader, ResourceRequest, ResourceResponse};
use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};

/// A fetch/XHR load that finished executing.
struct Completion {
    id: u64,
    result: Result<ResourceResponse, LoadError>,
}

/// How submitted loads run.
enum Executor {
    /// On the submitting thread, before `submit` returns — deterministic,
    /// so tests and non-event-loop embedders keep the legacy feel.
    Inline,
    /// One short-lived thread per load; the submitting thread never
    /// blocks, the wake hook announces each completion.
    Threads,
    /// Parked until [`NetworkQueue::drive`] runs the backlog inline —
    /// lets a test observe the pending window between submit and
    /// completion. Do not combine with runtime module loads: a parked
    /// module load keeps the script job executor napping forever.
    Manual,
}

/// A load parked in [`Executor::Manual`] mode.
enum Parked {
    Fetch {
        id: u64,
        request: ResourceRequest,
        loader: Arc<dyn ResourceLoader + Send + Sync>,
    },
    Module {
        request: ResourceRequest,
        loader: Arc<dyn ResourceLoader + Send + Sync>,
        slot: ModuleSlot,
    },
}

/// Shared cell a module-load future polls until the worker delivers
/// the source text (or the error).
pub(crate) type ModuleSlot = Arc<Mutex<Option<Result<String, String>>>>;

/// Runs the blocking load, turning a loader panic into an error so a
/// completion (or module slot) is always delivered — a panicking
/// loader must never strand a waiting promise.
fn run_load(
    loader: &Arc<dyn ResourceLoader + Send + Sync>,
    request: &ResourceRequest,
) -> Result<ResourceResponse, LoadError> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| loader.load(request)))
        .unwrap_or_else(|_| Err(LoadError::Http("loader panicked".to_string())))
}

/// Maps a finished load to the module-source form: body text, error
/// string. `Set-Cookie` headers of module responses are dropped — the
/// jar lives on the caller's thread and module loads complete inside
/// the script job executor, where no session is reachable.
fn module_outcome(result: Result<ResourceResponse, LoadError>) -> Result<String, String> {
    result
        .map(|response| response.text())
        .map_err(|error| error.to_string())
}

/// The hook workers call after each threaded completion. Shared with
/// in-flight workers so a hook registered AFTER a submit still fires
/// for it — the registration order (hook vs. first fetch during page
/// setup) must not matter.
type WakeHook = Arc<Mutex<Option<Arc<dyn Fn() + Send + Sync>>>>;

/// Fires the registered wake hook, if any.
fn wake_now(wake: &WakeHook) {
    let hook = wake.lock().expect("wake hook").clone();
    if let Some(hook) = hook {
        hook();
    }
}

/// The queue behind `PageScripts`: submits prepared requests for
/// execution and hands completions back for settlement. Not `Sync` —
/// it lives next to its `PageScripts` on the shell's thread; only the
/// completion channel and the wake hook cross to workers.
pub struct NetworkQueue {
    next_id: Cell<u64>,
    executor: Executor,
    tx: mpsc::Sender<Completion>,
    rx: mpsc::Receiver<Completion>,
    parked: RefCell<Vec<Parked>>,
    loader: RefCell<Option<Arc<dyn ResourceLoader + Send + Sync>>>,
    wake: WakeHook,
    /// Completions sent by workers but not yet drained — registration
    /// of a late wake hook re-fires for these (see `set_wake_hook`).
    undrained: Arc<AtomicUsize>,
    /// Module loads submitted but not yet delivered. The script job
    /// executor keeps polling (with a nap) while this is non-zero,
    /// because a parked dynamic-`import()` future can only make
    /// progress once the worker lands the source.
    module_in_flight: Arc<AtomicUsize>,
}

impl NetworkQueue {
    /// Executes each submitted load on the submitting thread.
    #[must_use]
    pub fn inline() -> Self {
        Self::new(Executor::Inline)
    }

    /// Executes each submitted load on its own short-lived thread.
    #[must_use]
    pub fn threaded() -> Self {
        Self::new(Executor::Threads)
    }

    /// Parks submitted loads until [`Self::drive`] runs them inline.
    #[must_use]
    pub fn manual() -> Self {
        Self::new(Executor::Manual)
    }

    fn new(executor: Executor) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            next_id: Cell::new(0),
            executor,
            tx,
            rx,
            parked: RefCell::new(Vec::new()),
            loader: RefCell::new(None),
            wake: Arc::new(Mutex::new(None)),
            undrained: Arc::new(AtomicUsize::new(0)),
            module_in_flight: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Registers the hook a worker calls after each threaded
    /// completion — the shell's "please drain me" signal (e.g. an
    /// event-loop proxy). Never called by the inline/manual strategies,
    /// whose completions are drained by the same pump that submitted.
    /// Registering after a submit still covers that submit's worker,
    /// and completions that already landed fire the hook once here so
    /// they cannot sit in the channel unnoticed.
    pub fn set_wake_hook(&self, wake: impl Fn() + Send + Sync + 'static) {
        let hook: Arc<dyn Fn() + Send + Sync> = Arc::new(wake);
        *self.wake.lock().expect("wake hook") = Some(hook.clone());
        if self.undrained.load(Ordering::SeqCst) > 0 {
            hook();
        }
    }

    /// The loader prepared requests execute against (the session's,
    /// shared read-only). Set by `PageScripts` before any submit.
    pub(crate) fn set_loader(&self, loader: Arc<dyn ResourceLoader + Send + Sync>) {
        *self.loader.borrow_mut() = Some(loader);
    }

    /// The module-load in-flight counter shared with the script job
    /// executor (see the field docs).
    pub(crate) fn module_activity(&self) -> Arc<AtomicUsize> {
        self.module_in_flight.clone()
    }

    fn loader(&self) -> Arc<dyn ResourceLoader + Send + Sync> {
        self.loader
            .borrow()
            .clone()
            .expect("PageScripts sets the loader before any submit")
    }

    /// Submits a prepared fetch/XHR request; the id pairs the eventual
    /// [`Self::drain`] completion with its waiting promise/XHR.
    pub(crate) fn submit(&self, request: ResourceRequest) -> u64 {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        let loader = self.loader();
        match self.executor {
            Executor::Inline => {
                let result = run_load(&loader, &request);
                let _ = self.tx.send(Completion { id, result });
                self.undrained.fetch_add(1, Ordering::SeqCst);
            }
            Executor::Threads => {
                let tx = self.tx.clone();
                let wake = self.wake.clone();
                let undrained = self.undrained.clone();
                std::thread::spawn(move || {
                    let result = run_load(&loader, &request);
                    let _ = tx.send(Completion { id, result });
                    undrained.fetch_add(1, Ordering::SeqCst);
                    wake_now(&wake);
                });
            }
            Executor::Manual => self
                .parked
                .borrow_mut()
                .push(Parked::Fetch { id, request, loader }),
        }
        id
    }

    /// Submits a prepared module-source request; the returned slot
    /// fills when the load lands. Completions bypass [`Self::drain`] —
    /// the loader future polls the slot from inside the job executor.
    pub(crate) fn submit_module(&self, request: ResourceRequest) -> ModuleSlot {
        let slot: ModuleSlot = Arc::new(Mutex::new(None));
        let loader = self.loader();
        self.module_in_flight.fetch_add(1, Ordering::SeqCst);
        match self.executor {
            Executor::Inline => {
                *slot.lock().expect("fresh slot") = Some(module_outcome(run_load(&loader, &request)));
                self.module_in_flight.fetch_sub(1, Ordering::SeqCst);
            }
            Executor::Threads => {
                let wake = self.wake.clone();
                let slot_for_worker = slot.clone();
                let counter = self.module_in_flight.clone();
                std::thread::spawn(move || {
                    *slot_for_worker.lock().expect("worker slot") =
                        Some(module_outcome(run_load(&loader, &request)));
                    counter.fetch_sub(1, Ordering::SeqCst);
                    wake_now(&wake);
                });
            }
            Executor::Manual => self.parked.borrow_mut().push(Parked::Module {
                request,
                loader,
                slot: slot.clone(),
            }),
        }
        slot
    }

    /// Drains every finished fetch/XHR completion (non-blocking).
    pub(crate) fn drain(&self) -> Vec<(u64, Result<ResourceResponse, LoadError>)> {
        let mut completions = Vec::new();
        while let Ok(completion) = self.rx.try_recv() {
            completions.push((completion.id, completion.result));
        }
        self.undrained
            .fetch_sub(completions.len(), Ordering::SeqCst);
        completions
    }

    /// Runs the parked backlog inline (manual executor): parked fetches
    /// land in the drain channel, parked module loads fill their slots.
    pub fn drive(&self) {
        let parked = std::mem::take(&mut *self.parked.borrow_mut());
        for item in parked {
            match item {
                Parked::Fetch {
                    id,
                    request,
                    loader,
                } => {
                    let _ = self.tx.send(Completion {
                        id,
                        result: run_load(&loader, &request),
                    });
                    self.undrained.fetch_add(1, Ordering::SeqCst);
                }
                Parked::Module {
                    request,
                    loader,
                    slot,
                } => {
                    *slot.lock().expect("drive slot") =
                        Some(module_outcome(run_load(&loader, &request)));
                    self.module_in_flight.fetch_sub(1, Ordering::SeqCst);
                }
            }
        }
    }
}
