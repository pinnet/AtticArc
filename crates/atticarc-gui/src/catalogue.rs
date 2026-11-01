//! Catalogue: browse each archive's folders, or search every file in every
//! archive, without unpacking; see a file's details and extract it. Mockup:
//! Catalogue.dc.html.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};

use eframe::egui::{self, RichText};

use atticarc_core::archive::{self, DEP_EXT, Replaced};
use atticarc_core::catalog::{self, ArchiveSummary, Catalog, FoundFile};

use crate::browse::Browse;
use crate::{App, card, human, primary_button, theme};

const MAX_RESULTS: usize = 500;

#[derive(Default)]
pub struct State {
    loaded: bool,
    query: String,
    /// The query and filter the results are for.
    searched: Option<(String, Option<i64>)>,
    archive_filter: Option<i64>,
    archives: Vec<ArchiveSummary>,
    results: Vec<FoundFile>,
    selected: Option<usize>,
    error: Option<String>,
    note: Option<String>,
    extracting: Option<Receiver<Result<PathBuf, String>>>,
    rebuilding: Option<Receiver<Result<archive::Rebuilt, String>>>,
    rebuild_note: Option<String>,
    /// The archive open in the browser, which is the one picked in the
    /// archive filter while there is no search.
    browse: Option<Browse>,
    confirm_clear: bool,
    /// Set after the catalogue was cleared, so the folder list reloads.
    cleared: bool,
    /// For the file shown, by entry id: what it stands for if it is a reference file.
    dep: Option<(i64, Option<Replaced>)>,
    /// For the file shown, by entry id: the reference files pointing to it.
    refs: Option<(i64, Vec<(String, String)>)>,
    /// Putting a replaced file back into its archive.
    putting_back: Option<(Arc<archive::Progress>, Receiver<Result<usize, String>>)>,
    /// A file to show once its archive is open in the browser: (archive id, path).
    goto: Option<(i64, String)>,
    /// Set when an archive was rewritten, so other pages reload.
    pub changed: bool,
}

impl State {
    pub fn invalidate(&mut self) {
        self.loaded = false;
        self.searched = None;
        self.browse = None;
    }

    /// Opens archive `id` in the browser, as if picked in the archive list.
    pub fn show_archive(&mut self, id: i64) {
        self.query.clear();
        self.archive_filter = Some(id);
        self.browse = None;
    }

    pub fn busy(&self) -> bool {
        self.extracting.is_some() || self.rebuilding.is_some() || self.putting_back.is_some()
    }

    /// Adds the archives in `out` that the catalogue is missing, from the
    /// manifests inside them, in the background.
    fn rebuild(&mut self, out: &std::path::Path) {
        let (tx, rx) = channel();
        let out = out.to_path_buf();
        std::thread::spawn(move || {
            let _ = tx.send(archive::rebuild_catalog(&out).map_err(|e| format!("{e:#}")));
        });
        self.rebuilding = Some(rx);
        self.rebuild_note = Some("Reading the manifests inside the archives…".into());
    }
}

