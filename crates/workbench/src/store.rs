//! Finds board folders and reads the files in them. Nothing here writes.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::Error;
use crate::plan::{Activity, Plan, PlanStatus, ProjectSettings};

pub(crate) const PROJECTS_FILE: &str = "board-projects.json";
/// Boards of projects without a folder (and of projects that keep their board outside the repo).
const LOCAL_BOARDS_DIR: &str = "boards";
/// Board folder inside a project folder.
const REPO_BOARD_DIR: &[&str] = &[".maomi", "board"];
pub(crate) const SETTINGS_FILE: &[&str] = &["_project.d", "settings.json"];

/// A project the Workbench knows, with the folder holding its plans.
#[derive(Debug, Clone, PartialEq)]
pub struct Project {
    pub key: String,
    /// Project folder; `None` for a project without one.
    pub path: Option<PathBuf>,
    pub board_dir: PathBuf,
    /// Folder name, or the key when there is no folder.
    pub name: String,
}

/// One plan in a board folder, without its cards.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanSummary {
    pub id: String,
    pub title: String,
    pub status: PlanStatus,
    /// 0.0 to 1.0.
    pub progress: f64,
    pub card_count: usize,
    pub rev: i64,
    /// Epoch milliseconds.
    pub updated_at: i64,
    /// The plan's JSON file, for [`read_plan`].
    pub path: PathBuf,
}

/// The Workbench's own folder, `~/.maomi-workbench`. `None` when the home folder is unknown.
pub fn workbench_home() -> Option<PathBuf> {
    let home = std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .or_else(|| etcetera::home_dir().ok())?;
    Some(home.join(".maomi-workbench"))
}

/// Projects of the current user's Workbench.
pub fn projects() -> Vec<Project> {
    workbench_home().map_or_else(Vec::new, |home| projects_in(&home))
}

/// Projects of the Workbench folder `home`: the ones in `board-projects.json` whose folder still
/// exists, plus folderless ones that only exist as a subfolder of `boards/`.
pub fn projects_in(home: &Path) -> Vec<Project> {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Entry {
        key: String,
        path: Option<String>,
    }

    let local = home.join(LOCAL_BOARDS_DIR);
    let mut out: Vec<Project> = Vec::new();

    let listed = read_json::<Value>(&home.join(PROJECTS_FILE)).unwrap_or_default();
    let entries = listed
        .get("projects")
        .and_then(Value::as_array)
        .into_iter()
        .flatten();
    for entry in entries {
        // One malformed entry must not hide the others.
        let Ok(entry) = serde_json::from_value::<Entry>(entry.clone()) else {
            tracing::warn!("skipping a malformed entry in {PROJECTS_FILE}");
            continue;
        };
        if entry.key.is_empty() || out.iter().any(|p| p.key == entry.key) {
            continue;
        }
        let folder = entry
            .path
            .as_deref()
            .filter(|p| !p.is_empty())
            .map(PathBuf::from);
        let project = match folder {
            Some(path) if !path.is_dir() => continue,
            Some(path) => {
                let in_repo: PathBuf = path.join(REPO_BOARD_DIR.iter().collect::<PathBuf>());
                let elsewhere = local.join(&entry.key);
                // A project can keep its board in the Workbench folder instead of the repo.
                let board_dir = if !in_repo.is_dir() && elsewhere.is_dir() {
                    elsewhere
                } else {
                    in_repo
                };
                let name = path
                    .file_name()
                    .map_or_else(|| entry.key.clone(), |n| n.to_string_lossy().into_owned());
                Project {
                    key: entry.key,
                    path: Some(path),
                    board_dir,
                    name,
                }
            }
            None => Project {
                board_dir: local.join(&entry.key),
                name: entry.key.clone(),
                key: entry.key,
                path: None,
            },
        };
        out.push(project);
    }

    if let Ok(dirs) = fs::read_dir(&local) {
        let mut extra: Vec<Project> = dirs
            .flatten()
            .filter(|d| d.path().is_dir())
            .filter_map(|d| {
                let key = d.file_name().to_string_lossy().into_owned();
                (!out.iter().any(|p| p.key == key)).then(|| Project {
                    name: key.clone(),
                    key,
                    path: None,
                    board_dir: d.path(),
                })
            })
            .collect();
        extra.sort_by(|a, b| a.key.cmp(&b.key));
        out.extend(extra);
    }
    out
}

