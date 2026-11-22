mod transport;

use fmem::{
    embeddings::{Embeddings, DIMENSIONS},
    memory::{io, shared, Binding, Store},
    Result,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

const REQUEST_BUDGET: Duration = Duration::from_millis(150);
const STARTUP_BUDGET: Duration = Duration::from_secs(1);
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_LIFETIME: Duration = Duration::from_secs(540);
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const LOOP_DELAY: Duration = Duration::from_millis(10);
const MAX_PROJECTS: usize = 16;
const BATCH_LIMIT: usize = 4;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Scope {
    cwd: PathBuf,
    binding: String,
}

impl Scope {
    fn new(cwd: &Path, binding: &Binding) -> Self {
        Self {
            cwd: cwd.to_owned(),
            binding: io::digest(&[
                &io::directory_key(cwd),
                &io::directory_key(&binding.root),
                &binding.project,
                &binding.mode,
                binding.generation.as_deref().unwrap_or_default(),
            ]),
        }
    }

    fn resolve(&self, store: &Store) -> Result<Option<Binding>> {
        let cwd = &self.cwd;
        if !cwd.is_absolute()
            || !io::equal(cwd, &io::absolute(cwd)?)
            || !io::state(&store.state, cwd, false)?
        {
            return Ok(None);
        }
        let Some(binding) = store.resolve(cwd)? else {
            return Ok(None);
        };
        let vault = binding.root.parent().ok_or("Missing parent vault")?;
        Ok(
            (!io::within(&store.state, vault) && *self == Scope::new(cwd, &binding))
                .then_some(binding),
        )
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    scope: Scope,
    query: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Answer {
    queued: bool,
    vector: Option<Vec<f32>>,
}

struct Job {
    request: Request,
    reply: Option<transport::Exchange>,
}

struct Project {
    scope: Scope,
    next_poll: Instant,
    last_seen: Instant,
    shared_next: bool,
}

pub fn try_query(store: &Store, cwd: &Path, binding: &Binding, query: &str) -> Option<Vec<f32>> {
    if query.trim().is_empty() || query.chars().count() > 500 {
        return None;
    }
    let deadline = Instant::now() + REQUEST_BUDGET;
    let request = Request {
        scope: Scope::new(cwd, binding),
        query: Some(query.into()),
    };
    let answer: Answer = transport::send(&store.state, &request, deadline).ok()?;
    answer.vector.filter(|vector| valid_vector(vector))
}

pub fn run(store: &Store, cwd: &Path, binding: &Binding) -> Result<()> {
    let scope = Scope::new(cwd, binding);
    if !Embeddings::installed(&store.state)? || scope.resolve(store)?.is_none() {
        return Ok(());
    }
    let started = Instant::now();
    let deadline = started + STARTUP_BUDGET;
    let listener = loop {
        if let Some(listener) = transport::Listener::open(&store.state)? {
            break listener;
        }
        let request = Request {
            scope: scope.clone(),
            query: None,
        };
        let attempt_deadline = deadline.min(Instant::now() + REQUEST_BUDGET);
        if transport::send::<_, Answer>(&store.state, &request, attempt_deadline)
            .is_ok_and(|answer| answer.queued)
            || Instant::now() >= deadline
        {
            return Ok(());
        }
        thread::sleep(LOOP_DELAY);
    };
    let ready = Arc::new(AtomicBool::new(false));
    let (sender, receiver) = mpsc::sync_channel(MAX_PROJECTS);
    let worker_store = Store::new(&store.state)?;
    let worker_ready = Arc::clone(&ready);
    let worker = thread::spawn(move || {
        let model = Embeddings::load_installed(&worker_store.state)?;
        worker_ready.store(true, Ordering::Release);
        work(&worker_store, model, scope, receiver, started);
        Ok(())
    });
    while !worker.is_finished() {
        match listener.accept::<Request>(REQUEST_BUDGET)? {
            Some((reply, request)) if request.query.is_none() => {
                let queued = sender
                    .try_send(Job {
                        request,
                        reply: None,
                    })
                    .is_ok();
                reply.reply(&Answer {
                    queued,
                    vector: None,
                });
            }
            Some((reply, request)) if ready.load(Ordering::Acquire) => {
                let _ = sender.try_send(Job {
                    request,
                    reply: Some(reply),
                });
            }
            Some((reply, _)) => reply.reply(&Answer::default()),
            None => thread::sleep(LOOP_DELAY),
        }
    }
    worker
        .join()
        .map_err(|_| "Semantic worker stopped unexpectedly")?
}

fn work(
    store: &Store,
    mut model: Embeddings,
    initial: Scope,
    jobs: Receiver<Job>,
    started: Instant,
) {
    let mut projects = VecDeque::new();
    register(store, &mut projects, initial);
    let hard_stop = started + MAX_LIFETIME;
    loop {
        let now = Instant::now();
        projects.retain(|project| now.duration_since(project.last_seen) < IDLE_TIMEOUT);
        if now >= hard_stop {
            return;
        }
        for _ in 0..BATCH_LIMIT {
            if let Ok(job) = jobs.try_recv() {
                handle_job(store, &mut model, &mut projects, job);
            } else {
                break;
            }
        }
        if projects.is_empty() {
            return;
        }
        if poll_next(store, &mut model, &mut projects) {
            continue;
        }
        match jobs.recv_timeout(LOOP_DELAY) {
            Ok(job) => handle_job(store, &mut model, &mut projects, job),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn register(store: &Store, projects: &mut VecDeque<Project>, scope: Scope) {
    if !matches!(scope.resolve(store), Ok(Some(_))) {
        return;
    }
    if let Some(project) = projects.iter_mut().find(|project| project.scope == scope) {
        project.last_seen = Instant::now();
        return;
    }
    projects.retain(|project| !io::equal(&project.scope.cwd, &scope.cwd));
    if projects.len() == MAX_PROJECTS {
        if let Some((index, _)) = projects
            .iter()
            .enumerate()
            .min_by_key(|(_, project)| project.last_seen)
        {
            projects.remove(index);
        }
    }
    let now = Instant::now();
    projects.push_back(Project {
        scope,
        next_poll: now,
        last_seen: now,
        shared_next: false,
    });
}

fn handle_job(store: &Store, model: &mut Embeddings, projects: &mut VecDeque<Project>, job: Job) {
    let Job { request, reply } = job;
    let Some(query) = request.query else {
        register(store, projects, request.scope);
        return;
    };
    let Some(reply) = reply else { return };
    let vector = if Instant::now() < reply.deadline
        && !query.trim().is_empty()
        && query.chars().count() <= 500
    {
        projects
            .iter_mut()
            .find(|project| project.scope == request.scope)
            .and_then(|project| {
                if !matches!(project.scope.resolve(store), Ok(Some(_)))
                    || Instant::now() >= reply.deadline
                {
                    return None;
                }
                project.last_seen = Instant::now();
                model.query(&query).ok()
            })
    } else {
        None
    };
    reply.reply(&Answer {
        queued: false,
        vector,
    });
}

fn poll_next(store: &Store, model: &mut Embeddings, projects: &mut VecDeque<Project>) -> bool {
    let now = Instant::now();
    let Some(index) = projects.iter().position(|project| project.next_poll <= now) else {
        return false;
    };
    let mut project = projects.remove(index).expect("existing pending project");
    let binding = match project.scope.resolve(store) {
        Ok(Some(binding)) => binding,
        Ok(None) => return true,
        Err(_) => {
            project.next_poll = now + POLL_INTERVAL;
            projects.push_back(project);
            return true;
        }
    };
    let selected = if project.shared_next {
        shared::for_recall(store, &binding).ok().flatten()
    } else {
        Some(binding)
    };
    let remains = selected.is_some_and(|binding| {
        store
            .update_index_batch(&binding, model, BATCH_LIMIT, || {
                matches!(project.scope.resolve(store), Ok(Some(_)))
            })
            .is_ok_and(|report| report.pending > 0)
    });
    project.shared_next = !project.shared_next;
    project.next_poll = if remains || project.shared_next {
        now
    } else {
        now + POLL_INTERVAL
    };
    projects.push_back(project);
    true
}

fn valid_vector(vector: &[f32]) -> bool {
    vector.len() == DIMENSIONS
        && vector.iter().all(|value| value.is_finite())
        && vector.iter().any(|value| *value != 0.0)
}

#[cfg(test)]
mod tests;
