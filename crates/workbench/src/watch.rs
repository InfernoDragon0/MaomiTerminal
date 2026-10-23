//! Tells the UI when the board files change, so it can reload without the user asking.
//!
//! This is a polling watcher: its own thread lists each board folder every [`Timing::poll`] and
//! compares (length, modified time) per file. Boards are a handful of small folders, so a scan is
//! cheap, and polling behaves the same on every platform without a file-notification dependency.
//!
//! A changed file is reported once it has stayed unchanged for [`Timing::debounce`], so the
//! write-temp-then-rename of a writer, or a burst of writes, gives a single event. Plans are then
//! compared by their `rev`: rewriting a plan with the same `rev` is not reported. Files that
//! exist when the watcher starts (or when a board folder first appears) are the baseline and are
//! not reported; the UI loads those itself.
//!
//! Activity log appends are reported on their own ([`BoardEvent::ActivityAppended`]), they are not
//! folded into [`BoardEvent::PlanChanged`].

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use async_channel::{Receiver, Sender};

use crate::Error;
use crate::store::{
    PROJECTS_FILE, SETTINGS_FILE, is_plan_file, projects_in, read_plan, workbench_home,
};

/// Suffix of a plan's activity log; the plan id comes before it.
const ACTIVITY_SUFFIX: &str = ".activity.jsonl";
/// How often (in polls) the project list is re-read without the projects file having changed, to
/// find a board folder that appeared later.
const REDISCOVER_EVERY: u32 = 8;

/// Something changed in the board files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoardEvent {
    /// A plan was added, or saved with a different `rev`.
    PlanChanged {
        board_dir: PathBuf,
        plan_id: String,
        path: PathBuf,
    },
    /// A plan file was deleted.
    PlanRemoved {
        board_dir: PathBuf,
        plan_id: String,
        path: PathBuf,
    },
    /// The plan's activity log was written to.
    ActivityAppended { board_dir: PathBuf, plan_id: String },
    /// `_project.d/settings.json` of a board was added, changed or deleted.
    SettingsChanged { board_dir: PathBuf },
    /// `board-projects.json` changed, or the set of board folders did. The watcher already
    /// watches the new folders; the UI should call [`crate::projects`] again.
    ProjectsChanged,
}

/// How often the watcher looks and how long a file must stay quiet before it is reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    pub poll: Duration,
    pub debounce: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            poll: Duration::from_millis(250),
            debounce: Duration::from_millis(200),
        }
    }
}

/// Watches the boards of the Workbench on a background thread of its own. Events arrive on
/// [`events`](Self::events); dropping the watcher stops and joins the thread.
pub struct BoardWatcher {
    events: Receiver<BoardEvent>,
    /// Dropping the sender wakes the thread and tells it to stop.
    stop: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl BoardWatcher {
    /// Watches the current user's Workbench. Does nothing when the home folder is unknown.
    pub fn start() -> Self {
        match workbench_home() {
            Some(home) => Self::start_with(&home, Timing::default()),
            None => {
                tracing::warn!("board watcher disabled: no home folder");
                let (_, events) = async_channel::unbounded();
                Self {
                    events,
                    stop: None,
                    thread: None,
                }
            }
        }
    }

    /// Watches the Workbench folder `home` with the default timing.
    pub fn start_in(home: &Path) -> Self {
        Self::start_with(home, Timing::default())
    }

    /// Watches the Workbench folder `home`. The first scan runs before this returns, so a change
    /// made afterwards is always reported.
    pub fn start_with(home: &Path, timing: Timing) -> Self {
        let (tx, events) = async_channel::unbounded();
        let (stop, stopped) = mpsc::channel();
        let worker = Worker::new(home.to_owned());
        let thread = thread::Builder::new()
            .name("maomi-board-watch".into())
            .spawn(move || worker.run(timing, &stopped, &tx))
            .map_err(|err| tracing::warn!("cannot start the board watcher: {err}"))
            .ok();
        Self {
            events,
            stop: Some(stop),
            thread,
        }
    }

    /// The event channel. Clone it to read from another task; the watcher keeps running while
    /// the `BoardWatcher` is alive.
    pub fn events(&self) -> &Receiver<BoardEvent> {
        &self.events
    }
}

impl Drop for BoardWatcher {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            tracing::warn!("the board watcher thread panicked");
        }
    }
}

