//! Browsing one archive's folders and files from the catalogue, like a file
//! manager, without unpacking it.

use std::collections::HashMap;

use eframe::egui::{self, RichText};

use atticarc_core::catalog::{Catalog, FoundFile};

use crate::{human, theme};

#[derive(Clone, Copy, PartialEq, Eq)]
enum SortBy {
    Name,
    Type,
    Size,
    Modified,
}

enum Item {
    /// A folder's full path inside the archive.
    Folder(String),
    /// An index into `Browse::files`.
    File(usize),
}

pub struct Browse {
    pub archive_id: i64,
    files: Vec<FoundFile>,
    /// Total size and file count of every folder, including what is in its
    /// subfolders, keyed by its path ("" is the archive's top level).
    folders: HashMap<String, (u64, u64)>,
    /// The folder being shown, `/` separated ("" is the top level).
    dir: String,
    listing: Vec<Item>,
    /// An index into `files`.
    selected: Option<usize>,
    sort: SortBy,
    descending: bool,
}

impl Browse {
    pub fn load(cat: &Catalog, archive_id: i64) -> anyhow::Result<Self> {
        let c = cat.contents(archive_id)?;
        let mut folders: HashMap<String, (u64, u64)> = HashMap::new();
        folders.insert(String::new(), (0, 0));
        for d in &c.folders {
            for a in ancestors(d).chain(std::iter::once(d.as_str())) {
                folders.entry(a.to_string()).or_default();
            }
        }
        for f in &c.files {
            for a in ancestors(&f.rel_path) {
                let e = folders.entry(a.to_string()).or_default();
                e.0 += f.size;
                e.1 += 1;
            }
        }
        let mut b = Browse {
            archive_id,
            files: c.files,
            folders,
            dir: String::new(),
            listing: Vec::new(),
            selected: None,
            sort: SortBy::Name,
            descending: false,
        };
        b.relist();
        Ok(b)
    }

    pub fn selected_file(&self) -> Option<&FoundFile> {
        self.selected.and_then(|i| self.files.get(i))
    }

    /// Opens the folder holding `rel` and selects that file.
    pub fn select_path(&mut self, rel: &str) {
        self.open(parent(rel).to_string());
        self.selected = self.files.iter().position(|f| f.rel_path == rel);
    }

    pub fn open(&mut self, dir: String) {
        self.dir = dir;
        self.selected = None;
        self.relist();
    }

    fn relist(&mut self) {
        let mut dirs: Vec<Item> = self
            .folders
            .keys()
            .filter(|k| !k.is_empty() && parent(k) == self.dir)
            .map(|k| Item::Folder(k.clone()))
            .collect();
        let mut files: Vec<Item> = (0..self.files.len())
            .filter(|&i| parent(&self.files[i].rel_path) == self.dir)
            .map(Item::File)
            .collect();
        let key = |it: &Item| -> (u64, String) {
            let (name, size, modified) = match it {
                Item::Folder(p) => (leaf(p).to_string(), self.folders[p].0, String::new()),
                Item::File(i) => {
                    let f = &self.files[*i];
                    (leaf(&f.rel_path).to_string(), f.size, f.modified_utc.clone().unwrap_or_default())
                }
            };
            match self.sort {
                SortBy::Name => (0, name.to_lowercase()),
                SortBy::Type => (0, format!("{}\u{0}{}", file_type(&name).to_lowercase(), name.to_lowercase())),
                SortBy::Size => (size, name.to_lowercase()),
                SortBy::Modified => (0, format!("{modified}\u{0}{}", name.to_lowercase())),
            }
        };
        for v in [&mut dirs, &mut files] {
            v.sort_by_cached_key(key);
            if self.descending {
                v.reverse();
            }
        }
        // Folders stay above files whichever way the files are sorted, as in
        // Explorer; for Type, folders are all one type.
        dirs.append(&mut files);
        self.listing = dirs;
    }

    /// Each folder from the top level down to the open one, as (name, path).
    pub fn crumbs(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut at = 0;
        for seg in self.dir.split('/').filter(|s| !s.is_empty()) {
            at += seg.len();
            out.push((seg.to_string(), self.dir[..at].to_string()));
            at += 1;
        }
        out
    }