/// Plans in a board folder, most recently updated first. A file that fails to read or parse is
/// logged and skipped so one bad plan does not hide the rest.
pub fn list_plans(board_dir: &Path) -> Vec<PlanSummary> {
    read_plans(board_dir)
        .into_iter()
        .map(|(path, plan)| PlanSummary {
            progress: plan.progress(),
            card_count: plan.cards.len(),
            id: plan.id,
            title: plan.title,
            status: plan.status,
            rev: plan.rev,
            updated_at: plan.updated_at,
            path,
        })
        .collect()
}

/// Every plan in a board folder with its file path, most recently updated first. Same skipping
/// rules as [`list_plans`]; for callers that need the cards too.
pub fn read_plans(board_dir: &Path) -> Vec<(PathBuf, Plan)> {
    let Ok(dir) = fs::read_dir(board_dir) else {
        return Vec::new();
    };
    let mut plans: Vec<(PathBuf, Plan)> = dir
        .flatten()
        .filter(|entry| is_plan_file(&entry.file_name().to_string_lossy()))
        .filter_map(|entry| match read_plan(&entry.path()) {
            Ok(plan) => Some((entry.path(), plan)),
            Err(err) => {
                tracing::warn!("skipping plan: {err}");
                None
            }
        })
        .collect();
    plans.sort_by(|(_, a), (_, b)| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    plans
}

/// Reads one plan file.
pub fn read_plan(path: &Path) -> Result<Plan, Error> {
    let mut plan: Plan = read_json(path)?;
    if plan.id.is_empty() {
        plan.id = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
    }
    Ok(plan)
}

/// The project's settings, or the defaults when the file is missing or unreadable.
pub fn read_settings(board_dir: &Path) -> ProjectSettings {
    let path = board_dir.join(SETTINGS_FILE.iter().collect::<PathBuf>());
    match read_json(&path) {
        Ok(settings) => settings,
        Err(Error::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
            ProjectSettings::default()
        }
        Err(err) => {
            tracing::warn!("using default project settings: {err}");
            ProjectSettings::default()
        }
    }
}

/// The activity log that belongs to a plan file (`<planId>.activity.jsonl` next to it).
pub fn activity_path(plan_path: &Path) -> PathBuf {
    let stem = plan_path.file_stem().unwrap_or_default().to_string_lossy();
    plan_path.with_file_name(format!("{stem}.activity.jsonl"))
}

/// The last `limit` entries of an activity log, oldest first. Lines that do not parse are
/// skipped (a line may be cut off while the log is being written); a missing log is empty.
pub fn read_activity(path: &Path, limit: usize) -> Vec<Activity> {
    let Ok(bytes) = fs::read(path) else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&bytes);
    let mut entries: Vec<Activity> = text
        .lines()
        .rev()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .take(limit)
        .collect();
    entries.reverse();
    entries
}