/// Modified time and length of a file, enough to notice that it was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    len: u64,
    modified: Option<SystemTime>,
}

impl Stamp {
    fn of(meta: &fs::Metadata) -> Self {
        Self {
            len: meta.len(),
            modified: meta.modified().ok(),
        }
    }

    fn of_path(path: &Path) -> Option<Self> {
        fs::metadata(path)
            .ok()
            .filter(fs::Metadata::is_file)
            .map(|meta| Self::of(&meta))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Plan,
    Activity,
    Settings,
}

/// What was last reported (or seen at the start) for a file.
struct Known {
    stamp: Stamp,
    /// Plans only; `None` when the file could not be parsed.
    rev: Option<i64>,
}

/// The files of one board folder and what has been reported about them.
struct BoardState {
    dir: PathBuf,
    /// Result of the latest scan.
    files: HashMap<PathBuf, (Kind, Stamp)>,
    known: HashMap<PathBuf, Known>,
    /// Changed files waiting for their quiet period, with when they last changed.
    pending: HashMap<PathBuf, (Kind, Instant)>,
    /// A listing error was logged and has not been followed by a good scan yet.
    failing: bool,
}

impl BoardState {
    /// Scans `dir` and treats everything in it as already reported.
    fn new(dir: PathBuf) -> Self {
        let mut state = Self {
            dir,
            files: HashMap::new(),
            known: HashMap::new(),
            pending: HashMap::new(),
            failing: false,
        };
        if let Some(files) = state.scan() {
            for (path, &(kind, stamp)) in &files {
                let rev = (kind == Kind::Plan)
                    .then(|| read_plan(path).ok().map(|plan| plan.rev))
                    .flatten();
                state.known.insert(path.clone(), Known { stamp, rev });
            }
            state.files = files;
        }
        state
    }