impl App {
    pub fn catalogue(&mut self, ui: &mut egui::Ui) {
        let out = self.out_dir();
        if std::mem::take(&mut self.catalogue.cleared) {
            self.reload();
        }
        let busy = self.jobs.is_busy() || self.duplicates.busy();
        let settings = self.settings();
        let st = &mut self.catalogue;
        if let Some((_, rx)) = &st.putting_back
            && let Ok(res) = rx.try_recv()
        {
            // Shown under the search bar, since the browser reloads.
            st.rebuild_note = Some(match res {
                Ok(_) => "The file was put back from its original; the archive was rewritten and verified.".into(),
                Err(e) => format!("Putting the file back failed, and the archive was left as it was: {e}"),
            });
            st.putting_back = None;
            st.changed = true;
            let keep = st.archive_filter;
            st.invalidate();
            st.archive_filter = keep;
        }
        if let Some(rx) = &st.extracting
            && let Ok(res) = rx.try_recv() {
                st.note = Some(match res {
                    Ok(p) => format!("Extracted to {}", p.display()),
                    Err(e) => format!("Extract failed: {e}"),
                });
                st.extracting = None;
            }
        if let Some(rx) = &st.rebuilding
            && let Ok(res) = rx.try_recv() {
                st.rebuild_note = Some(match res {
                    Ok(r) => {
                        let mut n = format!("Rebuilt from the archives: {} added, {} already in the catalogue.", r.added, r.already_there);
                        if !r.failed.is_empty() {
                            n.push_str(&format!(" {} couldn't be read:\n{}", r.failed.len(), r.failed.join("\n")));
                        }
                        n
                    }
                    Err(e) => format!("Rebuild failed: {e}"),
                });
                st.rebuilding = None;
                st.cleared = true;
                st.invalidate();
            }
        if !st.loaded {
            st.loaded = true;
            st.error = None;
            st.archives.clear();
            if catalog::exists(&out) {
                match Catalog::open_existing(&out).and_then(|c| c.archives()) {
                    Ok(a) => st.archives = a,
                    Err(e) => st.error = Some(format!("{e:#}")),
                }
            }
        }
        let want = (st.query.trim().to_string(), st.archive_filter);
        if !want.0.is_empty() && st.searched.as_ref() != Some(&want) {
            st.selected = None;
            st.results = Catalog::open_existing(&out)
                .and_then(|c| c.search(&want.0, want.1, MAX_RESULTS))
                .unwrap_or_else(|e| {
                    st.error = Some(format!("{e:#}"));
                    Vec::new()
                });
            st.selected = (!st.results.is_empty()).then_some(0);
            st.searched = Some(want);
        }

        if st.query.trim().is_empty() {
            load_browse(st, &out);
        }
        title(ui, st);
        ui.label(
            RichText::new(format!("Every file in every archive in {}, to browse or search without unpacking.", out.display()))
                .color(theme::muted()),
        );
        ui.add_space(10.0);
        if let Some(e) = &st.error {
            ui.colored_label(theme::danger(), e);
        }
        if st.archives.is_empty() {
            card(ui, |ui| {
                ui.label("No archives in the catalogue. Archive some folders from New job, or if this folder already holds archives, rebuild the catalogue from them.");
                ui.add_space(6.0);
                let rebuild = egui::Button::new("Rebuild from archives");
                if ui.add_enabled(!busy && st.rebuilding.is_none(), rebuild).clicked() {
                    st.rebuild(&out);
                }
                for n in [&st.note, &st.rebuild_note].into_iter().flatten() {
                    ui.label(RichText::new(n).color(theme::muted()));
                }
            });
            return;
        }

        ui.horizontal(|ui| {
            ui.label(RichText::new("Search files").strong());
            ui.add(egui::TextEdit::singleline(&mut st.query).hint_text("name or folder, e.g. holiday 2014").desired_width(360.0));
            let current = st
                .archive_filter
                .and_then(|id| st.archives.iter().find(|a| a.id == id))
                .map(|a| a.name.clone())
                .unwrap_or_else(|| "All archives".into());
            egui::ComboBox::from_id_salt("archive-filter").selected_text(current).show_ui(ui, |ui| {
                ui.selectable_value(&mut st.archive_filter, None, "All archives");
                for a in &st.archives {
                    ui.selectable_value(&mut st.archive_filter, Some(a.id), &a.name);
                }
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let clear = egui::Button::new(RichText::new("Clear the catalogue…").color(theme::danger()));
                if ui.add_enabled(!busy, clear).on_hover_text("For development: forget every archive in this catalogue").clicked() {
                    st.confirm_clear = true;
                }
                let rebuild = egui::Button::new("Rebuild from archives");
                let tip = "Add any archives in this folder that the catalogue is missing, from the manifests inside them";
                if ui.add_enabled(!busy && st.rebuilding.is_none(), rebuild).on_hover_text(tip).clicked() {
                    st.rebuild(&out);
                }
            });
        });
        if let Some(n) = &st.rebuild_note {
            ui.label(RichText::new(n).color(theme::muted()));
        }
        ui.add_space(8.0);
        if st.confirm_clear {
            confirm_clear(ui.ctx(), st, &out);
        }

        if st.query.trim().is_empty() {
            match st.archive_filter {
                Some(id) => browse_view(ui, st, id, &out, &settings),
                None => archive_list(ui, st),
            }
            return;
        }

        ui.columns(2, |c| {
            card(&mut c[0], |ui| {
                let more = if st.results.len() == MAX_RESULTS { " (first 500, largest first)" } else { "" };
                ui.label(RichText::new(format!("{} matches{more} · click a file for details", st.results.len())).color(theme::muted()));
                ui.separator();
                egui::ScrollArea::vertical().id_salt("results").auto_shrink([false, false]).max_height(ui.available_height()).show(ui, |ui| {
                    for (i, f) in st.results.iter().enumerate() {
                        let name = f.rel_path.rsplit('/').next().unwrap_or(&f.rel_path);
                        let folder = f.rel_path.rsplit_once('/').map(|x| x.0).unwrap_or("");
                        let text = format!("{name}\n{} › {folder}", f.archive_name);
                        let on = st.selected == Some(i);
                        ui.horizontal(|ui| {
                            if ui.add(egui::Button::selectable(on, RichText::new(text).monospace()).min_size(egui::vec2(ui.available_width() - 90.0, 0.0))).clicked() {
                                st.selected = Some(i);
                            }
                            ui.label(human(f.size));
                        });
                    }
                });
            });
            card(&mut c[1], |ui| match st.selected.and_then(|i| st.results.get(i)).cloned() {
                None => {
                    ui.label("No file selected.");
                }
                Some(f) => details(ui, st, &f, &out, &settings),
            });
        });
    }
}

