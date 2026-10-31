//! Headless checks for M13.30 (GPU in the browser's job workers), run natively: a job worker
//! session whose accelerator is a device with deferred readbacks (the worker's own WebGPU
//! device) runs the Render Queue and an analysis with their frames rendered on the device in
//! passes (`offload::run_request_async`, `render::passes`), and gets what a CPU worker gets.

use std::sync::{Arc, Mutex};

use effectcraft_engine::Session;
use effectcraft_engine::effects::Buf;
use effectcraft_engine::effects::psim::ParticleSim;
use effectcraft_engine::offload::{Inbox, Offload, Post, WorkerReply, WorkerRequest};
use effectcraft_engine::project::ItemId;
use effectcraft_engine::raster::Image;
use effectcraft_engine::render::three_d::adv;
use effectcraft_engine::render::{Accelerator, AutoPick, FxStep, Renderer};
use effectcraft_engine::time::Tick;
use effectcraft_gpu::Gpu;
use serde_json::json;

/// [`Gpu`] as a worker's accelerator, with its software classification set by the test. The
/// export drops a software device (`Cx::new`) and renders on the CPU, so on a machine whose only
/// adapter is a software rasteriser (llvmpipe on CI) the deferred passes these tests check would
/// never run. `software: false` reports the device as hardware; everything else is the device's.
struct Reporting {
    gpu: Gpu,
    software: bool,
}

fn worker_gpu(gpu: &Gpu, software: bool) -> Arc<dyn Accelerator> {
    Arc::new(Reporting { gpu: gpu.clone(), software })
}

impl Accelerator for Reporting {
    fn name(&self) -> String {
        self.gpu.name()
    }
    fn available(&self) -> bool {
        self.gpu.available()
    }
    fn is_software(&self) -> bool {
        self.software
    }
    fn comp_frame(&self, r: &Renderer, comp: ItemId, t: Tick) -> Option<Image> {
        self.gpu.comp_frame(r, comp, t)
    }
    fn comp_frame_yuv420(&self, r: &Renderer, comp: ItemId, t: Tick, bg: [f32; 3]) -> Option<Vec<u8>> {
        self.gpu.comp_frame_yuv420(r, comp, t, bg)
    }
    fn supports_effect(&self, id: &str) -> bool {
        self.gpu.supports_effect(id)
    }
    fn effects(&self, chain: &[FxStep], buf: &Buf, levels: Option<f32>) -> Option<Buf> {
        self.gpu.effects(chain, buf, levels)
    }
    fn raster_3d(&self, scene: &adv::Scene) -> Option<adv::Target> {
        self.gpu.raster_3d(scene)
    }
    fn render_3d(&self, run: &adv::Prepared) -> Option<adv::Rendered> {
        self.gpu.render_3d(run)
    }
    fn particles(&self) -> Option<&dyn ParticleSim> {
        self.gpu.particles()
    }
    fn auto_pick(&self) -> Option<&AutoPick> {
        self.gpu.auto_pick()
    }
    fn frame_begin(&self) {
        self.gpu.frame_begin()
    }
    fn pass_begin(&self) {
        self.gpu.pass_begin()
    }
    fn pass_missed(&self) -> bool {
        self.gpu.pass_missed()
    }
    fn miss_gate(&self) -> Option<Arc<std::sync::atomic::AtomicBool>> {
        self.gpu.miss_gate()
    }
    fn settle(&self) -> Option<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + '_>>> {
        self.gpu.settle()
    }
}

/// Holds offloaded jobs until the test runs them.
#[derive(Default)]
struct Manual(Mutex<Vec<(WorkerRequest, Arc<Inbox>)>>);

impl Offload for Manual {
    fn start(&self, req: WorkerRequest, inbox: Arc<Inbox>) -> Result<(), String> {
        self.0.lock().unwrap().push((req, inbox));
        Ok(())
    }
    fn cancel(&self, _: u64) {}
}

type Files = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

fn exporter(files: Files) -> Arc<effectcraft_host::FileExporter> {
    let sink: Arc<dyn Fn(&str, Vec<u8>) + Send + Sync> = Arc::new(move |p: &str, d: Vec<u8>| files.lock().unwrap().push((p.to_string(), d)));
    Arc::new(effectcraft_host::FileExporter { sink: Some(sink) })
}