    /// The relevant files in the board folder. `None` when the folder cannot be listed for a
    /// reason other than not existing, so a hiccup does not look like every plan being deleted.
    fn scan(&mut self) -> Option<HashMap<PathBuf, (Kind, Stamp)>> {
        let mut found = HashMap::new();
        match fs::read_dir(&self.dir) {
            Ok(dir) => {
                for entry in dir.flatten() {
                    let name = entry.file_name();
                    let Some(kind) = file_kind(&name.to_string_lossy()) else {
                        continue;
                    };
                    // Folders such as `<planId>.d` are skipped here.
                    if let Ok(meta) = entry.metadata()
                        && meta.is_file()
                    {
                        found.insert(entry.path(), (kind, Stamp::of(&meta)));
                    }
                }
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => {
                if !self.failing {
                    tracing::warn!("cannot list board {}: {err}", self.dir.display());
                    self.failing = true;
                }
                return None;
            }
        }
        self.failing = false;
        let settings = self.dir.join(SETTINGS_FILE.iter().collect::<PathBuf>());
        if let Some(stamp) = Stamp::of_path(&settings) {
            found.insert(settings, (Kind::Settings, stamp));
        }
        Some(found)
    }

    fn tick(&mut self, now: Instant, debounce: Duration, out: &mut Vec<BoardEvent>) {
        if let Some(current) = self.scan() {
            for (path, &(kind, stamp)) in &current {
                if self.files.get(path).map(|(_, s)| s) != Some(&stamp) {
                    self.pending.insert(path.clone(), (kind, now));
                }
            }
            for (path, &(kind, _)) in &self.files {
                if !current.contains_key(path) {
                    self.pending.insert(path.clone(), (kind, now));
                }
            }
            self.files = current;
        }

        let mut ready: Vec<PathBuf> = self
            .pending
            .iter()
            .filter(|(_, (_, changed))| now.duration_since(*changed) >= debounce)
            .map(|(path, _)| path.clone())
            .collect();
        ready.sort();
        for path in ready {
            if let Some((kind, _)) = self.pending.remove(&path) {
                self.resolve(&path, kind, out);
            }
        }
    }

    /// Decides whether a file that stayed quiet needs an event.
    fn resolve(&mut self, path: &Path, kind: Kind, out: &mut Vec<BoardEvent>) {
        let board_dir = self.dir.clone();
        let stamp = self.files.get(path).map(|&(_, stamp)| stamp);
        match kind {
            Kind::Plan => {
                let plan_id = id_of(path, ".json");
                // Parsed again here, not at scan time: most changes are the same `rev`.
                let state = stamp.and_then(|stamp| match read_plan(path) {
                    Ok(plan) => Some((stamp, Some(plan.rev))),
                    // Deleted since the scan; the next scan reports it.
                    Err(Error::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
                        None
                    }
                    Err(err) => {
                        tracing::warn!("cannot parse plan: {err}");
                        Some((stamp, None))
                    }
                });
                match state {
                    None => {
                        if self.known.remove(path).is_some() {
                            out.push(BoardEvent::PlanRemoved {
                                board_dir,
                                plan_id,
                                path: path.to_owned(),
                            });
                        }
                    }
                    Some((stamp, rev)) => {
                        let changed = self
                            .known
                            .get(path)
                            .is_none_or(|k| k.rev != rev || (rev.is_none() && k.stamp != stamp));
                        self.known.insert(path.to_owned(), Known { stamp, rev });
                        if changed {
                            out.push(BoardEvent::PlanChanged {
                                board_dir,
                                plan_id,
                                path: path.to_owned(),
                            });
                        }
                    }
                }
            }
            Kind::Activity => match stamp {
                Some(stamp) => {
                    let old = self
                        .known
                        .insert(path.to_owned(), Known { stamp, rev: None });
                    if old.is_none_or(|k| k.stamp != stamp) {
                        out.push(BoardEvent::ActivityAppended {
                            board_dir,
                            plan_id: id_of(path, ACTIVITY_SUFFIX),
                        });
                    }
                }
                None => {
                    self.known.remove(path);
                }
            },
            Kind::Settings => {
                let changed = match stamp {
                    Some(stamp) => {
                        let old = self
                            .known
                            .insert(path.to_owned(), Known { stamp, rev: None });
                        old.is_none_or(|k| k.stamp != stamp)
                    }
                    None => self.known.remove(path).is_some(),
                };
                if changed {
                    out.push(BoardEvent::SettingsChanged { board_dir });
                }
            }
        }
    }
}

/// Plan file, activity log or nothing: locks, temp files and folders never count.
fn file_kind(name: &str) -> Option<Kind> {
    if name.contains(".tmp") || name.ends_with(".lock") {
        return None;
    }
    if is_plan_file(name) {
        return Some(Kind::Plan);
    }
    let id = name.strip_suffix(ACTIVITY_SUFFIX)?;
    (!id.is_empty() && !id.starts_with('_')).then_some(Kind::Activity)
}

/// The plan id in a file name: everything before `suffix`.
fn id_of(path: &Path, suffix: &str) -> String {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    name.strip_suffix(suffix).unwrap_or(&name).to_owned()
}

/// The state the watcher thread owns.
struct Worker {
    home: PathBuf,
    boards: BTreeMap<PathBuf, BoardState>,
    projects_stamp: Option<Stamp>,
    /// When `board-projects.json` last changed, until it has been quiet long enough.
    projects_pending: Option<Instant>,
    polls: u32,
}

impl Worker {
    fn new(home: PathBuf) -> Self {
        let mut worker = Self {
            projects_stamp: Stamp::of_path(&home.join(PROJECTS_FILE)),
            home,
            boards: BTreeMap::new(),
            projects_pending: None,
            polls: 0,
        };
        worker.rediscover();
        worker
    }

    fn run(mut self, timing: Timing, stopped: &mpsc::Receiver<()>, tx: &Sender<BoardEvent>) {
        let mut out = Vec::new();
        // Nothing is ever sent on `stopped`; the sender being dropped ends the wait early.
        while let Err(RecvTimeoutError::Timeout) = stopped.recv_timeout(timing.poll) {
            self.tick(Instant::now(), timing.debounce, &mut out);
            for event in out.drain(..) {
                if tx.send_blocking(event).is_err() {
                    return;
                }
            }
        }
    }