fn confirm_clear(ctx: &egui::Context, st: &mut State, out: &std::path::Path) {
    let mut close = false;
    egui::Modal::new(egui::Id::unique("clear-catalogue")).show(ctx, |ui| {
        ui.set_max_width(460.0);
        ui.heading("Clear the catalogue?");
        ui.label(format!(
            "Forgets all {} archives recorded in {}. The .7z files stay on disk with their manifests \
             inside them, so Rebuild from archives can bring the records back. Until then their \
             folders show as not archived.",
            st.archives.len(),
            catalog::db_path(out).display()
        ));
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let clear = egui::Button::new(RichText::new("Clear the catalogue").color(egui::Color32::WHITE)).fill(theme::danger_fill());
            if ui.add(clear).clicked() {
                match Catalog::open_existing(out).and_then(|mut c| c.clear()) {
                    Ok(n) => {
                        st.note = Some(format!("Catalogue cleared: {n} archives forgotten."));
                        st.rebuild_note = None;
                        st.cleared = true;
                        st.invalidate();
                        st.archive_filter = None;
                        st.query.clear();
                        st.results.clear();
                    }
                    Err(e) => st.error = Some(format!("Couldn't clear the catalogue: {e:#}")),
                }
                close = true;
            }
            if ui.button("Cancel").clicked() {
                close = true;
            }
        });
    });
    if close {
        st.confirm_clear = false;
    }
}

/// Opens the archive picked in the archive list in the browser, if it isn't
/// open already.
fn load_browse(st: &mut State, out: &std::path::Path) {
    let Some(id) = st.archive_filter else { return };
    if st.browse.as_ref().map(|b| b.archive_id) == Some(id) {
        return;
    }
    st.browse = None;
    let Some(name) = st.archives.iter().find(|a| a.id == id).map(|a| a.name.clone()) else {
        st.archive_filter = None;
        return;
    };
    match Catalog::open_existing(out).and_then(|c| Browse::load(&c, id)) {
        Ok(mut b) => {
            if let Some((_, rel)) = st.goto.take().filter(|g| g.0 == id) {
                b.select_path(&rel);
            }
            st.browse = Some(b)
        }
        Err(e) => {
            st.error = Some(format!("Couldn't open {name}: {e:#}"));
            st.archive_filter = None;
        }
    }
}

