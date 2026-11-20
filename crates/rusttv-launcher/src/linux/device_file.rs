//! Device files: controls a particular device adds to Settings (and to the
//! side menu over apps), described in `/etc/rusttv/device.d/*.toml` by the
//! device's image instead of in the launcher. A projector adds its lens
//! focus this way:
//!
//! ```toml
//! [[tab]]
//! id = "projector"
//! title = "Projector"
//! titles = { de = "Projektor" }
//!
//! [[tab.control]]
//! id = "focus"
//! label = "Focus"
//! labels = { de = "Fokus" }
//! # Left and Right step it; nothing to show but the picture itself.
//! kind = "nudge"
//! # While it moves, a test pattern shows over everything.
//! pattern = true
//! # Also a row of the side menu over apps (holding Home).
//! side_menu = true
//! # The remote's keys for it (evdev codes, Left then Right; here
//! # KEY_PROG3/KEY_PROG4): the side menu takes them itself while it is open
//! # (it has the keyboard then, so the compositor's bindings don't run).
//! keys = [202, 203]
//! # Run as the launcher's user, without a shell.
//! left = ["hy320f-focus", "down"]
//! right = ["hy320f-focus", "up"]
//! ```
//!
//! The files are root-owned: they name commands. Files are read in name
//! order; a tab id already taken by an earlier file is skipped.

use std::collections::HashMap;
use std::path::Path;

use crate::device::{Control, ControlKind, Tab};

/// Where device files are.
pub const DIR: &str = "/etc/rusttv/device.d";

/// One device file.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    tab: Vec<FileTab>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FileTab {
    id: String,
    title: String,
    /// `title` in other languages, by language code (`de`, `pt_BR`).
    #[serde(default)]
    titles: HashMap<String, String>,
    #[serde(default)]
    control: Vec<FileControl>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FileControl {
    id: String,
    label: String,
    #[serde(default)]
    labels: HashMap<String, String>,
    kind: Kind,
    #[serde(default)]
    pattern: bool,
    #[serde(default)]
    side_menu: bool,
    #[serde(default)]
    keys: Option<[u32; 2]>,
    left: Vec<String>,
    right: Vec<String>,
}

/// What a device file's control is (only nudges so far).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum Kind {
    /// [`ControlKind::Nudge`].
    Nudge,
}

/// A control the side menu over apps shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SideRow {
    /// Its tab's id and its own, for [`DeviceControls::command`].
    pub tab: String,
    pub id: String,
    /// In the UI's language.
    pub label: String,
    /// Shows the test pattern while it is the focused row.
    pub pattern: bool,
    /// The keys that step it left and right (evdev codes).
    pub keys: Option<[u32; 2]>,
}

/// The controls of every device file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceControls {
    tabs: Vec<FileTab>,
    /// The UI's language (`LANG` style, e.g. `de_DE.UTF-8`).
    language: Option<String>,
}

impl DeviceControls {
    /// Reads the `*.toml` files in `dir`; a file that doesn't parse is
    /// skipped with a warning.
    pub fn load(dir: &Path, language: Option<String>) -> Self {
        let mut paths: Vec<_> = std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
                    .collect()
            })
            .unwrap_or_default();
        paths.sort();
        let mut controls = Self {
            tabs: Vec::new(),
            language,
        };
        for path in paths {
            let parsed = std::fs::read_to_string(&path)
                .map_err(|error| error.to_string())
                .and_then(|text| parse(&text));
            match parsed {
                Ok(tabs) => controls.add(tabs),
                Err(error) => log::warn!("{}: {error}", path.display()),
            }
        }
        controls
    }

    fn add(&mut self, tabs: Vec<FileTab>) {
        for tab in tabs {
            if self.tabs.iter().any(|known| known.id == tab.id) {
                log::warn!("device tab {} is described twice; the first counts", tab.id);
                continue;
            }
            self.tabs.push(tab);
        }
    }

    /// The Settings tabs, in the UI's language.
    pub fn tabs(&self) -> Vec<Tab> {
        self.tabs
            .iter()
            .map(|tab| Tab {
                id: tab.id.clone(),
                title: self.pick(&tab.title, &tab.titles),
                controls: tab
                    .control
                    .iter()
                    .map(|control| Control {
                        id: control.id.clone(),
                        label: self.pick(&control.label, &control.labels),
                        detail: String::new(),
                        kind: match control.kind {
                            Kind::Nudge => ControlKind::Nudge,
                        },
                    })
                    .collect(),
            })
            .collect()
    }

    /// The command that steps control `id` of tab `tab` (`step` > 0:
    /// right); `None` if no device file has it.
    pub fn command(&self, tab: &str, id: &str, step: i32) -> Option<&[String]> {
        let control = self.find(tab, id)?;
        Some(if step > 0 {
            &control.right
        } else {
            &control.left
        })
    }

    /// Whether control `id` of tab `tab` shows the test pattern.
    pub fn pattern(&self, tab: &str, id: &str) -> bool {
        self.find(tab, id).is_some_and(|control| control.pattern)
    }

    /// The controls of the side menu over apps, in file order.
    pub fn side_menu(&self) -> Vec<SideRow> {
        self.tabs
            .iter()
            .flat_map(|tab| {
                tab.control
                    .iter()
                    .filter(|control| control.side_menu)
                    .map(|control| SideRow {
                        tab: tab.id.clone(),
                        id: control.id.clone(),
                        label: self.pick(&control.label, &control.labels),
                        pattern: control.pattern,
                        keys: control.keys,
                    })
            })
            .collect()
    }

    fn find(&self, tab: &str, id: &str) -> Option<&FileControl> {
        self.tabs
            .iter()
            .find(|known| known.id == tab)?
            .control
            .iter()
            .find(|control| control.id == id)
    }

    /// `text` in the UI's language: `de_DE.UTF-8` tries `de_DE`, then
    /// `de`, then `text` itself.
    fn pick(&self, text: &str, translations: &HashMap<String, String>) -> String {
        let Some(language) = &self.language else {
            return text.to_owned();
        };
        let locale = language.split(['.', '@']).next().unwrap_or(language);
        let base = locale.split('_').next().unwrap_or(locale);
        translations
            .get(locale)
            .or_else(|| translations.get(base))
            .cloned()
            .unwrap_or_else(|| text.to_owned())
    }
}

