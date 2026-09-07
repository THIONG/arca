use arca_core::Entry;
use eframe::egui;
use std::collections::BTreeMap;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Dir,
    Image,
    Text,
    Archive,
    Audio,
    Video,
    Other,
}

impl Kind {
    fn color(self) -> egui::Color32 {
        match self {
            Kind::Dir => egui::Color32::from_rgb(232, 184, 92),
            Kind::Image => egui::Color32::from_rgb(122, 192, 132),
            Kind::Text => egui::Color32::from_rgb(142, 172, 214),
            Kind::Archive => egui::Color32::from_rgb(190, 142, 214),
            Kind::Audio => egui::Color32::from_rgb(214, 142, 160),
            Kind::Video => egui::Color32::from_rgb(214, 160, 112),
            Kind::Other => egui::Color32::from_rgb(150, 152, 158),
        }
    }
}

pub fn kind_of(name: &str, is_dir: bool) -> Kind {
    if is_dir {
        return Kind::Dir;
    }
    let lower = name.to_ascii_lowercase();
    let ext = lower.rsplit('.').next().unwrap_or("");
    match ext {
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "svg" | "ico" | "tif" | "tiff" => {
            Kind::Image
        }
        "txt" | "md" | "log" | "csv" | "tsv" | "json" | "xml" | "yml" | "yaml" | "toml"
        | "ini" | "cfg" | "conf" | "rs" | "py" | "js" | "ts" | "html" | "css" | "sh" | "ps1" => {
            Kind::Text
        }
        "zip" | "tar" | "gz" | "tgz" | "7z" | "rar" | "xz" | "bz2" | "zst" => Kind::Archive,
        "mp3" | "wav" | "flac" | "ogg" | "m4a" | "aac" => Kind::Audio,
        "mp4" | "mkv" | "avi" | "mov" | "webm" | "wmv" => Kind::Video,
        _ => Kind::Other,
    }
}

pub fn draw_icon(ui: &mut egui::Ui, kind: Kind) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(15.0, 15.0), egui::Sense::hover());
    let p = ui.painter();
    let c = kind.color();
    let faded = egui::Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), 110);

    if kind == Kind::Dir {
        let tab = egui::Rect::from_min_size(
            rect.left_top() + egui::vec2(1.0, 2.5),
            egui::vec2(6.0, 2.5),
        );
        p.rect_filled(tab, 1.0, c);
        let body = egui::Rect::from_min_max(
            rect.left_top() + egui::vec2(1.0, 4.5),
            rect.right_bottom() - egui::vec2(1.0, 2.0),
        );
        p.rect_filled(body, 2.0, c);
        return;
    }

    let body = egui::Rect::from_min_max(
        rect.left_top() + egui::vec2(2.5, 1.5),
        rect.right_bottom() - egui::vec2(2.5, 1.5),
    );
    p.rect_filled(body, 1.5, faded);
    p.rect_stroke(body, 1.5, egui::Stroke::new(1.0_f32, c));
    let fold = vec![
        egui::pos2(body.right() - 4.5, body.top()),
        egui::pos2(body.right(), body.top() + 4.5),
        egui::pos2(body.right() - 4.5, body.top() + 4.5),
    ];
    p.add(egui::Shape::convex_polygon(fold, c, egui::Stroke::NONE));
}

pub struct Row {
    pub label: String,
    pub path: String,
    pub kind: Kind,
    pub is_dir: bool,
    pub entry: Option<usize>,
    pub size: u64,
    pub packed: u64,
    pub method: &'static str,
    pub encrypted: bool,
    pub count: usize,
}

fn normalized(e: &Entry) -> String {
    e.name.replace('\\', "/")
}

pub fn children_of(entries: &[Entry], dir: &str) -> Vec<Row> {
    let mut folders: BTreeMap<String, (u64, u64, usize)> = BTreeMap::new();
    let mut files: Vec<Row> = Vec::new();

    for (i, e) in entries.iter().enumerate() {
        let full = normalized(e);
        if !full.starts_with(dir) {
            continue;
        }
        let rest = full[dir.len()..].trim_end_matches('/');
        if rest.is_empty() {
            continue;
        }
        match rest.find('/') {
            Some(cut) => {
                let seg = rest[..cut].to_string();
                let slot = folders.entry(seg).or_insert((0, 0, 0));
                if !e.is_dir {
                    slot.0 += e.size;
                    slot.1 += e.compressed_size;
                    slot.2 += 1;
                }
            }
            None => {
                if e.is_dir {
                    folders.entry(rest.to_string()).or_insert((0, 0, 0));
                } else {
                    files.push(Row {
                        label: rest.to_string(),
                        path: full.clone(),
                        kind: kind_of(rest, false),
                        is_dir: false,
                        entry: Some(i),
                        size: e.size,
                        packed: e.compressed_size,
                        method: e.method.name(),
                        encrypted: e.encrypted,
                        count: 0,
                    });
                }
            }
        }
    }

    let mut rows: Vec<Row> = folders
        .into_iter()
        .map(|(name, (size, packed, count))| Row {
            path: format!("{dir}{name}/"),
            label: name,
            kind: Kind::Dir,
            is_dir: true,
            entry: None,
            size,
            packed,
            method: "",
            encrypted: false,
            count,
        })
        .collect();
    rows.append(&mut files);
    rows
}