/// The page title in the heading style, followed while browsing by the
/// archive and each folder down to the open one, highlighted in the accent
/// colour. Every part but the last can be clicked to go back up to it. The
/// path shrinks to stay on one line ("Catalogue" keeps its size); if even
/// the smallest size is too wide, folders in the middle fold into "…" first.
fn title(ui: &mut egui::Ui, st: &mut State) {
    const FULL: f32 = 26.0;
    const SMALLEST: f32 = 15.0;
    const GAP: f32 = 10.0;
    // The path is a little smaller than "Catalogue", 23 pt beside 26.
    const PATH: f32 = 23.0 / 26.0;
    let browsing = st.query.trim().is_empty() && st.browse.is_some();
    // (text, where clicking goes, highlighted): None goes to all archives.
    let mut parts: Vec<(String, Option<Option<String>>, bool)> = vec![("Catalogue".into(), Some(None), false)];
    let mut full_path = String::new();
    if browsing && let Some(b) = &st.browse {
        if let Some(a) = st.archives.iter().find(|a| a.id == b.archive_id) {
            full_path = format!("{}.7z", a.name);
            parts.push((full_path.clone(), Some(Some(String::new())), true));
        }
        for (name, path) in b.crumbs() {
            full_path = format!("{full_path} › {name}");
            parts.push((name, Some(Some(path)), true));
        }
    }
    if let Some(last) = parts.last_mut() {
        last.1 = None;
    }

    let avail = ui.available_width();
    let width_at = |parts: &[(String, Option<Option<String>>, bool)], size: f32| -> f32 {
        let font = |highlight: bool| egui::FontId::proportional(if highlight { size * PATH } else { FULL });
        let text: f32 = ui.fonts_mut(|f| {
            parts.iter().map(|p| f.layout_no_wrap(p.0.clone(), font(p.2), theme::ink()).size().x).sum::<f32>()
                + f.layout_no_wrap("›".into(), font(true), theme::ink()).size().x * (parts.len() - 1) as f32
        });
        text + GAP * (2 * parts.len() - 2) as f32
    };
    // Fold middle folders into "…" until the path fits at the smallest
    // size, then make it as large as fits.
    let mut folded = false;
    while width_at(&parts, SMALLEST) > avail && parts.len() > 3 {
        let at = if folded { 3 } else { 2 };
        if parts.len() <= at + 1 {
            break;
        }
        parts.remove(at);
        if !folded {
            parts.insert(2, ("…".into(), None, true));
            folded = true;
        }
    }
    // Only the path shrinks, and its width grows in step with its size.
    let (wide, narrow) = (width_at(&parts, FULL), width_at(&parts, SMALLEST));
    let size = if wide <= avail {
        FULL
    } else {
        (SMALLEST + (avail - narrow) * (FULL - SMALLEST) / (wide - narrow)).clamp(SMALLEST, FULL)
    };

    let mut go: Option<Option<String>> = None;
    // Bottom aligned, so the smaller path sits on the heading's line.
    let row = ui.fonts_mut(|f| f.row_height(&egui::FontId::proportional(FULL)));
    let layout = egui::Layout::left_to_right(egui::Align::Max);
    let resp = ui.allocate_ui_with_layout(egui::vec2(avail, row), layout, |ui| {
        ui.spacing_mut().item_spacing.x = GAP;
        for (i, (name, target, highlight)) in parts.into_iter().enumerate() {
            if i > 0 {
                ui.label(RichText::new("›").size(size * PATH).color(theme::muted()));
            }
            let (color, part_size) = if highlight { (theme::accent_text(), size * PATH) } else { (theme::ink(), FULL) };
            let text = RichText::new(name).size(part_size).strong().color(color);
            match target {
                None => {
                    ui.add(egui::Label::new(text).extend());
                }
                Some(t) => {
                    let tip = if t.is_none() { "Show all archives" } else { "Go back to this folder" };
                    let r = ui.add(egui::Label::new(text).extend().sense(egui::Sense::click()));
                    if r.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(tip).clicked() {
                        go = Some(t);
                    }
                }
            }
        }
    });
    if folded {
        resp.response.on_hover_text(full_path);
    }
    match go {
        Some(None) => st.archive_filter = None,
        Some(Some(p)) => {
            if let Some(b) = &mut st.browse {
                b.open(p);
            }
        }
        None => {}
    }
}