    fn tick(&mut self, now: Instant, debounce: Duration, out: &mut Vec<BoardEvent>) {
        self.polls = self.polls.wrapping_add(1);

        let stamp = Stamp::of_path(&self.home.join(PROJECTS_FILE));
        if stamp != self.projects_stamp {
            self.projects_stamp = stamp;
            self.projects_pending = Some(now);
        }
        let settled = self
            .projects_pending
            .is_some_and(|changed| now.duration_since(changed) >= debounce);
        if settled {
            self.projects_pending = None;
            self.rediscover();
            out.push(BoardEvent::ProjectsChanged);
        } else if self.projects_pending.is_none()
            && self.polls.is_multiple_of(REDISCOVER_EVERY)
            && self.rediscover()
        {
            // A board folder can appear without the projects file changing.
            out.push(BoardEvent::ProjectsChanged);
        }

        for board in self.boards.values_mut() {
            board.tick(now, debounce, out);
        }
    }

    /// Re-reads the project list and watches exactly those board folders. Returns whether the set
    /// of folders changed. New folders start silently (their plans are the baseline).
    fn rediscover(&mut self) -> bool {
        let dirs: HashSet<PathBuf> = projects_in(&self.home)
            .into_iter()
            .map(|project| project.board_dir)
            .collect();
        let before = self.boards.len();
        self.boards.retain(|dir, _| dirs.contains(dir));
        let mut changed = self.boards.len() != before;
        for dir in dirs {
            if !self.boards.contains_key(&dir) {
                self.boards.insert(dir.clone(), BoardState::new(dir));
                changed = true;
            }
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway Workbench folder under the system temp dir, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "maomi-workbench-watch-{}-{name}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        /// The Workbench folder (`home`), holding one local board, `proj`.
        fn home(&self) -> PathBuf {
            self.0.join("home")
        }

        fn board(&self) -> PathBuf {
            self.home().join("boards").join("proj")
        }

        fn write(&self, rel: &str, text: &str) -> PathBuf {
            let path = self.0.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, text).unwrap();
            path
        }

        fn write_board(&self, rel: &str, text: &str) -> PathBuf {
            self.write(&format!("home/boards/proj/{rel}"), text)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn plan_json(id: &str, rev: i64) -> String {
        format!(r#"{{"id":"{id}","title":"T","rev":{rev}}}"#)
    }

    /// Everything that arrives within `window`.
    fn collect(watcher: &BoardWatcher, window: Duration) -> Vec<BoardEvent> {
        let end = Instant::now() + window;
        let mut got = Vec::new();
        while Instant::now() < end {
            while let Ok(event) = watcher.events().try_recv() {
                got.push(event);
            }
            thread::sleep(Duration::from_millis(20));
        }
        got
    }

    /// The next event, waiting up to 3 s.
    fn next(watcher: &BoardWatcher) -> Option<BoardEvent> {
        let end = Instant::now() + Duration::from_secs(3);
        while Instant::now() < end {
            if let Ok(event) = watcher.events().try_recv() {
                return Some(event);
            }
            thread::sleep(Duration::from_millis(20));
        }
        None
    }

    fn quiet(watcher: &BoardWatcher) {
        let got = collect(watcher, Duration::from_millis(600));
        assert!(got.is_empty(), "unexpected events: {got:?}");
    }

    fn changed(s: &Scratch, id: &str) -> BoardEvent {
        BoardEvent::PlanChanged {
            board_dir: s.board(),
            plan_id: id.to_owned(),
            path: s.board().join(format!("{id}.json")),
        }
    }

    #[test]
    fn reports_plan_changes_by_rev() {
        let s = Scratch::new("plans");
        s.write_board("p-old.json", &plan_json("p-old", 1));
        let w = BoardWatcher::start_in(&s.home());
        // Plans that exist at the start are the baseline.
        quiet(&w);

        s.write_board("p-a.json", &plan_json("p-a", 1));
        assert_eq!(next(&w), Some(changed(&s, "p-a")));
        quiet(&w);

        // Same rev, different bytes: not a change.
        s.write_board("p-a.json", &format!("{}\n\n", plan_json("p-a", 1)));
        quiet(&w);

        // The writers' way: temp file, then rename over the plan.
        let tmp = s.write_board("p-a.json.4242.abcd.tmp", &plan_json("p-a", 2));
        fs::rename(&tmp, s.board().join("p-a.json")).unwrap();
        assert_eq!(next(&w), Some(changed(&s, "p-a")));
        quiet(&w);

        // A burst of writes comes out as one event.
        for rev in 3..8 {
            s.write_board("p-a.json", &plan_json("p-a", rev));
            thread::sleep(Duration::from_millis(30));
        }
        let got = collect(&w, Duration::from_millis(900));
        assert_eq!(got, [changed(&s, "p-a")]);

        fs::remove_file(s.board().join("p-a.json")).unwrap();
        assert_eq!(
            next(&w),
            Some(BoardEvent::PlanRemoved {
                board_dir: s.board(),
                plan_id: "p-a".to_owned(),
                path: s.board().join("p-a.json"),
            })
        );
        quiet(&w);
    }

    #[test]
    fn ignores_locks_temp_files_and_plan_folders() {
        let s = Scratch::new("noise");
        s.write_board("p-a.json", &plan_json("p-a", 1));
        let w = BoardWatcher::start_in(&s.home());

        s.write_board("p-a.json.lock", "");
        s.write_board("p-b.json.4242.abcd.tmp", &plan_json("p-b", 1));
        s.write_board("p-a.activity.jsonl.4242.abcd.tmp", "{}\n");
        s.write_board("p-a.d/notes.json", "{}");
        s.write_board("p-a.d/p-c.json", &plan_json("p-c", 1));
        s.write_board("_project.d/other.json", "{}");
        quiet(&w);
    }

    #[test]
    fn reports_activity_and_settings() {
        let s = Scratch::new("extras");
        s.write_board("p-a.json", &plan_json("p-a", 1));
        s.write_board("p-a.activity.jsonl", "{\"at\":1,\"op\":\"x\"}\n");
        s.write_board("_project.d/settings.json", "{}");
        let w = BoardWatcher::start_in(&s.home());
        quiet(&w);

        s.write_board(
            "p-a.activity.jsonl",
            "{\"at\":1,\"op\":\"x\"}\n{\"at\":2,\"op\":\"y\"}\n",
        );
        assert_eq!(
            next(&w),
            Some(BoardEvent::ActivityAppended {
                board_dir: s.board(),
                plan_id: "p-a".to_owned(),
            })
        );

        s.write_board("_project.d/settings.json", r#"{"cardTypes":[]}"#);
        let settings = BoardEvent::SettingsChanged {
            board_dir: s.board(),
        };
        assert_eq!(next(&w), Some(settings.clone()));

        fs::remove_file(s.board().join("_project.d/settings.json")).unwrap();
        assert_eq!(next(&w), Some(settings));
        quiet(&w);
    }

    #[test]
    fn follows_the_project_list() {
        let s = Scratch::new("projects");
        let w = BoardWatcher::start_in(&s.home());
        quiet(&w);

        // A repo folder with a board of its own, added to board-projects.json.
        let repo = s.write("repo/.maomi/board/p-r.json", &plan_json("p-r", 1));
        let repo = repo.ancestors().nth(3).unwrap().to_owned();
        let listed = serde_json::json!({ "projects": [{ "key": "r", "path": repo }] });
        s.write("home/board-projects.json", &listed.to_string());
        assert_eq!(next(&w), Some(BoardEvent::ProjectsChanged));
        assert!(
            projects_in(&s.home())
                .iter()
                .any(|p| p.board_dir == repo.join(".maomi").join("board"))
        );
        quiet(&w);

        // The new board is watched.
        let board = repo.join(".maomi").join("board");
        fs::write(board.join("p-n.json"), plan_json("p-n", 1)).unwrap();
        assert_eq!(
            next(&w),
            Some(BoardEvent::PlanChanged {
                path: board.join("p-n.json"),
                board_dir: board,
                plan_id: "p-n".to_owned(),
            })
        );
    }

    #[test]
    fn dropping_the_watcher_stops_its_thread() {
        let s = Scratch::new("drop");
        let w = BoardWatcher::start_with(
            &s.home(),
            Timing {
                poll: Duration::from_secs(30),
                debounce: Duration::from_millis(200),
            },
        );
        let started = Instant::now();
        drop(w);
        // The thread is mid-wait on a 30 s poll; it must wake up and exit at once.
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