/// Run `req` on a worker session (`accel`: its accelerator); its replies and the files it wrote.
fn run(req: WorkerRequest, accel: Option<Arc<dyn Accelerator>>) -> (Vec<WorkerReply>, Vec<(String, Vec<u8>)>) {
    let files: Files = Arc::default();
    let mut w = Session { exporter: Some(exporter(files.clone())), ..Default::default() };
    w.accel = accel;
    let replies = Arc::new(Mutex::new(vec![]));
    let r2 = replies.clone();
    let post: Post = std::rc::Rc::new(move |r| r2.lock().unwrap().push(r));
    effectcraft_engine::offload::run_request(&mut w, req, &post);
    let r = replies.lock().unwrap().clone();
    let f = files.lock().unwrap().clone();
    (r, f)
}

fn page() -> (Session, Arc<Manual>) {
    let off = Arc::new(Manual::default());
    let mut s = Session { offload: Some(off.clone()), exporter: Some(exporter(Arc::default())), ..Default::default() };
    s.execute("comp.new", json!({"name": "Job", "width": 96, "height": 64, "frameRate": 10, "duration": 0.6})).unwrap();
    let l = s.execute("layer.newSolid", json!({"color": "#3070c0"})).unwrap()["layer"].as_u64().unwrap();
    // Moving texture (Fractal Noise, offset animated), then a blur: GPU effects.
    s.execute("effect.apply", json!({"layers": [l], "effect": "ec.noise.fractal"})).unwrap();
    s.execute("prop.addKey", json!({"layer": l, "path": "effects/#1/transform/offset", "time": 0.0, "value": [0.5, 0.5]})).unwrap();
    s.execute("prop.addKey", json!({"layer": l, "path": "effects/#1/transform/offset", "time": 0.5, "value": [0.6, 0.56]})).unwrap();
    s.execute("effect.apply", json!({"layers": [l], "effect": "ec.blur.gaussian"})).unwrap();
    s.execute("prop.set", json!({"layer": l, "path": "effects/#2/blurriness", "value": 12.0})).unwrap();
    (s, off)
}

#[test]
fn render_queue_jobs_render_on_the_worker_gpu() {
    let _gpu = crate::gpu_test::lock();
    let Some(g) = Gpu::headless_deferred() else {
        eprintln!("no GPU adapter: skipped");
        return;
    };
    let (mut s, off) = page();
    s.execute("renderQueue.add", json!({"format": "png", "output": "/job/f_[#####].png"})).unwrap();
    s.execute("renderQueue.render", json!({"wait": false})).unwrap();
    let (req, _) = off.0.lock().unwrap().pop().expect("the render went to the worker");
    let (cpu_replies, mut cpu_files) = run(req.clone(), None);
    let before = g.deferred_stats().unwrap();
    let (gpu_replies, mut gpu_files) = run(req, Some(worker_gpu(&g, false)));
    // (the CPU worker writes a batch's files in parallel)
    cpu_files.sort();
    gpu_files.sort();
    let after = g.deferred_stats().unwrap();
    let done = |r: &[WorkerReply]| r.iter().any(|r| matches!(r, WorkerReply::Item { update } if format!("{update:?}").contains("Done")));
    assert!(done(&cpu_replies) && done(&gpu_replies), "{gpu_replies:?}");
    assert!(matches!(gpu_replies.last(), Some(WorkerReply::Done)));
    // Every frame renders on the project's renderer, the GPU: in passes on the device (their
    // chains, then the frame read back).
    let frames = gpu_files.len() as u64;
    assert_eq!(frames, 6);
    assert!(after.0 - before.0 >= 6, "passes {before:?} → {after:?}");
    assert!(after.1 - before.1 >= 6, "readbacks {before:?} → {after:?}");
    assert_eq!(after.2, 0, "nothing left in flight");
    // The same frames as the CPU worker's (within GPU rounding).
    assert_eq!(cpu_files.iter().map(|f| &f.0).collect::<Vec<_>>(), gpu_files.iter().map(|f| &f.0).collect::<Vec<_>>());
    for ((name, a), (_, b)) in cpu_files.iter().zip(&gpu_files) {
        let a = image::load_from_memory(a).unwrap().to_rgba8();
        let b = image::load_from_memory(b).unwrap().to_rgba8();
        assert_eq!(a.dimensions(), b.dimensions());
        let worst = a.as_raw().iter().zip(b.as_raw()).map(|(x, y)| x.abs_diff(*y)).max().unwrap();
        assert!(worst <= 3, "{name}: differs by {worst} levels");
    }
}