fn browse_view(ui: &mut egui::Ui, st: &mut State, id: i64, out: &Path, settings: &archive::Settings) {
    let Some(a) = st.archives.iter().find(|a| a.id == id).cloned() else {
        st.archive_filter = None;
        return;
    };
    let Some(mut b) = st.browse.take() else { return };
    let w = ui.available_width();
    let h = ui.available_height();
    let gap = ui.spacing().item_spacing.x;
    ui.horizontal_top(|ui| {
        ui.allocate_ui_with_layout(egui::vec2(w * 0.66 - gap, h), egui::Layout::top_down(egui::Align::Min), |ui| card(ui, |ui| b.table(ui)));
        ui.allocate_ui_with_layout(egui::vec2(w * 0.34, h), egui::Layout::top_down(egui::Align::Min), |ui| {
            card(ui, |ui| match b.selected_file().cloned() {
                Some(f) => details(ui, st, &f, out, settings),
                None => archive_info(ui, &a),
            })
        });
    });
    st.browse = Some(b);
}

/// A reference file that stands in for a duplicate: what it replaced, where
/// the original is, and following it or putting the file back.
fn reference_details(ui: &mut egui::Ui, st: &mut State, f: &FoundFile, dep: &Replaced, out: &Path, settings: &archive::Settings) {
    egui::ScrollArea::vertical().id_salt("dep-details").auto_shrink([false, true]).show(ui, |ui| reference_body(ui, st, f, dep, out, settings));
}

fn reference_body(ui: &mut egui::Ui, st: &mut State, f: &FoundFile, dep: &Replaced, out: &Path, settings: &archive::Settings) {
    let e = &dep.entry;
    let name = e.rel_path.rsplit('/').next().unwrap_or(&e.rel_path).to_string();
    ui.label(RichText::new(&name).monospace().strong().size(16.0));
    ui.label(RichText::new("Replaced by a reference to an identical original").color(theme::warn()));
    ui.add_space(4.0);
    ui.add_space(8.0);
    let idle = st.extracting.is_none() && st.putting_back.is_none();
    let there = out.join(&dep.original_archive).is_file();
    ui.horizontal_wrapped(|ui| {
        let tip = "Follows the reference: extracts the original under this file's name and checks its hash";
        if ui.add_enabled(idle && there, primary_button("Extract the original to…")).on_hover_text(tip).clicked()
            && let Some(dest) = rfd::FileDialog::new().pick_folder()
        {
            let (tx, rx) = channel();
            let (out, d) = (out.to_path_buf(), dep.clone());
            std::thread::spawn(move || {
                let _ = tx.send(archive::extract_original(&out, &d, &dest, archive::default_threads()).map_err(|e| format!("{e:#}")));
            });
            st.extracting = Some(rx);
            st.note = Some(format!("Extracting {name} from {}…", dep.original_archive));
        }
        let tip = "Rewrites this archive with the real file in place of the reference, taken from the original and checked against its hash";
        let archive_path = st.archives.iter().find(|a| a.id == f.archive_id).map(|a| PathBuf::from(&a.archive_path));
        if ui.add_enabled(idle && there && archive_path.is_some(), egui::Button::new("Put the file back")).on_hover_text(tip).clicked()
            && let Some(arch) = archive_path
        {
            let (tx, rx) = channel();
            let progress = Arc::new(archive::Progress::default());
            let (p, rel, settings) = (progress.clone(), e.rel_path.clone(), settings.clone());
            std::thread::spawn(move || {
                let _ = tx.send(archive::put_back_replaced(&arch, &[rel], &settings, &p).map_err(|e| format!("{e:#}")));
            });
            st.putting_back = Some((progress, rx));
            st.note = None;
        }
        let target = st
            .archives
            .iter()
            .find(|a| Path::new(&a.archive_path).file_name().is_some_and(|n| n.to_string_lossy() == dep.original_archive))
            .map(|a| a.id);
        if ui.add_enabled(target.is_some(), egui::Button::new("Show the original")).clicked()
            && let Some(id) = target
        {
            st.query.clear();
            st.archive_filter = Some(id);
            st.browse = None;
            st.goto = Some((id, dep.original_path.clone()));
        }
    });
    if !there {
        ui.colored_label(theme::danger(), format!("{} isn't in {}", dep.original_archive, out.display()));
    }
    if let Some((p, _)) = &st.putting_back {
        ui.label(RichText::new(p.phase.lock().unwrap().clone()).color(theme::muted()));
        ui.add(egui::ProgressBar::new(p.fraction()).desired_height(10.0).fill(theme::accent()));
    }
    if let Some(n) = &st.note {
        ui.label(RichText::new(n).color(theme::muted()));
    }
    ui.add_space(8.0);
    let original = PathBuf::from(&f.source_path).join(e.rel_path.replace('/', std::path::MAIN_SEPARATOR_STR));
    egui::Grid::new("dep-details").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
        let row = |ui: &mut egui::Ui, k: &str, v: String| {
            ui.label(RichText::new(k).color(theme::muted()));
            ui.add(egui::Label::new(RichText::new(v).monospace()).wrap());
            ui.end_row();
        };
        row(ui, "Path here", format!("{}.7z › {}", f.archive_name, e.rel_path));
        row(ui, "Reference file", dep.dep_path.clone());
        row(ui, "Original kept in", format!("{} › {}", dep.original_archive, dep.original_path));
        row(ui, "Came from", original.display().to_string());
        row(ui, "Size", human(e.size));
        row(ui, "Modified", e.modified_utc.clone().unwrap_or_default().replace('T', " ").replace('Z', " UTC"));
        row(ui, "BLAKE3", e.blake3.clone().unwrap_or_default());
        row(ui, "Replaced", dep.replaced_utc.replace('T', " ").replace('Z', " UTC"));
    });
}