/// A device file's tabs; every control needs ids, a label and both
/// commands.
fn parse(text: &str) -> Result<Vec<FileTab>, String> {
    let file: File = toml::from_str(text).map_err(|error| error.to_string())?;
    for tab in &file.tab {
        if tab.id.is_empty() || tab.title.is_empty() {
            return Err("a tab needs an id and a title".into());
        }
        for control in &tab.control {
            if control.id.is_empty() || control.label.is_empty() {
                return Err(format!(
                    "a control of tab {} needs an id and a label",
                    tab.id
                ));
            }
            if control.left.is_empty() || control.right.is_empty() {
                return Err(format!(
                    "control {} needs left and right commands",
                    control.id
                ));
            }
        }
    }
    Ok(file.tab)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FOCUS: &str = r#"
        [[tab]]
        id = "projector"
        title = "Projector"
        titles = { de = "Projektor" }

        [[tab.control]]
        id = "focus"
        label = "Focus"
        labels = { de = "Fokus" }
        kind = "nudge"
        pattern = true
        side_menu = true
        keys = [202, 203]
        left = ["hy320f-focus", "down"]
        right = ["hy320f-focus", "up"]

        [[tab.control]]
        id = "zoom"
        label = "Zoom"
        kind = "nudge"
        left = ["zoom", "out"]
        right = ["zoom", "in"]
    "#;

    fn controls(language: Option<&str>) -> DeviceControls {
        let mut controls = DeviceControls {
            tabs: Vec::new(),
            language: language.map(str::to_owned),
        };
        controls.add(parse(FOCUS).unwrap());
        controls
    }

    #[test]
    fn tabs_and_labels_follow_the_language() {
        let tabs = controls(Some("de_DE.UTF-8")).tabs();
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0].title, "Projektor");
        assert_eq!(tabs[0].controls[0].label, "Fokus");
        assert_eq!(tabs[0].controls[0].kind, ControlKind::Nudge);
        // No translation: the plain text.
        assert_eq!(tabs[0].controls[1].label, "Zoom");
        assert_eq!(controls(None).tabs()[0].title, "Projector");
        assert_eq!(controls(Some("fr_FR.UTF-8")).tabs()[0].title, "Projector");
    }

    #[test]
    fn steps_run_their_side_s_command() {
        let controls = controls(None);
        assert_eq!(
            controls.command("projector", "focus", 1).unwrap(),
            ["hy320f-focus", "up"]
        );
        assert_eq!(
            controls.command("projector", "focus", -1).unwrap(),
            ["hy320f-focus", "down"]
        );
        assert_eq!(controls.command("projector", "brightness", 1), None);
        assert_eq!(controls.command("sound", "focus", 1), None);
        assert!(controls.pattern("projector", "focus"));
        assert!(!controls.pattern("projector", "zoom"));
    }

    #[test]
    fn only_side_menu_controls_go_to_the_side_menu() {
        assert_eq!(
            controls(Some("de")).side_menu(),
            [SideRow {
                tab: "projector".into(),
                id: "focus".into(),
                label: "Fokus".into(),
                pattern: true,
                keys: Some([202, 203]),
            }]
        );
    }

    #[test]
    fn broken_files_are_refused() {
        assert!(parse("[[tab]]\nid = \"x\"\n").is_err(), "no title");
        assert!(parse("[[tab]]\nid = \"x\"\ntitle = \"X\"\nmystery = 1\n").is_err());
        let no_command = r#"
            [[tab]]
            id = "x"
            title = "X"
            [[tab.control]]
            id = "a"
            label = "A"
            kind = "nudge"
            left = []
            right = ["b"]
        "#;
        assert!(parse(no_command).is_err());
        assert!(parse("").unwrap().is_empty());
    }

    #[test]
    fn a_tab_described_twice_counts_once() {
        let mut twice = controls(None);
        twice.add(parse(FOCUS).unwrap());
        assert_eq!(twice.tabs().len(), 1);
    }

    #[test]
    fn a_missing_directory_has_no_controls() {
        let controls = DeviceControls::load(Path::new("/nonexistent/rusttv"), None);
        assert!(controls.tabs().is_empty());
    }
}