    /// The open folder's contents as a table. Clicking a folder opens it,
    /// clicking a file selects it.
    pub fn table(&mut self, ui: &mut egui::Ui) {
        let (size, count) = self.folders.get(&self.dir).copied().unwrap_or_default();
        let subfolders = self.listing.iter().filter(|i| matches!(i, Item::Folder(_))).count();
        let mut up = false;
        ui.horizontal(|ui| {
            up = ui
                .add_enabled(!self.dir.is_empty(), egui::Button::new("⬆ Up"))
                .on_hover_text("Open the parent folder")
                .clicked();
            ui.label(
            RichText::new(format!(
                "{subfolders} folders · {} files here · {} in all, {count} files",
                self.listing.len() - subfolders,
                human(size)
            ))
            .color(theme::muted()),
            );
        });
        if up {
            self.open(parent(&self.dir).to_string());
        }
        ui.separator();

        let widths = |ui: &egui::Ui| {
            let w = ui.available_width();
            let (t, s, m) = (125.0, 85.0, 120.0);
            [(w - t - s - m - 24.0).max(120.0), t, s, m]
        };
        let mut resort = None;
        ui.horizontal(|ui| {
            let w = widths(ui);
            for (i, (label, by)) in
                [("Name", SortBy::Name), ("Type", SortBy::Type), ("Size", SortBy::Size), ("Modified", SortBy::Modified)]
                    .into_iter()
                    .enumerate()
            {
                let arrow = if self.sort == by { if self.descending { " ▼" } else { " ▲" } } else { "" };
                let text = RichText::new(format!("{label}{arrow}")).color(theme::muted());
                let clicked = cell(ui, w[i], 22.0, i == 2, |ui| {
                    ui.add(egui::Button::new(text).frame(false)).on_hover_text("Sort by this column").clicked()
                });
                if clicked {
                    resort = Some(by);
                }
            }
        });
        if let Some(by) = resort {
            self.descending = self.sort == by && !self.descending;
            self.sort = by;
            self.relist();
        }

        let mut go = None;
        let row_h = ui.text_style_height(&egui::TextStyle::Body) + 10.0;
        egui::ScrollArea::vertical().id_salt("browse").auto_shrink([false, false]).show_rows(
            ui,
            row_h,
            self.listing.len(),
            |ui, range| {
                let w = widths(ui);
                for item in &self.listing[range] {
                    ui.horizontal(|ui| {
                        let (name, kind, size, modified, on) = match item {
                            Item::Folder(p) => {
                                let (s, n) = self.folders[p];
                                let size = format!("{} · {n}", human(s));
                                (format!("🗀  {}", leaf(p)), "Folder".to_string(), size, String::new(), false)
                            }
                            Item::File(i) => {
                                let f = &self.files[*i];
                                let name = leaf(&f.rel_path);
                                let modified = f.modified_utc.as_deref().map(short_time).unwrap_or_default();
                                // A reference file shows as the file it stands in for.
                                let shown = match name.strip_suffix(&format!(".{}", atticarc_core::archive::DEP_EXT)) {
                                    Some(stem) => format!("🔗  {stem}"),
                                    None => format!("🗋  {name}"),
                                };
                                (shown, file_type(name), human(f.size), modified, self.selected == Some(*i))
                            }
                        };
                        let resp = cell(ui, w[0], row_h - 4.0, false, |ui| {
                            // The empty right text pushes the name to the left edge.
                            let b = egui::Button::selectable(on, RichText::new(name).monospace()).right_text("").truncate();
                            ui.add(b.min_size(egui::vec2(ui.available_width(), 0.0)))
                        });
                        if resp.clicked() {
                            match item {
                                Item::Folder(p) => go = Some(p.clone()),
                                Item::File(i) => self.selected = Some(*i),
                            }
                        }
                        for (i, text) in [(1, kind), (2, size), (3, modified)] {
                            cell(ui, w[i], row_h - 4.0, i == 2, |ui| {
                                ui.add(egui::Label::new(RichText::new(text).color(theme::muted())).truncate())
                            });
                        }
                    });
                }
                if self.listing.is_empty() {
                    ui.label(RichText::new("This folder is empty.").color(theme::muted()));
                }
            },
        );
        if let Some(d) = go {
            self.open(d);
        }
    }
}

/// One table cell of a fixed size, its contents left aligned, or right
/// aligned for numbers.
fn cell<R>(ui: &mut egui::Ui, width: f32, height: f32, right: bool, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let layout = if right {
        egui::Layout::right_to_left(egui::Align::Center)
    } else {
        egui::Layout::left_to_right(egui::Align::Center)
    };
    ui.allocate_ui_with_layout(egui::vec2(width, height), layout, |ui| {
        ui.set_min_size(egui::vec2(width, height));
        add(ui)
    })
    .inner
}