fn archive_info(ui: &mut egui::Ui, a: &ArchiveSummary) {
    ui.label(RichText::new(format!("{}.7z", a.name)).monospace().strong().size(16.0));
    ui.add_space(4.0);
    egui::Grid::new("archive-info").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
        let row = |ui: &mut egui::Ui, k: &str, v: String| {
            ui.label(RichText::new(k).color(theme::muted()));
            ui.add(egui::Label::new(RichText::new(v).monospace()).wrap());
            ui.end_row();
        };
        row(ui, "Archive file", a.archive_path.clone());
        row(ui, "Original folder", a.source_path.clone());
        row(ui, "Files", a.file_count.to_string());
        row(ui, "Original size", human(a.input_bytes));
        let pct = if a.input_bytes > 0 { a.archive_bytes as f64 * 100.0 / a.input_bytes as f64 } else { 100.0 };
        row(ui, "Archive size", format!("{} ({pct:.0}% of original)", human(a.archive_bytes)));
        row(ui, "Archived", a.created_utc.replace('T', " ").replace('Z', " UTC"));
        if let Some(d) = &a.source_deleted_utc {
            row(ui, "Original deleted", d.replace('T', " ").replace('Z', " UTC"));
        }
    });
    ui.add_space(8.0);
    ui.label(RichText::new("Click a folder to open it, or a file to see its details and extract it.").color(theme::muted()));
}

fn archive_list(ui: &mut egui::Ui, st: &mut State) {
    card(ui, |ui| {
        ui.label(RichText::new(format!("{} archives", st.archives.len())).strong().size(16.0));
        ui.label(RichText::new("Click an archive to look inside it.").color(theme::muted()));
        ui.separator();
        egui::ScrollArea::vertical().auto_shrink([false, true]).show(ui, |ui| {
            egui::Grid::new("archives").num_columns(5).striped(true).spacing([18.0, 8.0]).show(ui, |ui| {
                for h in ["Archive", "Files", "Original", "Archive size", "Archived"] {
                    ui.label(RichText::new(h).color(theme::muted()));
                }
                ui.end_row();
                for a in &st.archives {
                    if ui.link(RichText::new(&a.name).monospace()).on_hover_text(&a.archive_path).clicked() {
                        st.archive_filter = Some(a.id);
                    }
                    ui.label(a.file_count.to_string());
                    ui.label(human(a.input_bytes));
                    ui.label(human(a.archive_bytes));
                    let when = a.created_utc.get(..10).unwrap_or(&a.created_utc);
                    let mut t = when.to_string();
                    if a.source_deleted_utc.is_some() {
                        t.push_str(" · source deleted");
                    }
                    ui.label(t);
                    ui.end_row();
                }
            });
        });
    });
}

