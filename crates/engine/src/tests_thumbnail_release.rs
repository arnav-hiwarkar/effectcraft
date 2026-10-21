//! #8: a Project panel thumbnail of a clip no layer shows releases that clip's decoder (its
//! threads and buffers) once the frame is decoded; a clip a layer shows keeps it.

use std::sync::{Arc, Mutex};

use effectcraft_project::{Footage, FootageKind, ItemId, ItemKind};
use effectcraft_render::FootageSource;
use effectcraft_time::Tick;
use serde_json::json;

use crate::Session;

/// A fake source of 4×4 frames that records the paths whose decoder it was asked to release.
struct Frames {
    released: Arc<Mutex<Vec<String>>>,
}

impl FootageSource for Frames {
    fn frame(&self, _: ItemId, _: &Footage, _: Tick) -> Option<Arc<effectcraft_raster::Image>> {
        Some(Arc::new(effectcraft_raster::Image::filled(4, 4, [0.0, 0.0, 0.0, 1.0])))
    }
    fn release_decoder(&self, path: &str) {
        self.released.lock().unwrap().push(path.to_string());
    }
}

fn session() -> (Session, Arc<Mutex<Vec<String>>>) {
    let released = Arc::new(Mutex::new(Vec::new()));
    let s = Session { footage: Arc::new(Frames { released: released.clone() }), ..Default::default() };
    (s, released)
}

fn footage(p: &str) -> Footage {
    Footage { path: p.into(), kind: FootageKind::Video, width: 4, height: 4, has_video: true, ..Default::default() }
}

/// Adds `a.mp4` and `b.mp4`, and a comp with one layer showing `a`.
fn clips(s: &mut Session) -> (ItemId, ItemId) {
    let (a, b) = s
        .edit("x", None, |proj, _| {
            let a = proj.add_item("a.mp4", Default::default(), None, ItemKind::Footage(footage("a.mp4")));
            let b = proj.add_item("b.mp4", Default::default(), None, ItemKind::Footage(footage("b.mp4")));
            Ok((a, b))
        })
        .unwrap();
    s.execute("comp.new", json!({"name": "Main", "width": 4, "height": 4, "frameRate": 30, "duration": 1})).unwrap();
    s.execute("layer.addItem", json!({"item": a.0})).unwrap();
    (a, b)
}

#[test]
fn a_thumbnail_of_an_unused_clip_releases_its_decoder() {
    let (mut s, released) = session();
    let (a, b) = clips(&mut s);
    assert!(s.thumbnail_job(b, Tick::ZERO, 64).unwrap()().is_some(), "the unused clip thumbnails");
    assert_eq!(*released.lock().unwrap(), ["b.mp4".to_string()], "its decoder was released");
    assert!(s.thumbnail_job(a, Tick::ZERO, 64).unwrap()().is_some(), "the used clip thumbnails");
    assert_eq!(*released.lock().unwrap(), ["b.mp4".to_string()], "a clip a layer shows keeps its decoder");
}

#[test]
fn a_clip_is_in_use_whether_its_comp_is_nested_or_not() {
    let (mut s, _released) = session();
    let (a, _b) = clips(&mut s);
    assert!(s.footage_path_in_use("a.mp4"));
    assert!(!s.footage_path_in_use("b.mp4"));
    // Nest Main in an outer comp: `a` is still shown.
    let inner = s.active_comp_id().unwrap();
    s.execute("comp.new", json!({"name": "Outer", "width": 4, "height": 4, "frameRate": 30, "duration": 1})).unwrap();
    s.execute("layer.addItem", json!({"item": inner.0})).unwrap();
    assert!(s.footage_path_in_use("a.mp4"), "a layer of a nested comp counts");
    assert!(s.thumbnail_job(a, Tick::ZERO, 64).unwrap()().is_some());
}
