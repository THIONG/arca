//! Persistent user preferences for the Arca GUI.

use crate::i18n::Lang;
use crate::{Columns, ThemePreference, CELL_LEAST, CELL_WIDE, NAME_LEAST, NAME_WIDE};
use std::fs;
use std::path::PathBuf;

pub(crate) fn config_file() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    }?;
    Some(base.join("Arca").join("gui.conf"))
}

pub(crate) struct Settings {
    pub(crate) lang: Option<Lang>,
    pub(crate) theme: ThemePreference,
    pub(crate) columns: Columns,
    // Whether to ask, once at startup, if there is a newer Arca.
    pub(crate) updates: bool,
    // Every file in the archive at once, instead of one folder at a time.
    pub(crate) flat: bool,
    // The folders of the archive down the left hand side.
    pub(crate) tree: bool,
    // Which code page an unflagged zip has its names written in. Only the
    // person looking at the archive can know, so it is remembered: somebody
    // whose archives all come from one machine says it once.
    pub(crate) page: arca_zip::pages::Page,
    // Where the window was left and how big: x, y, width, height. None until it
    // has been opened once.
    pub(crate) window: Option<[f32; 4]>,
    // The archives opened lately, newest first. Paths, so that one that has
    // since been moved can be noticed and dropped rather than opened blind.
    pub(crate) recent: Vec<String>,
    // How wide each column is: the name first, then the ones that can be
    // turned off, in the order of `Columns::ALL`. A column keeps its width
    // while it is off, so turning one back on does not lose how it was set.
    pub(crate) widths: Vec<f32>,
}

impl Settings {
    // What a column starts out at, before anybody has pulled on it.
    pub(crate) fn default_widths() -> Vec<f32> {
        std::iter::once(NAME_WIDE)
            .chain(std::iter::repeat(CELL_WIDE).take(Columns::ALL.len()))
            .collect()
    }

    // The least a column can be pulled down to, by its place in `widths`.
    pub(crate) fn least(slot: usize) -> f32 {
        if slot == 0 {
            NAME_LEAST
        } else {
            CELL_LEAST
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            lang: None,
            theme: ThemePreference::System,
            columns: Columns::default(),
            updates: true,
            flat: false,
            tree: false,
            page: arca_zip::pages::Page::default(),
            window: None,
            recent: Vec::new(),
            widths: Settings::default_widths(),
        }
    }
}

impl Settings {
    pub(crate) fn load() -> Self {
        let Some(p) = config_file() else {
            return Settings::default();
        };
        let Ok(text) = fs::read_to_string(p) else {
            return Settings::default();
        };
        Settings::parse(&text)
    }

    /// The settings file read back out of text.
    ///
    /// Apart from `load` so that what `text` writes can be read back and
    /// compared without going near a disk.
    pub(crate) fn parse(text: &str) -> Settings {
        let mut s = Settings::default();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            match (k.trim(), v.trim()) {
                ("lang", "system") => s.lang = None,
                ("lang", other) => s.lang = Lang::from_code(other),
                ("theme", "light") => s.theme = ThemePreference::Light,
                ("theme", "dark") => s.theme = ThemePreference::Dark,
                ("theme", _) => s.theme = ThemePreference::System,
                ("flat", v) => s.flat = v == "yes",
                ("tree", v) => s.tree = v == "yes",
                ("updates", v) => s.updates = v != "no",
                ("page", v) => {
                    if let Some(p) = arca_zip::pages::Page::from_code(v) {
                        s.page = p;
                    }
                }
                // One line each, because a path can hold anything a filename
                // can and there is no separator left that it could not.
                ("recent", p) if !p.is_empty() => s.recent.push(p.to_string()),
                ("window", v) => {
                    let n: Vec<f32> = v.split(',').filter_map(|x| x.trim().parse().ok()).collect();
                    if let [x, y, w, h] = n[..] {
                        // A window smaller than the minimum, or one left on a
                        // screen that is no longer plugged in, is not a window
                        // anybody can use.
                        if w >= 720.0 && h >= 320.0 {
                            s.window = Some([x, y, w, h]);
                        }
                    }
                }
                // Written since the columns could first be turned off and read
                // by nobody, so every window opened with the six of them
                // showing however they had been left.
                ("columns", list) => {
                    let mut c = Columns::default();
                    for (which, _) in Columns::ALL {
                        c.set(which, false);
                    }
                    for name in list.split(',').map(str::trim) {
                        if let Some((which, _)) = Columns::ALL.iter().find(|(_, n)| *n == name) {
                            c.set(*which, true);
                        }
                    }
                    s.columns = c;
                }
                // All of them or none: a line with a column missing from it
                // belongs to a different set of columns than this one has, and
                // guessing which is which would put the widths on the wrong
                // ones. Each is held above its floor in case the file was
                // written by hand.
                ("widths", list) => {
                    let read: Vec<f32> = list
                        .split(',')
                        .filter_map(|n| n.trim().parse::<f32>().ok())
                        .enumerate()
                        .map(|(i, w)| w.max(Settings::least(i)))
                        .collect();
                    if read.len() == s.widths.len() {
                        s.widths = read;
                    }
                }
                _ => {}
            }
        }
        s
    }

    pub(crate) fn save(&self) {
        let Some(p) = config_file() else { return };
        if let Some(dir) = p.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let _ = fs::write(p, self.text());
    }

    /// The settings file as text.
    ///
    /// Apart from `save` so that what is written can be read back and compared
    /// without going near a disk. That is not tidiness: six settings were being
    /// read at startup and never written, because the line that builds this had
    /// quietly stopped mentioning them -- `flat`, `tree`, `page`, `window` and
    /// `recent` were all built into a string that was then thrown away. A round
    /// trip that never touches a file is the only way that stays fixed.
    pub(crate) fn text(&self) -> String {
        let lang = self.lang.map(|l| l.code()).unwrap_or("system");
        let theme = match self.theme {
            ThemePreference::Light => "light",
            ThemePreference::Dark => "dark",
            ThemePreference::System => "system",
        };
        let yes = |b: bool| if b { "yes" } else { "no" };
        let columns: Vec<&str> = Columns::ALL
            .iter()
            .filter(|(which, _)| self.columns.on(*which))
            .map(|(_, name)| *name)
            .collect();
        let widths: Vec<String> = self.widths.iter().map(|w| format!("{w:.1}")).collect();

        let mut out = String::new();
        out.push_str(&format!("lang = {lang}\n"));
        out.push_str(&format!("theme = {theme}\n"));
        out.push_str(&format!("flat = {}\n", yes(self.flat)));
        out.push_str(&format!("tree = {}\n", yes(self.tree)));
        out.push_str(&format!("updates = {}\n", yes(self.updates)));
        out.push_str(&format!("page = {}\n", self.page.code()));
        out.push_str(&format!("columns = {}\n", columns.join(",")));
        out.push_str(&format!("widths = {}\n", widths.join(",")));
        if let Some([x, y, w, h]) = self.window {
            out.push_str(&format!("window = {x:.0},{y:.0},{w:.0},{h:.0}\n"));
        }
        // One line each: a path can hold anything a filename can and there is
        // no separator left that it could not.
        for path in &self.recent {
            out.push_str(&format!("recent = {path}\n"));
        }
        out
    }

    pub(crate) fn effective_lang(&self) -> Lang {
        self.lang.unwrap_or_else(Lang::from_system)
    }
}