fn details(ui: &mut egui::Ui, st: &mut State, f: &FoundFile, out: &Path, settings: &archive::Settings) {
    let name = f.rel_path.rsplit('/').next().unwrap_or(&f.rel_path).to_string();
    if f.rel_path.ends_with(&format!(".{DEP_EXT}")) {
        if st.dep.as_ref().map(|d| d.0) != Some(f.entry_id) {
            let dep = Catalog::open_existing(out).and_then(|c| c.dep_at(f.archive_id, &f.rel_path)).ok().flatten();
            st.dep = Some((f.entry_id, dep));
        }
        if let Some((_, Some(dep))) = st.dep.clone() {
            return reference_details(ui, st, f, &dep, out, settings);
        }
    }
    if st.refs.as_ref().map(|r| r.0) != Some(f.entry_id) {
        let archive_path = st.archives.iter().find(|a| a.id == f.archive_id).map(|a| PathBuf::from(&a.archive_path));
        let refs = archive_path
            .and_then(|p| Catalog::open_existing(out).and_then(|c| c.references_to(&p, &f.rel_path)).ok())
            .unwrap_or_default();
        st.refs = Some((f.entry_id, refs));
    }
    let refs = st.refs.as_ref().map(|r| r.1.clone()).unwrap_or_default();
    ui.label(RichText::new(&name).monospace().strong().size(16.0));
    ui.add_space(4.0);
    let original = PathBuf::from(&f.source_path).join(f.rel_path.replace('/', std::path::MAIN_SEPARATOR_STR));
    egui::Grid::new("details").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
        let row = |ui: &mut egui::Ui, k: &str, v: String| {
            ui.label(RichText::new(k).color(theme::muted()));
            ui.add(egui::Label::new(RichText::new(v).monospace()).wrap());
            ui.end_row();
        };
        row(ui, "Original path", original.display().to_string());
        row(ui, "Size", human(f.size));
        row(ui, "Modified", f.modified_utc.clone().unwrap_or_default().replace('T', " ").replace('Z', " UTC"));
        row(ui, "Archive", f.archive_name.clone() + ".7z");
        row(ui, "BLAKE3", f.blake3.clone().unwrap_or_default());
        row(ui, "SHA-256", f.sha256.clone().unwrap_or_default());
        row(ui, "Copies", if f.copies > 1 { format!("{} (see Duplicates)", f.copies) } else { "1".into() });
        if !refs.is_empty() {
            let list: Vec<String> = refs.iter().map(|(a, p)| format!("{a}.7z › {p}")).collect();
            row(ui, "Referenced by", list.join("\n"));
        }
    });
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        let archive_path = st.archives.iter().find(|a| a.id == f.archive_id).map(|a| PathBuf::from(&a.archive_path));
        let can = archive_path.as_ref().is_some_and(|p| p.is_file()) && st.extracting.is_none();
        if ui.add_enabled(can, primary_button("Extract to…")).clicked()
            && let (Some(dest), Some(arch)) = (rfd::FileDialog::new().pick_folder(), archive_path) {
                let (tx, rx) = channel();
                let rel = f.rel_path.clone();
                std::thread::spawn(move || {
                    let threads = archive::default_threads();
                    let _ = tx.send(archive::extract_file(&arch, &rel, &dest, threads).map_err(|e| format!("{e:#}")));
                });
                st.extracting = Some(rx);
                st.note = Some(format!("Extracting {name}…"));
            }
        if ui.button("Copy hash").clicked() {
            ui.ctx().copy_text(f.blake3.clone().unwrap_or_default());
        }
    });
    if let Some(n) = &st.note {
        ui.label(RichText::new(n).color(theme::muted()));
    }
}
