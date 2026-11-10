//! File ▸ Import ▸ Adobe After Effects Project… (`effectcraft_aep::import`) and File ▸ Export ▸
//! Adobe After Effects Project… (`effectcraft_aep::export`).
//!
//! Adds the items of an `.aep` (or its XML form, `.aepx`) to the project in a folder named after
//! the file (`docs/aep-import.md` lists what arrives). What the importer cannot bring over (effects
//! outside its table, undecoded text and style settings, …) is returned in the report; with losses and without
//! `allowLosses` the import is refused and changes nothing. Fonts that are not installed are listed
//! in the report's `missingFonts` (not a loss: the text keeps the font name and is drawn with the
//! fallback font).

use effectcraft_aep::export::{AepExportError, ExportOptions};
use effectcraft_aep::import::{AepImportError, ImportOptions};
use effectcraft_project::{FootageKind, ItemId, ItemKind, Project};
use serde_json::{Value, json};

use super::{CommandSpec, always, b_p, bad, str_p};
use crate::{EngineError, Result, Session, cmd};

/// Whether `path` is an After Effects project (`.aep`, or its XML form `.aepx`): imported as a
/// project (`file.importAep`), not as footage.
pub fn is_aep_path(path: &str) -> bool {
    std::path::Path::new(path).extension().and_then(std::ffi::OsStr::to_str).is_some_and(|e| e.eq_ignore_ascii_case("aep") || e.eq_ignore_ascii_case("aepx"))
}

/// Whether `bytes` are an After Effects project whatever the file is called: the `RIFX`
/// container of an `.aep`, or an XML file (`.aepx`) with the `AfterEffectsProject` root element in
/// its first 4 KiB (after the declaration and comments; bounded so a large file is not scanned). An EffectCraft project is JSON, or XML that never holds that element unescaped.
pub fn is_aep_content(bytes: &[u8]) -> bool {
    const ROOT: &[u8] = b"<AfterEffectsProject";
    let head = bytes.get(..bytes.len().min(4096)).unwrap_or_default();
    bytes.starts_with(b"RIFX") || effectcraft_aep::is_aepx(bytes) && head.windows(ROOT.len()).any(|w| w == ROOT)
}

/// The file name of `path`, as the folder of an import and the messages name the project.
fn file_name(path: &str) -> String {
    std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "After Effects Project".into())
}

/// Whether a footage path stored in the project file at `doc` is there. Paths reach the file
/// system only through the document-path policy (`doc_path`): relative ones are taken from the
/// file's folder, paths to other computers are never looked at (the footage arrives missing).
fn footage_exists(s: &Session, doc: &str) -> impl Fn(&str) -> bool + use<> {
    let services = s.services.clone();
    let doc = doc.to_string();
    move |f: &str| crate::doc_path::resolve(f, Some(&doc)).ok().and_then(|p| p.to_str().map(|p| services.exists(p))).unwrap_or(false)
}

/// The renderer's own font lookup (family, full or PostScript name); a miss falls back.
fn has_font(font: &str) -> bool {
    !effectcraft_text::resolve(font, "Regular").missing
}

/// The `created` footage of a project converted from the file at `doc`: resolved against the
/// file (a refused path arrives missing), and a video file that is there says whether it carries
/// sound. That is right when the project file's stored sample rate is not (the file changed since,
/// or the project is not After Effects's own). A refused or missing path is not read and keeps what
/// the project file says.
fn settle_footage(proj: &mut Project, created: &std::collections::HashSet<ItemId>, doc: &str, importer: Option<&std::sync::Arc<dyn crate::Importer>>) {
    crate::doc_path::resolve_footage_of(proj, |id| created.contains(&id), Some(doc));
    let Some(importer) = importer else { return };
    for item in proj.items.values_mut().filter(|i| created.contains(&i.id)) {
        if let ItemKind::Footage(f) = &mut item.kind
            && f.kind == FootageKind::Video
            && !f.missing
            && let Ok(probed) = importer.probe(&f.path)
        {
            f.has_audio = probed.has_audio;
        }
    }
}

