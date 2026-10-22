//! The Media Browser asks for a thumbnail-sized frame, not for the whole frame.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use effectcraft_engine::{
    Session,
    media_browser::{Browser, Entry},
};
use effectcraft_project::{Footage, FootageKind, ItemId};
use effectcraft_render::{FootageSource, Image};
use effectcraft_time::Tick;
use effectcraft_ui_egui::{EffectcraftApp, dock::PanelKind};
use egui_kittest::Harness;

/// The path of the movie the browser lists.
const MOVIE: &str = "/files/clip.mov";

/// A browser with one movie in it.
struct OneMovieBrowser;

impl Browser for OneMovieBrowser {
    fn home(&self) -> String {
        "/Shoot".into()
    }
    fn list(&self, dir: &str, _only: bool) -> Result<Vec<Entry>, String> {
        if dir != "/Shoot" {
            return Err(format!("{dir}: no such folder"));
        }
        Ok(vec![Entry { name: "Clip.mov".into(), path: MOVIE.into(), is_dir: false, size: 10, modified: None, kind: Some("video") }])
    }
}

/// A source that makes every file a 1920 × 1080 video.
struct FakeImporter;

impl effectcraft_engine::Importer for FakeImporter {
    fn probe(&self, path: &str) -> Result<Footage, String> {
        Ok(Footage { path: path.into(), kind: FootageKind::Video, width: 1920, height: 1080, has_video: true, ..Default::default() })
    }
}

/// A source that records what the Media Browser asked it for.
struct RecordingFootage {
    /// Answer `frame_at_size` with the whole frame, as a source without scaled reads does.
    full_frame: bool,
    /// Cannot read a frame at a size at all.
    no_sized: bool,
    /// How often the whole frame was read.
    frame_reads: AtomicUsize,
    /// The sizes `frame_at_size` was asked for.
    at_size: Mutex<Vec<(u32, u32)>>,
}

impl RecordingFootage {
    fn new(full_frame: bool) -> Arc<RecordingFootage> {
        RecordingFootage::make(full_frame, false)
    }
    fn make(full_frame: bool, no_sized: bool) -> Arc<RecordingFootage> {
        Arc::new(RecordingFootage { full_frame, no_sized, frame_reads: AtomicUsize::new(0), at_size: Mutex::new(Vec::new()) })
    }
    fn frame_of(&self, width: u32, height: u32) -> Option<Arc<Image>> {
        Some(Arc::new(Image::filled(width, height, [0.25, 0.5, 0.75, 1.0])))
    }
    fn asks(&self) -> Vec<(u32, u32)> {
        self.at_size.lock().map(|a| a.clone()).unwrap_or_default()
    }
}

impl FootageSource for RecordingFootage {
    fn frame(&self, _item: ItemId, _footage: &Footage, _t: Tick) -> Option<Arc<Image>> {
        self.frame_reads.fetch_add(1, Ordering::Relaxed);
        self.frame_of(1920, 1080)
    }
    fn frame_at_size(&self, _item: ItemId, _footage: &Footage, _t: Tick, width: u32, height: u32) -> Option<Arc<Image>> {
        if let Ok(mut asks) = self.at_size.lock() {
            asks.push((width, height));
        }
        if self.no_sized {
            return None;
        }
        if self.full_frame { self.frame_of(1920, 1080) } else { self.frame_of(width, height) }
    }
}

type Thumb = Option<(egui::TextureHandle, [u32; 2])>;

fn harness(footage: Arc<RecordingFootage>) -> Harness<'static, EffectcraftApp> {
    let mut session = Session { browser: Some(Arc::new(OneMovieBrowser)), ..Default::default() };
    session.importer = Some(Arc::new(FakeImporter));
    session.footage = footage.clone() as Arc<dyn FootageSource>;
    session.prefs.general.language = "en".into();
    let mut h = Harness::builder().with_size(egui::vec2(1600.0, 1000.0)).with_step_dt(1.0 / 60.0).build_eframe(|_| {
        let mut app = EffectcraftApp::new(session);
        app.ui.start_screen = false;
        app.show_panel(PanelKind::MediaBrowser);
        app.ui.maximized = Some(PanelKind::MediaBrowser);
        app
    });
    h.run_steps(3);
    h
}

/// The thumbnail the Media Browser cached for `path` (its size in pixels).
fn thumb_size(h: &Harness<'_, EffectcraftApp>, path: &str) -> Option<[u32; 2]> {
    h.ctx.data(|d| d.get_temp::<Thumb>(egui::Id::new(("mb-thumb", path)))).and_then(|t| t).map(|(_tex, size)| size)
}

#[test]
fn thumbnail_asks_for_the_thumbnail_size_not_the_whole_frame() {
    let footage = RecordingFootage::new(false);
    let h = harness(footage.clone());
    // The frame was asked for at the thumbnail's size (1920 × 1080 stepped down by 15).
    assert_eq!(footage.asks(), [(128, 72)]);
    // The whole frame was never read.
    assert_eq!(footage.frame_reads.load(Ordering::Relaxed), 0);
    // The thumbnail is at that size, and no bigger.
    let size = thumb_size(&h, MOVIE).expect("no thumbnail");
    assert_eq!(size, [128, 72]);
    assert!(size[0].max(size[1]) <= 128);
}

#[test]
fn thumbnail_downsamples_a_source_that_returns_the_whole_frame() {
    let footage = RecordingFootage::new(true);
    let h = harness(footage.clone());
    assert_eq!(footage.asks(), [(128, 72)]);
    assert_eq!(footage.frame_reads.load(Ordering::Relaxed), 0);
    let size = thumb_size(&h, MOVIE).expect("no thumbnail");
    assert_eq!(size, [128, 72]);
    assert!(size[0].max(size[1]) <= 128);
}

#[test]
fn thumbnail_falls_back_to_the_whole_frame_when_the_source_cannot_size_it() {
    let footage = RecordingFootage::make(false, true);
    let h = harness(footage.clone());
    assert_eq!(footage.asks(), [(128, 72)]);
    // The whole frame was read instead, and still downsampled.
    assert_eq!(footage.frame_reads.load(Ordering::Relaxed), 1);
    let size = thumb_size(&h, MOVIE).expect("no thumbnail");
    assert_eq!(size, [128, 72]);
}