/// `a/b/c` → "", "a", "a/b".
fn ancestors(p: &str) -> impl Iterator<Item = &str> {
    std::iter::once("").chain(p.match_indices('/').map(move |(i, _)| &p[..i]))
}

fn parent(p: &str) -> &str {
    p.rsplit_once('/').map(|x| x.0).unwrap_or("")
}

fn leaf(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

/// "2024-05-01T10:20:30Z" → "2024-05-01 10:20".
fn short_time(t: &str) -> String {
    t.get(..16).unwrap_or(t).replace('T', " ")
}

/// A plain description of a file from its extension, like Explorer's Type column.
pub fn file_type(name: &str) -> String {
    let Some((_, ext)) = name.rsplit_once('.').filter(|(stem, _)| !stem.is_empty()) else {
        return "File".into();
    };
    let ext = ext.to_ascii_lowercase();
    let kind = match ext.as_str() {
        "jpg" | "jpeg" => "JPEG image",
        "png" => "PNG image",
        "gif" => "GIF image",
        "bmp" => "Bitmap image",
        "tif" | "tiff" => "TIFF image",
        "webp" => "WebP image",
        "heic" => "HEIC photo",
        "raw" | "cr2" | "nef" | "arw" | "dng" => "Camera raw photo",
        "svg" => "SVG drawing",
        "ico" => "Icon",
        "psd" => "Photoshop image",
        "mp4" | "m4v" => "MP4 video",
        "mov" => "QuickTime video",
        "avi" => "AVI video",
        "mkv" => "Matroska video",
        "wmv" => "Windows Media video",
        "mpg" | "mpeg" => "MPEG video",
        "mp3" => "MP3 audio",
        "wav" => "WAV audio",
        "flac" => "FLAC audio",
        "m4a" | "aac" => "AAC audio",
        "ogg" => "Ogg audio",
        "wma" => "Windows Media audio",
        "mid" | "midi" => "MIDI file",
        "txt" => "Text document",
        "md" => "Markdown document",
        "log" => "Log file",
        "pdf" => "PDF document",
        "doc" | "docx" => "Word document",
        "xls" | "xlsx" => "Excel spreadsheet",
        "ppt" | "pptx" => "PowerPoint presentation",
        "odt" => "OpenDocument text",
        "rtf" => "Rich text document",
        "csv" => "CSV data",
        "json" => "JSON data",
        "xml" => "XML data",
        "htm" | "html" => "Web page",
        "zip" => "ZIP archive",
        "7z" => "7-Zip archive",
        "rar" => "RAR archive",
        "gz" | "tgz" => "Gzip archive",
        "tar" => "Tar archive",
        "iso" => "Disc image",
        "mrimg" => "Macrium image",
        "exe" => "Application",
        "msi" => "Windows installer",
        "dll" => "Application extension",
        "bat" | "cmd" => "Windows batch file",
        "ps1" => "PowerShell script",
        "lnk" => "Shortcut",
        "atticarcdep" => "AtticARC reference",
        "ini" | "cfg" | "conf" => "Settings file",
        "ttf" | "otf" => "Font",
        "rs" => "Rust source",
        "py" => "Python source",
        "js" | "ts" => "Script source",
        "c" | "cpp" | "h" | "cs" => "Source code",
        "db" | "sqlite" => "Database",
        "bin" | "dat" => "Data file",
        _ => return format!("{} file", ext.to_uppercase()),
    };
    kind.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_and_types() {
        assert_eq!(ancestors("a/b/c.txt").collect::<Vec<_>>(), ["", "a", "a/b"]);
        assert_eq!(ancestors("top.txt").collect::<Vec<_>>(), [""]);
        assert_eq!(parent("a/b/c.txt"), "a/b");
        assert_eq!(parent("top.txt"), "");
        assert_eq!(leaf("a/b/c.txt"), "c.txt");
        assert_eq!(file_type("Holiday.JPG"), "JPEG image");
        assert_eq!(file_type("notes.xyz"), "XYZ file");
        assert_eq!(file_type(".gitignore"), "File");
        assert_eq!(file_type("Makefile"), "File");
        assert_eq!(short_time("2024-05-01T10:20:30Z"), "2024-05-01 10:20");
    }
}