fn import_aep(s: &mut Session, p: &Value) -> Result<Value> {
    let path = str_p(p, "path").ok_or_else(|| bad("file.importAep", "missing `path`"))?.to_string();
    let allow_losses = b_p(p, "allowLosses").unwrap_or(false);
    let dry_run = b_p(p, "dryRun").unwrap_or(false);
    let bytes = s.services.read_file(&path).map_err(|e| EngineError::Other(format!("cannot read {path}: {e}")))?;
    let exists = footage_exists(s, &path);
    let name = file_name(&path);
    // Into a project without compositions the file's settings come along (it is opened as a
    // project); into one with compositions only when asked (`applySettings`). Settings that are not
    // taken over cannot be lost, so the dry run and the refusal leave them out.
    let apply = b_p(p, "applySettings").unwrap_or_else(|| s.project.comps().next().is_none());
    let options = ImportOptions { allow_losses, exists: &exists, has_font: &has_font, folder_name: &name, settings: apply };
    if dry_run {
        let report = effectcraft_aep::import::analyze_with(&bytes, &options).map_err(|e| EngineError::Other(format!("cannot import {path}: {e}")))?;
        return Ok(json!({"dryRun": true, "report": report}));
    }
    let importer = s.importer.clone();
    let res = s.edit("Import Adobe After Effects Project", None, |proj, st| {
        let r = effectcraft_aep::import::import(proj, &bytes, &options).map_err(|e| match e {
            AepImportError::Refused(_) => EngineError::Refused {
                losses: e.to_string(),
                advice: "nothing was imported: pass `allowLosses` to import anyway, or `dryRun` for the full report",
            },
            other => EngineError::Other(format!("cannot import {path}: {other}")),
        })?;
        if apply {
            proj.settings = r.settings.clone();
        }
        let created: std::collections::HashSet<_> = r.items.iter().copied().collect();
        settle_footage(proj, &created, &path, importer.as_ref());
        st.project_selection = vec![r.folder];
        Ok(r)
    })?;
    let created: std::collections::HashSet<_> = res.items.iter().copied().collect();
    s.stored_paths.add_items(&s.project, |id| created.contains(&id), Some(&path));
    if let Some(first) = res.comps.first() {
        s.open_comp(*first);
    }
    let losses = res.report.total_losses();
    s.toast(if losses == 0 {
        format!("Imported {name}")
    } else {
        format!("Imported {name} ({losses} unsupported item{} not imported)", if losses == 1 { "" } else { "s" })
    });
    Ok(json!({
        "folder": res.folder.0,
        "comps": res.comps.iter().map(|c| c.0).collect::<Vec<_>>(),
        "items": res.items.iter().map(|i| i.0).collect::<Vec<_>>(),
        "report": res.report,
    }))
}

/// File ▸ Open of an After Effects project (`file.open` with an `.aep` / `.aepx`, `bytes` read
/// from `path`): converted into a new, untitled project with the file's settings and its items at
/// the top level, as After Effects shows a project it opens. The file is never written: Save asks
/// for an EffectCraft project, starting from the file's name and folder (`Session::opened_from`).
/// Like opening an EffectCraft project it is not an undoable step. Losses are refused unless
/// `allowLosses`; a refusal or a damaged file leaves the open project as it was.
pub(crate) fn open_aep(s: &mut Session, p: &Value, path: &str, bytes: &[u8]) -> Result<Value> {
    let allow_losses = b_p(p, "allowLosses").unwrap_or(false);
    let exists = footage_exists(s, path);
    let name = file_name(path);
    let options = ImportOptions { allow_losses, exists: &exists, has_font: &has_font, folder_name: &name, settings: true };
    if b_p(p, "dryRun").unwrap_or(false) {
        let report = effectcraft_aep::import::analyze_with(bytes, &options).map_err(|e| EngineError::Other(format!("cannot open {path}: {e}")))?;
        return Ok(json!({"dryRun": true, "report": report}));
    }
    let mut proj = Project::default();
    let r = effectcraft_aep::import::import(&mut proj, bytes, &options).map_err(|e| match e {
        AepImportError::Refused(report) => EngineError::Refused {
            losses: format!("opening would lose {}", report.summary()),
            advice: "nothing was opened: pass `allowLosses` to open anyway, or `dryRun` for the full report",
        },
        other => EngineError::Other(format!("cannot open {path}: {other}")),
    })?;
    proj.settings = r.settings.clone();
    // The importer's folder named after the file is the project itself here: its items go to the
    // top level.
    for item in proj.items.values_mut().filter(|i| i.parent == Some(r.folder)) {
        item.parent = None;
    }
    proj.items.remove(&r.folder);
    let created: std::collections::HashSet<_> = r.items.iter().copied().filter(|id| *id != r.folder).collect();
    settle_footage(&mut proj, &created, path, s.importer.clone().as_ref());
    s.replace_project(proj, None);
    s.opened_from = Some(path.to_string());
    s.stored_paths.add(&s.project, Some(path));
    if let Some(first) = r.comps.first() {
        s.open_comp(*first);
    }
    // In File ▸ Open Recent and on the Home screen, opened again the same way.
    s.note_project_path(path);
    let losses = r.report.total_losses();
    s.toast(if losses == 0 {
        format!("Opened {name} as an untitled project: save it as an EffectCraft project to keep your changes")
    } else {
        format!(
            "Opened {name} as an untitled project ({losses} unsupported item{} left out): save it as an EffectCraft project to keep your changes",
            if losses == 1 { "" } else { "s" }
        )
    });
    let missing = proj_missing_footage(s, &created);
    if missing > 0 {
        s.toast(format!("{missing} footage item{} marked missing.", if missing == 1 { "" } else { "s" }));
    }
    if let Some(font) = r.report.missing_fonts.first() {
        let n = r.report.missing_fonts.len();
        s.toast(format!(
            "{n} font{} not installed ({font}{}): the text uses a fallback font.",
            if n == 1 { " is" } else { "s are" },
            if n > 1 { ", …" } else { "" }
        ));
    }
    if s.check_footage_on_open {
        crate::footage_check::start(s, None, false)?;
    }
    Ok(json!({
        "path": path,
        "converted": true,
        "comps": r.comps.iter().map(|c| c.0).collect::<Vec<_>>(),
        "report": r.report,
    }))
}