pub fn entries_under(entries: &[Entry], prefix: &str) -> Vec<usize> {
    entries
        .iter()
        .enumerate()
        .filter(|(_, e)| normalized(e).starts_with(prefix))
        .map(|(i, _)| i)
        .collect()
}

pub fn parent_of(dir: &str) -> String {
    let trimmed = dir.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(cut) => trimmed[..cut + 1].to_string(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arca_core::Method;

    fn entry(name: &str, is_dir: bool, size: u64) -> Entry {
        Entry {
            name: name.to_string(),
            size,
            compressed_size: size,
            method: Method::Store,
            crc32: 0,
            is_dir,
            mtime: None,
            offset: 0,
            encrypted: false,
        }
    }

    fn corpus() -> Vec<Entry> {
        vec![
            entry("arbol/LEEME.md", false, 10),
            entry("arbol/docs/nota1.txt", false, 100),
            entry("arbol/docs/nota2.txt", false, 200),
            entry("arbol/img/foto.png", false, 300),
            entry("arbol/musica/cancion.mp3", false, 400),
        ]
    }

    #[test]
    fn root_shows_only_the_top_folder() {
        let rows = children_of(&corpus(), "");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label, "arbol");
        assert!(rows[0].is_dir);
        assert_eq!(rows[0].count, 5);
        assert_eq!(rows[0].size, 1010);
    }

    #[test]
    fn folders_come_before_files_and_sizes_add_up() {
        let rows = children_of(&corpus(), "arbol/");
        let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels, vec!["docs", "img", "musica", "LEEME.md"]);
        assert!(rows[0].is_dir && rows[1].is_dir && rows[2].is_dir);
        assert!(!rows[3].is_dir);
        assert_eq!(rows[0].count, 2);
        assert_eq!(rows[0].size, 300);
    }

    #[test]
    fn descending_and_going_back_up_lands_where_it_started() {
        let rows = children_of(&corpus(), "arbol/docs/");
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| !r.is_dir));
        assert_eq!(parent_of("arbol/docs/"), "arbol/");
        assert_eq!(parent_of("arbol/"), "");
        assert_eq!(parent_of(""), "");
    }

    #[test]
    fn folders_appear_even_without_their_own_entry() {
        let flat = vec![entry("a/b/c/deep.txt", false, 7)];
        assert_eq!(children_of(&flat, "").len(), 1);
        assert_eq!(children_of(&flat, "a/")[0].label, "b");
        assert_eq!(children_of(&flat, "a/b/")[0].label, "c");
        assert_eq!(children_of(&flat, "a/b/c/")[0].label, "deep.txt");
    }

    #[test]
    fn explicit_directory_entries_do_not_duplicate_rows() {
        let mixed = vec![
            entry("d/", true, 0),
            entry("d/x.txt", false, 5),
            entry("d/sub/", true, 0),
            entry("d/sub/y.txt", false, 5),
        ];
        let root = children_of(&mixed, "");
        assert_eq!(root.len(), 1, "the folder must appear once, not twice");
        let inside = children_of(&mixed, "d/");
        let labels: Vec<&str> = inside.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels, vec!["sub", "x.txt"]);
    }

    #[test]
    fn checking_a_folder_reaches_every_entry_inside_it() {
        let c = corpus();
        assert_eq!(entries_under(&c, "arbol/docs/").len(), 2);
        assert_eq!(entries_under(&c, "arbol/").len(), 5);
        assert_eq!(entries_under(&c, "nothing/").len(), 0);
    }

    #[test]
    fn garbage_names_do_not_panic() {
        let nasty = vec![
            entry("", false, 0),
            entry("/", false, 0),
            entry("///", false, 0),
            entry("..", false, 0),
            entry("a//b", false, 1),
            entry(r"windows\style\path.txt", false, 1),
            entry("trailing/", true, 0),
            entry("\u{0}weird", false, 1),
        ];
        for dir in ["", "a/", "windows/", "trailing/", "///"] {
            let _ = children_of(&nasty, dir);
            let _ = entries_under(&nasty, dir);
        }
        let _ = parent_of("///");
    }

    #[test]
    fn kinds_are_recognized_by_extension() {
        assert_eq!(kind_of("x.PNG", false), Kind::Image);
        assert_eq!(kind_of("x.txt", false), Kind::Text);
        assert_eq!(kind_of("x.zip", false), Kind::Archive);
        assert_eq!(kind_of("x.mp3", false), Kind::Audio);
        assert_eq!(kind_of("x.mkv", false), Kind::Video);
        assert_eq!(kind_of("noextension", false), Kind::Other);
        assert_eq!(kind_of("whatever", true), Kind::Dir);
    }
}
