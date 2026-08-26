#![deny(unsafe_code)]

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use bamts_cancel::CancellationToken;
use bamts_cli::cli::cli_outcome_in_context_with_cancel;
use bamts_cli::context::ExecutionContext;
use napi::bindgen_prelude::{Buffer, Object};
use napi::{Env, Error, JsDeferred, Result, Status};
use napi_derive::napi;

const QUEUE_CAPACITY: usize = 2;

type OutcomeResolver = Box<dyn FnOnce(Env) -> Result<RunOutcome> + Send>;
type Deferred = JsDeferred<RunOutcome, OutcomeResolver>;

#[napi(object)]
pub struct RunRequest {
    pub args: Vec<Buffer>,
    pub cwd: Buffer,
    pub env: Vec<Buffer>,
}

#[napi(object)]
pub struct RunOutcome {
    pub exit_code: i32,
    pub stdout: Buffer,
    pub stderr: Buffer,
    pub truncation: Option<RunTruncation>,
}

#[napi(object)]
pub struct RunTruncation {
    pub elided: u32,
    pub limit: u32,
}

#[napi(object)]
pub struct ReleaseMetadata {
    pub package_version: String,
    pub source_commit: String,
    pub build_set_id: String,
    pub release_id: String,
    pub target: String,
    pub artifact_kind: String,
    pub native_abi: u32,
    pub cli_protocol: u32,
}

struct RawRequest {
    args: Vec<Vec<u8>>,
    cwd: Vec<u8>,
    env: Vec<Vec<u8>>,
}

struct Job {
    request: RawRequest,
    deferred: Deferred,
}

struct Shared {
    closing: AtomicBool,
    active: Mutex<Option<CancellationToken>>,
}

