use crate::protocol::{ChangeEvent, ChangeKind};
use notify::{
    Config as NotifyConfig, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
};
use rand_core::{OsRng, RngCore};
use std::collections::{HashMap, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

const MAX_EVENTS: usize = 50_000;
const MAX_EVENTS_PER_REPLY: usize = 4_096;
const ECHO_WINDOW: Duration = Duration::from_secs(10);
const MAX_WATCHED_DIRS: usize = 100_000;

pub struct WaitResult {
    pub epoch: u64,
    pub cursor: u64,
    pub events: Vec<ChangeEvent>,
    pub reset: bool,
    pub live: bool,
}

struct Recorded {
    seq: u64,
    origin: u64,
    event: ChangeEvent,
}

type Signature = Option<(u64, Option<SystemTime>, bool)>;

struct FeedState {
    next_seq: u64,
    events: VecDeque<Recorded>,
    recent: HashMap<String, (Instant, Signature)>,
    busy: HashMap<String, usize>,
}

pub struct RootFeed {
    root: PathBuf,
    state: Mutex<FeedState>,
    changed: Condvar,
    live: AtomicBool,
    watching: AtomicBool,
    watcher: Mutex<Option<RecommendedWatcher>>,
    watched_dirs: Mutex<usize>,
    ignore: Mutex<Vec<String>>,
}

pub fn epoch() -> u64 {
    static EPOCH: OnceLock<u64> = OnceLock::new();
    *EPOCH.get_or_init(|| OsRng.next_u64() | 1)
}

pub fn feed(root: &Path) -> Arc<RootFeed> {
    static FEEDS: OnceLock<Mutex<HashMap<PathBuf, Arc<RootFeed>>>> = OnceLock::new();
    let feeds = FEEDS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut feeds = feeds.lock().unwrap_or_else(|error| error.into_inner());
    feeds
        .entry(root.to_path_buf())
        .or_insert_with(|| {
            Arc::new(RootFeed {
                root: root.to_path_buf(),
                state: Mutex::new(FeedState {
                    next_seq: 1,
                    events: VecDeque::new(),
                    recent: HashMap::new(),
                    busy: HashMap::new(),
                }),
                changed: Condvar::new(),
                live: AtomicBool::new(false),
                watching: AtomicBool::new(false),
                watcher: Mutex::new(None),
                watched_dirs: Mutex::new(0),
                ignore: Mutex::new(Vec::new()),
            })
        })
        .clone()
}

pub fn record(root: &Path, origin: u64, path: &str, kind: ChangeKind) {
    feed(root).push(origin, path, kind, true);
}

pub struct WriteGuard {
    feed: Arc<RootFeed>,
    path: String,
}

impl Drop for WriteGuard {
    fn drop(&mut self) {
        let mut state = self
            .feed
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(count) = state.busy.get_mut(&self.path) {
            *count -= 1;
            if *count == 0 {
                state.busy.remove(&self.path);
            }
        }
    }
}

pub fn begin_write(root: &Path, path: &str) -> WriteGuard {
    let feed = feed(root);
    *feed
        .state
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .busy
        .entry(path.to_string())
        .or_insert(0) += 1;
    WriteGuard {
        feed,
        path: path.to_string(),
    }
}

fn signature(path: &Path) -> Signature {
    std::fs::symlink_metadata(path)
        .ok()
        .map(|meta| (meta.len(), meta.modified().ok(), meta.is_dir()))
}

impl RootFeed {
    fn push(&self, origin: u64, path: &str, kind: ChangeKind, from_mobfs: bool) {
        if !from_mobfs
            && self
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .busy
                .contains_key(path)
        {
            return;
        }
        let current = signature(&self.root.join(path));
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if !from_mobfs && state.busy.contains_key(path) {
            return;
        }
        if from_mobfs {
            state
                .recent
                .insert(path.to_string(), (Instant::now(), current));
            if state.recent.len() > 16_384 {
                state
                    .recent
                    .retain(|_, (seen, _)| seen.elapsed() < ECHO_WINDOW);
            }
        } else if state
            .recent
            .get(path)
            .is_some_and(|(seen, known)| seen.elapsed() < ECHO_WINDOW && *known == current)
        {
            return;
        }
        let seq = state.next_seq;
        state.next_seq += 1;
        state.events.push_back(Recorded {
            seq,
            origin,
            event: ChangeEvent {
                path: path.to_string(),
                kind,
            },
        });
        while state.events.len() > MAX_EVENTS {
            state.events.pop_front();
        }
        self.changed.notify_all();
    }

    pub fn wait(
        self: &Arc<Self>,
        client: u64,
        client_epoch: u64,
        since: Option<u64>,
        ignore: &[String],
        timeout: Duration,
    ) -> WaitResult {
        self.ensure_watching(ignore);
        let epoch = epoch();
        let deadline = Instant::now() + timeout;
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let Some(since) = since else {
            return WaitResult {
                epoch,
                cursor: state.next_seq - 1,
                events: Vec::new(),
                reset: false,
                live: self.live.load(Ordering::SeqCst),
            };
        };
        let oldest = state
            .events
            .front()
            .map(|event| event.seq)
            .unwrap_or(state.next_seq);
        if client_epoch != epoch || since >= state.next_seq || since.saturating_add(1) < oldest {
            return WaitResult {
                epoch,
                cursor: state.next_seq - 1,
                events: Vec::new(),
                reset: true,
                live: self.live.load(Ordering::SeqCst),
            };
        }
        loop {
            let mut events = Vec::new();
            let mut cursor = state.next_seq - 1;
            let oldest = state
                .events
                .front()
                .map_or(state.next_seq, |event| event.seq);
            let skip = usize::try_from((since + 1).saturating_sub(oldest)).unwrap_or(usize::MAX);
            for recorded in state.events.iter().skip(skip) {
                if events.len() >= MAX_EVENTS_PER_REPLY {
                    cursor = recorded.seq - 1;
                    break;
                }
                if client != 0 && recorded.origin == client {
                    continue;
                }
                events.push(recorded.event.clone());
            }
            let now = Instant::now();
            if !events.is_empty() || now >= deadline {
                return WaitResult {
                    epoch,
                    cursor,
                    events,
                    reset: false,
                    live: self.live.load(Ordering::SeqCst),
                };
            }
            state = self
                .changed
                .wait_timeout(state, deadline - now)
                .map(|(guard, _)| guard)
                .unwrap_or_else(|error| error.into_inner().0);
        }
    }

    fn ensure_watching(self: &Arc<Self>, ignore: &[String]) {
        if self.watching.swap(true, Ordering::SeqCst) {
            return;
        }
        *self
            .ignore
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = ignore.to_vec();
        let feed = self.clone();
        thread::spawn(move || {
            if let Err(error) = feed.start_watcher() {
                crate::ui::warn(format!(
                    "change watcher unavailable for {}: {error}",
                    feed.root.display()
                ));
                feed.live.store(false, Ordering::SeqCst);
            }
        });
    }

    fn start_watcher(self: &Arc<Self>) -> notify::Result<()> {
        let (tx, rx) = mpsc::channel::<notify::Result<Event>>();
        let watcher = RecommendedWatcher::new(tx, NotifyConfig::default())?;
        *self
            .watcher
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(watcher);
        let complete = if cfg!(target_os = "linux") {
            self.watch_tree(&self.root.clone())
        } else {
            self.with_watcher(|watcher| watcher.watch(&self.root, RecursiveMode::Recursive))
                .is_ok()
        };
        self.live.store(complete, Ordering::SeqCst);
        let feed = self.clone();
        thread::spawn(move || {
            for event in rx {
                match event {
                    Ok(event) => feed.handle_fs_event(event),
                    Err(error) => {
                        crate::ui::warn(format!("change watcher error: {error}"));
                    }
                }
            }
            feed.live.store(false, Ordering::SeqCst);
        });
        Ok(())
    }

    fn with_watcher<T>(
        &self,
        action: impl FnOnce(&mut RecommendedWatcher) -> notify::Result<T>,
    ) -> notify::Result<T> {
        let mut guard = self
            .watcher
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        match guard.as_mut() {
            Some(watcher) => action(watcher),
            None => Err(notify::Error::generic("watcher stopped")),
        }
    }

    fn watch_tree(&self, dir: &Path) -> bool {
        let ignore = self
            .ignore
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let walker = walkdir::WalkDir::new(dir)
            .into_iter()
            .filter_entry(|entry| {
                entry.path() == dir
                    || entry
                        .file_name()
                        .to_str()
                        .is_some_and(|name| !crate::local::should_ignore_part(name, &ignore))
            });
        let mut complete = true;
        for entry in walker.flatten() {
            if !entry.file_type().is_dir() {
                continue;
            }
            let mut watched = self
                .watched_dirs
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if *watched >= MAX_WATCHED_DIRS {
                return false;
            }
            if self
                .with_watcher(|watcher| watcher.watch(entry.path(), RecursiveMode::NonRecursive))
                .is_ok()
            {
                *watched += 1;
            } else {
                complete = false;
            }
        }
        complete
    }

    fn handle_fs_event(&self, event: Event) {
        if matches!(event.kind, EventKind::Access(_)) {
            return;
        }
        let ignore = self
            .ignore
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        for path in &event.paths {
            let Some(rel) = relative(&self.root, path) else {
                continue;
            };
            if rel.is_empty()
                || rel
                    .split('/')
                    .any(|part| crate::local::should_ignore_part(part, &ignore))
                || is_daemon_temp(&rel)
            {
                continue;
            }
            if cfg!(target_os = "linux")
                && matches!(
                    event.kind,
                    EventKind::Create(_) | EventKind::Modify(notify::event::ModifyKind::Name(_))
                )
                && path.is_dir()
                && !self.watch_tree(path)
            {
                self.live.store(false, Ordering::SeqCst);
            }
            self.push(0, &rel, ChangeKind::Replaced, false);
        }
    }
}

fn is_daemon_temp(rel: &str) -> bool {
    rel.rsplit('/')
        .next()
        .is_some_and(|name| name.contains(".mobfs-upload-") || name.contains(".mobfs-tmp-"))
}

fn relative(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let mut parts = Vec::new();
    for component in rel.components() {
        match component {
            Component::Normal(value) => parts.push(value.to_str()?.to_string()),
            _ => return None,
        }
    }
    Some(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feed_filters_own_events_and_detects_resets() {
        let root = std::env::temp_dir().join(format!("mobfs-feed-test-{}", OsRng.next_u64()));
        let feed = feed(&root);
        feed.watching.store(true, Ordering::SeqCst);
        let start = feed.wait(1, epoch(), None, &[], Duration::ZERO).cursor;
        record(&root, 7, "a.mov", ChangeKind::Write { offset: 0, len: 10 });
        record(&root, 7, "a.mov", ChangeKind::Write { offset: 10, len: 5 });
        record(&root, 9, "b.txt", ChangeKind::Replaced);
        let seen = feed.wait(9, epoch(), Some(start), &[], Duration::from_millis(10));
        assert_eq!(seen.events.len(), 2);
        assert_eq!(seen.events[0].path, "a.mov");
        assert_eq!(
            seen.events[1].kind,
            ChangeKind::Write { offset: 10, len: 5 }
        );
        let later = feed.wait(
            9,
            epoch(),
            Some(seen.cursor),
            &[],
            Duration::from_millis(10),
        );
        assert!(later.events.is_empty());
        record(&root, 7, "a.mov", ChangeKind::Write { offset: 15, len: 5 });
        let appended = feed.wait(
            9,
            epoch(),
            Some(seen.cursor),
            &[],
            Duration::from_millis(10),
        );
        assert_eq!(appended.events.len(), 1);
        let reset = feed.wait(9, epoch() ^ 2, Some(start), &[], Duration::ZERO);
        assert!(reset.reset);
    }
}