/// A software device (llvmpipe, WARP, SwiftShader) is not a GPU for export: the worker's job
/// renders on the CPU, with no deferred passes or readbacks, and the same frames as a CPU worker.
#[test]
fn software_worker_gpu_falls_back_to_the_cpu() {
    let _gpu = crate::gpu_test::lock();
    let Some(g) = Gpu::headless_deferred() else {
        eprintln!("no GPU adapter: skipped");
        return;
    };
    let (mut s, off) = page();
    s.execute("renderQueue.add", json!({"format": "png", "output": "/job/f_[#####].png"})).unwrap();
    s.execute("renderQueue.render", json!({"wait": false})).unwrap();
    let (req, _) = off.0.lock().unwrap().pop().expect("the render went to the worker");
    let (cpu_replies, mut cpu_files) = run(req.clone(), None);
    let before = g.deferred_stats().unwrap();
    let (sw_replies, mut sw_files) = run(req, Some(worker_gpu(&g, true)));
    let after = g.deferred_stats().unwrap();
    cpu_files.sort();
    sw_files.sort();
    let done = |r: &[WorkerReply]| r.iter().any(|r| matches!(r, WorkerReply::Item { update } if format!("{update:?}").contains("Done")));
    assert!(done(&cpu_replies) && done(&sw_replies), "{sw_replies:?}");
    assert!(matches!(sw_replies.last(), Some(WorkerReply::Done)));
    assert_eq!(before, after, "the software device rendered nothing in passes");
    assert_eq!(cpu_files.len(), 6);
    assert_eq!(cpu_files, sw_files, "the software fallback writes what a CPU worker writes");
}

#[test]
fn analyses_render_their_input_on_the_worker_gpu() {
    let _gpu = crate::gpu_test::lock();
    let Some(g) = Gpu::headless_deferred() else {
        eprintln!("no GPU adapter: skipped");
        return;
    };
    let (mut s, off) = page();
    let l = s.active_comp().unwrap().layers[0].id;
    s.execute("layer.select", json!({"layers": [l.0]})).unwrap();
    s.execute("effect.apply", json!({"layers": [l.0], "effect": "Warp Stabilizer"})).unwrap();
    s.execute("warp.analyze", json!({"layer": l.0, "wait": false})).unwrap();
    let (req, _) = off.0.lock().unwrap().pop().expect("the analysis went to the worker");
    let (cpu, _) = run(req.clone(), None);
    let before = g.deferred_stats().unwrap();
    let (gpu, _) = run(req, Some(worker_gpu(&g, false)));
    let after = g.deferred_stats().unwrap();
    let group = |r: &[WorkerReply]| r.iter().find_map(|r| if let WorkerReply::Group { group, .. } = r { Some(group.clone()) } else { None });
    let failed = |r: &[WorkerReply]| r.iter().find_map(|r| if let WorkerReply::Failed { error } = r { Some(error.clone()) } else { None });
    assert_eq!(group(&cpu).is_some(), group(&gpu).is_some(), "cpu {:?} / gpu {:?}", failed(&cpu), failed(&gpu));
    assert!(group(&gpu).is_some(), "{:?}", failed(&gpu));
    // The input frames (the effects above Warp Stabilizer) rendered on the device in passes.
    assert!(after.1 - before.1 >= 6, "readbacks {before:?} → {after:?}");
    assert!(after.0 - before.0 >= 12, "passes {before:?} → {after:?}");
    let progress = gpu.iter().filter(|r| matches!(r, WorkerReply::Progress { .. })).count();
    assert!(progress >= 1);
}
