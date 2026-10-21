//! Open windows, tabs and splits, saved on quit and reopened on the next start.
//!
//! Kept apart from `config.json` because it is app state, not settings the user edits. Only
//! the shape and each pane's folder are kept; shells always start fresh.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{ConfigError, write_atomic};

const FILE_NAME: &str = "layout.json";

/// Schema version written by this build. Files from other versions are ignored.
pub const LAYOUT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Layout {
    pub version: u32,
    pub windows: Vec<WindowLayout>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowLayout {
    /// None = let the OS place the window.
    pub bounds: Option<WindowBounds>,
    pub maximized: bool,
    pub dock_open: bool,
    pub active_tab: usize,
    pub tabs: Vec<TabLayout>,
}

/// Window rectangle in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowBounds {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TabLayout {
    pub root: PaneLayout,
    /// Leaf index (reading order) of the focused pane.
    #[serde(default)]
    pub focused: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PaneLayout {
    Pane {
        #[serde(default)]
        cwd: Option<PathBuf>,
        /// None = the pane never opened its drawer.
        #[serde(default)]
        drawer: Option<DrawerLayout>,
    },
    Split {
        /// True = children side by side; false = stacked.
        horizontal: bool,
        children: Vec<PaneLayout>,
    },
}

/// A pane's file drawer.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DrawerLayout {
    pub open: bool,
    /// Logical pixels.
    pub width: f32,
    /// Opening a folder in the drawer runs `cd` in the shell.
    pub follow: bool,
}

impl PaneLayout {
    /// Panes in this subtree.
    pub fn leaf_count(&self) -> usize {
        match self {
            Self::Pane { .. } => 1,
            Self::Split { children, .. } => children.iter().map(Self::leaf_count).sum(),
        }
    }
}

impl Layout {
    pub fn path() -> Result<PathBuf, ConfigError> {
        Ok(crate::config_dir()?.join(FILE_NAME))
    }

    /// The saved layout; empty when there is none or it cannot be read.
    pub fn load() -> Self {
        Self::path()
            .map(|p| Self::load_from(&p))
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<(), ConfigError> {
        self.save_to(&Self::path()?)
    }

    fn load_from(path: &Path) -> Self {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) => {
                if err.kind() != io::ErrorKind::NotFound {
                    tracing::warn!(%err, "cannot read layout.json; starting fresh");
                }
                return Self::default();
            }
        };
        match serde_json::from_str::<Self>(&text) {
            Ok(layout) if layout.version == LAYOUT_VERSION => layout.cleaned(),
            Ok(_) => Self::default(),
            Err(err) => {
                tracing::warn!(%err, "layout.json is not valid; starting fresh");
                Self::default()
            }
        }
    }

    fn save_to(&self, path: &Path) -> Result<(), ConfigError> {
        let layout = Self {
            version: LAYOUT_VERSION,
            ..self.clone()
        };
        write_atomic(path, &serde_json::to_string_pretty(&layout)?)
    }

    /// Drops windows without tabs, empty splits and out-of-range indices.
    fn cleaned(mut self) -> Self {
        for window in &mut self.windows {
            window.tabs.retain_mut(|tab| {
                prune(&mut tab.root);
                tab.root.leaf_count() > 0
            });
            for tab in &mut window.tabs {
                tab.focused = tab.focused.min(tab.root.leaf_count() - 1);
            }
            window.active_tab = window.active_tab.min(window.tabs.len().saturating_sub(1));
        }
        self.windows.retain(|w| !w.tabs.is_empty());
        self
    }
}

/// Removes empty splits and collapses splits with one child into that child.
fn prune(node: &mut PaneLayout) {
    if let PaneLayout::Split { children, .. } = node {
        children.iter_mut().for_each(prune);
        children.retain(|c| c.leaf_count() > 0);
        if children.len() == 1 {
            *node = children.remove(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(cwd: &str) -> PaneLayout {
        PaneLayout::Pane {
            cwd: Some(cwd.into()),
            drawer: Some(DrawerLayout {
                open: true,
                width: 240.,
                follow: false,
            }),
        }
    }

    #[test]
    fn round_trips() {
        let dir = std::env::temp_dir().join(format!("maomi-layout-rt-{}", std::process::id()));
        let path = dir.join(FILE_NAME);
        let layout = Layout {
            version: LAYOUT_VERSION,
            windows: vec![WindowLayout {
                bounds: Some(WindowBounds {
                    x: 10.0,
                    y: 20.0,
                    width: 900.0,
                    height: 600.0,
                }),
                maximized: false,
                dock_open: true,
                active_tab: 1,
                tabs: vec![
                    TabLayout {
                        root: pane("/a"),
                        focused: 0,
                    },
                    TabLayout {
                        root: PaneLayout::Split {
                            horizontal: true,
                            children: vec![pane("/b"), pane("/c")],
                        },
                        focused: 1,
                    },
                ],
            }],
        };
        layout.save_to(&path).unwrap();
        assert_eq!(Layout::load_from(&path), layout);
        _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn cleaning_fixes_bad_shapes() {
        let layout = Layout {
            version: LAYOUT_VERSION,
            windows: vec![
                WindowLayout {
                    active_tab: 9,
                    tabs: vec![TabLayout {
                        root: PaneLayout::Split {
                            horizontal: false,
                            children: vec![
                                PaneLayout::Split {
                                    horizontal: true,
                                    children: vec![],
                                },
                                pane("/x"),
                            ],
                        },
                        focused: 5,
                    }],
                    ..WindowLayout::default()
                },
                WindowLayout::default(),
            ],
        }
        .cleaned();
        assert_eq!(layout.windows.len(), 1);
        let window = &layout.windows[0];
        assert_eq!(window.active_tab, 0);
        assert_eq!(window.tabs[0].root, pane("/x"));
        assert_eq!(window.tabs[0].focused, 0);
    }

    #[test]
    fn other_versions_are_ignored() {
        let dir = std::env::temp_dir().join(format!("maomi-layout-ver-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE_NAME);
        fs::write(&path, r#"{"version": 99, "windows": []}"#).unwrap();
        assert_eq!(Layout::load_from(&path), Layout::default());
        _ = fs::remove_dir_all(dir);
    }
}