/// `<planId>.json`, not an activity log, lock or half-written temp file (those are
/// `<file>.<pid>.<token>.tmp` before the rename).
pub(crate) fn is_plan_file(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".json") else {
        return false;
    };
    !stem.is_empty() && !stem.contains('.') && !stem.starts_with('_') && !name.contains(".tmp")
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T, Error> {
    let bytes = fs::read(path).map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })?;
    // Files written by some editors start with a byte order mark.
    let json = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
    serde_json::from_slice(json).map_err(|source| Error::Json {
        path: path.to_owned(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAN: &str = include_str!("../tests/fixtures/plan.json");
    const SETTINGS: &str = include_str!("../tests/fixtures/settings.json");
    const ACTIVITY: &str = include_str!("../tests/fixtures/plan.activity.jsonl");

    /// A throwaway folder under the system temp dir, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "maomi-workbench-test-{}-{name}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn write(&self, rel: &str, text: &str) -> PathBuf {
            let path = self.0.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, text).unwrap();
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn lists_plans_and_skips_noise() {
        let s = Scratch::new("list");
        s.write("p-aaa.json", &PLAN.replace("p-mvg50cth6ccf", "p-aaa"));
        s.write(
            "p-bbb.json",
            r#"{"id":"p-bbb","title":"Newer","updatedAt":1792999999999,"rev":2}"#,
        );
        s.write("p-bad.json", "{ not json");
        s.write("p-aaa.activity.jsonl", ACTIVITY);
        s.write("p-aaa.json.4242.abcd.tmp", "{}");
        s.write("p-aaa.json.lock", "");
        s.write("_project.d/settings.json", SETTINGS);
        s.write("p-aaa.d/notes.json", "{}");

        let plans = list_plans(&s.0);
        let ids: Vec<_> = plans.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["p-bbb", "p-aaa"]);
        assert_eq!(plans[0].rev, 2);
        assert_eq!(plans[1].card_count, 3);
        assert!((plans[1].progress - 0.26).abs() < 1e-9);
        assert!(list_plans(&s.0.join("missing")).is_empty());
    }

    #[test]
    fn reads_a_plan_with_bom_and_fills_the_id() {
        let s = Scratch::new("bom");
        let path = s.write("p-xyz.json", "\u{feff}{\"title\":\"T\"}");
        let plan = read_plan(&path).unwrap();
        assert_eq!((plan.id.as_str(), plan.title.as_str()), ("p-xyz", "T"));
        assert!(matches!(
            read_plan(&s.0.join("nope.json")),
            Err(Error::Io { .. })
        ));
        let bad = s.write("p-bad.json", "\"text\"");
        assert!(matches!(read_plan(&bad), Err(Error::Json { .. })));
    }

    #[test]
    fn settings_default_when_missing_or_bad() {
        let s = Scratch::new("settings");
        assert_eq!(read_settings(&s.0), ProjectSettings::default());
        s.write("_project.d/settings.json", SETTINGS);
        assert_eq!(read_settings(&s.0).card_types.len(), 5);
        s.write("_project.d/settings.json", "oops");
        assert_eq!(read_settings(&s.0), ProjectSettings::default());
    }

    #[test]
    fn activity_returns_the_tail_and_skips_bad_lines() {
        let s = Scratch::new("activity");
        let plan = s.write("p-aaa.json", "{}");
        let log = s.write(
            "p-aaa.activity.jsonl",
            &format!("{ACTIVITY}garbage\n\n{{\"at\":5,\"op\":\"x\"}}\n"),
        );
        assert_eq!(activity_path(&plan), log);

        let all = read_activity(&log, 100);
        assert_eq!(all.len(), 4);
        assert_eq!(all[0].op, "createPlan");
        assert_eq!(all[0].rev, Some(1));
        assert!(all[1].milestone_id.is_some());

        let tail = read_activity(&log, 2);
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[1].at, 5);
        assert!(read_activity(&s.0.join("none.jsonl"), 5).is_empty());
    }

    #[test]
    fn finds_projects() {
        let s = Scratch::new("projects");
        let repo = s.write("repo/.maomi/board/p-a.json", "{}");
        let repo = repo.ancestors().nth(3).unwrap().to_owned();
        let kept_local = s.write("elsewhere/readme.txt", "");
        let kept_local = kept_local.parent().unwrap().to_owned();
        fs::create_dir_all(s.0.join("home/boards/kept-local")).unwrap();
        fs::create_dir_all(s.0.join("home/boards/loose")).unwrap();
        fs::create_dir_all(s.0.join("home/boards/no-folder")).unwrap();
        let listed = serde_json::json!({ "projects": [
            { "key": "in-repo", "path": repo },
            { "key": "kept-local", "path": kept_local },
            { "key": "gone", "path": s.0.join("deleted") },
            { "key": "no-folder" },
            { "key": "blank", "path": "" },
            { "key": 7 },
            { "key": "in-repo", "path": repo },
        ]});
        s.write("home/board-projects.json", &listed.to_string());

        let found = projects_in(&s.0.join("home"));
        let keys: Vec<_> = found.iter().map(|p| p.key.as_str()).collect();
        assert_eq!(
            keys,
            ["in-repo", "kept-local", "no-folder", "blank", "loose"]
        );

        assert_eq!(found[0].board_dir, repo.join(".maomi").join("board"));
        assert_eq!(found[0].name, "repo");
        assert_eq!(found[1].board_dir, s.0.join("home/boards/kept-local"));
        assert_eq!(found[2].path, None);
        assert_eq!(found[2].board_dir, s.0.join("home/boards/no-folder"));
        assert_eq!(found[3].board_dir, s.0.join("home/boards/blank"));
        assert_eq!(found[4].name, "loose");

        // No board-projects.json at all: only the boards folder counts.
        assert!(projects_in(&s.0.join("empty")).is_empty());
    }
}