/// How many of the `created` footage items arrived missing.
fn proj_missing_footage(s: &Session, created: &std::collections::HashSet<ItemId>) -> usize {
    s.project.items.values().filter(|i| created.contains(&i.id) && matches!(&i.kind, ItemKind::Footage(f) if f.missing)).count()
}

fn export_aep(s: &mut Session, p: &Value) -> Result<Value> {
    let path = str_p(p, "path").ok_or_else(|| bad("file.exportAep", "missing `path`"))?.to_string();
    let allow_losses = b_p(p, "allowLosses").unwrap_or(false);
    let dry_run = b_p(p, "dryRun").unwrap_or(false);
    if dry_run {
        return Ok(json!({"dryRun": true, "report": effectcraft_aep::export::analyze(&s.project)}));
    }
    // Image-sequence folders keep their stored numbering in After Effects only with their actual
    // modification time (otherwise it rescans them and warns about every gap on open). The folder
    // is looked at only when the sequence's file passes the document-path policy (a path that came
    // from a document is followed as import followed it; a refused one leaves the time unset).
    let services = s.services.clone();
    let stored = s.stored_paths.clone();
    let folder_time = move |file: &str| {
        let file = stored.follow(file).ok()?;
        let file = file.to_str()?;
        // Either separator, as the exporter splits the alias (a Windows path on another system).
        let dir = file.get(..file.rfind(['/', '\\'])?).filter(|d| !d.is_empty())?;
        services.stamp(dir).and_then(|(_, ns)| u64::try_from(ns / 1_000_000_000).ok())
    };
    let res = effectcraft_aep::export::export_with(&s.project, &ExportOptions { allow_losses }, &folder_time).map_err(|e| match e {
        AepExportError::Refused(_) => {
            EngineError::Refused { losses: e.to_string(), advice: "nothing was written: pass `allowLosses` to export anyway, or `dryRun` for the full report" }
        }
        other => EngineError::Other(format!("cannot export {path}: {other}")),
    })?;
    s.services.write_file(&path, &res.bytes).map_err(|e| EngineError::Other(format!("cannot write {path}: {e}")))?;
    let losses = res.report.total_losses();
    s.toast(if losses == 0 {
        format!("Exported After Effects project to {path}")
    } else {
        format!("Exported After Effects project to {path} ({losses} unsupported item{} not exported)", if losses == 1 { "" } else { "s" })
    });
    Ok(json!({"path": path, "bytes": res.bytes.len(), "report": res.report}))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            "file.importAep",
            "Adobe After Effects Project...",
            ["File", "Import"],
            None,
            "{path (.aep or .aepx), allowLosses?: bool (import although parts cannot be imported), dryRun?: bool (only return the report), applySettings?: bool (take over the file's project settings; default: when the project has no compositions)}",
            always,
            import_aep
        ),
        cmd!(
            "file.exportAep",
            "Adobe After Effects Project...",
            ["File", "Export"],
            None,
            "{path (.aep), allowLosses?: bool (export although parts cannot be exported), dryRun?: bool (only return the report)}",
            always,
            export_aep
        ),
    ]
}