struct Executor {
    shared: Arc<Shared>,
    sender: Mutex<Option<SyncSender<Job>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl Executor {
    fn start() -> Result<Arc<Self>> {
        let (sender, receiver) = sync_channel(QUEUE_CAPACITY);
        let shared = Arc::new(Shared {
            closing: AtomicBool::new(false),
            active: Mutex::new(None),
        });
        let worker_shared = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("bamti-napi".to_owned())
            .spawn(move || worker_loop(worker_shared, receiver))
            .map_err(|error| Error::new(Status::GenericFailure, error.to_string()))?;
        Ok(Arc::new(Self {
            shared,
            sender: Mutex::new(Some(sender)),
            worker: Mutex::new(Some(worker)),
        }))
    }

    fn submit(&self, job: Job) {
        let sender = self.sender.lock().expect("executor sender lock poisoned");
        if self.shared.closing.load(Ordering::Acquire) {
            drop(job);
            return;
        }
        let result = match sender.as_ref() {
            Some(sender) => sender.try_send(job),
            None => Err(TrySendError::Disconnected(job)),
        };
        drop(sender);
        match result {
            Ok(()) => {}
            Err(TrySendError::Full(job)) => job.deferred.reject(Error::new(
                Status::QueueFull,
                "bamti native invocation queue is full".to_owned(),
            )),
            Err(TrySendError::Disconnected(job)) => job.deferred.reject(Error::new(
                Status::Closing,
                "bamti native executor is closing".to_owned(),
            )),
        }
    }

    fn shutdown(&self) {
        if self.shared.closing.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(token) = self
            .shared
            .active
            .lock()
            .expect("active invocation lock poisoned")
            .as_ref()
        {
            token.cancel();
        }
        self.sender
            .lock()
            .expect("executor sender lock poisoned")
            .take();
        if let Some(worker) = self
            .worker
            .lock()
            .expect("executor worker lock poisoned")
            .take()
        {
            let _ = worker.join();
        }
    }
}

impl Drop for Executor {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn worker_loop(shared: Arc<Shared>, receiver: Receiver<Job>) {
    while let Ok(job) = receiver.recv() {
        let token = CancellationToken::new();
        {
            let mut active = shared
                .active
                .lock()
                .expect("active invocation lock poisoned");
            if shared.closing.load(Ordering::Acquire) {
                drop(job);
                continue;
            }
            *active = Some(token.clone());
        }

        let result = execute(job.request, &token);
        shared
            .active
            .lock()
            .expect("active invocation lock poisoned")
            .take();
        if shared.closing.load(Ordering::Acquire) {
            drop(job.deferred);
            continue;
        }
        match result {
            Ok(outcome) => job.deferred.resolve(Box::new(move |_| Ok(outcome))),
            Err(error) => job.deferred.reject(error),
        }
    }
}

fn execute(request: RawRequest, cancel: &CancellationToken) -> Result<RunOutcome> {
    let args = request
        .args
        .iter()
        .enumerate()
        .map(|(index, bytes)| decode_string(bytes, &format!("argument {index}")))
        .collect::<Result<Vec<_>>>()?;
    let cwd = decode_string(&request.cwd, "cwd")?;
    let mut environment = BTreeMap::new();
    for bytes in &request.env {
        let separator = bytes.iter().position(|byte| *byte == b'=').ok_or_else(|| {
            Error::new(
                Status::InvalidArg,
                "environment entry must contain '='".to_owned(),
            )
        })?;
        let key = decode_os_string(&bytes[..separator], "environment name")?;
        let value = decode_os_string(&bytes[separator + 1..], "environment value")?;
        environment.insert(key, value);
    }
    let context = ExecutionContext::new(cwd, environment)
        .map_err(|error| Error::new(Status::InvalidArg, error.to_string()))?;
    let outcome = cli_outcome_in_context_with_cancel(args, &context, cancel.clone());
    let truncation = outcome
        .truncation
        .map(|notice| -> Result<RunTruncation> {
            Ok(RunTruncation {
                elided: u32::try_from(notice.elided()).map_err(|_| {
                    Error::new(
                        Status::GenericFailure,
                        "truncation elided count exceeds the Node-API number range".to_owned(),
                    )
                })?,
                limit: u32::try_from(notice.limit()).map_err(|_| {
                    Error::new(
                        Status::GenericFailure,
                        "truncation limit exceeds the Node-API number range".to_owned(),
                    )
                })?,
            })
        })
        .transpose()?;
    Ok(RunOutcome {
        exit_code: outcome.exit_code,
        stdout: outcome.stdout.into(),
        stderr: outcome.stderr.into(),
        truncation,
    })
}

fn decode_string(bytes: &[u8], label: &str) -> Result<String> {
    let value = std::str::from_utf8(bytes)
        .map_err(|_| Error::new(Status::InvalidArg, format!("{label} must be valid UTF-8")))?;
    if value.contains('\0') {
        return Err(Error::new(
            Status::InvalidArg,
            format!("{label} must not contain NUL bytes"),
        ));
    }
    Ok(value.to_owned())
}

fn decode_os_string(bytes: &[u8], label: &str) -> Result<OsString> {
    decode_string(bytes, label).map(OsString::from)
}

fn executor(env: &Env) -> Result<Arc<Executor>> {
    if let Some(executor) = env.get_instance_data::<Arc<Executor>>()? {
        return Ok(Arc::clone(executor));
    }
    let executor = Executor::start()?;
    env.add_env_cleanup_hook(Arc::clone(&executor), |executor| executor.shutdown())?;
    env.set_instance_data(Arc::clone(&executor), (), |context| {
        context.value.shutdown();
    })?;
    Ok(executor)
}

#[napi]
pub fn run<'env>(env: &'env Env, request: RunRequest) -> Result<Object<'env>> {
    let executor = executor(env)?;
    let (deferred, promise) = env.create_deferred::<RunOutcome, OutcomeResolver>()?;
    executor.submit(Job {
        request: RawRequest {
            args: request
                .args
                .into_iter()
                .map(|value| value.to_vec())
                .collect(),
            cwd: request.cwd.to_vec(),
            env: request
                .env
                .into_iter()
                .map(|value| value.to_vec())
                .collect(),
        },
        deferred,
    });
    Ok(promise)
}

#[napi(js_name = "releaseMetadata")]
pub fn release_metadata() -> ReleaseMetadata {
    ReleaseMetadata {
        package_version: env!("BAMTI_RELEASE_PACKAGE_VERSION").to_owned(),
        source_commit: env!("BAMTI_SOURCE_COMMIT").to_owned(),
        build_set_id: env!("BAMTI_BUILD_SET_ID").to_owned(),
        release_id: env!("BAMTI_RELEASE_ID").to_owned(),
        target: env!("BAMTI_TARGET").to_owned(),
        artifact_kind: env!("BAMTI_ARTIFACT_KIND").to_owned(),
        native_abi: env!("BAMTI_NATIVE_ABI")
            .parse()
            .expect("BAMTI_NATIVE_ABI is a positive integer"),
        cli_protocol: env!("BAMTI_CLI_PROTOCOL")
            .parse()
            .expect("BAMTI_CLI_PROTOCOL is a positive integer"),
    }
}
